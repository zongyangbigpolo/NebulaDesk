//
// CwaClient.mm - orchestrates the CWA receive/decode/render pipeline
//
#include "CwaClient.h"
#include "Transport.h"
#include "Signaling.h"
#include "NebulaProtocol.h"
#include "NebulaCrypto.h"
#include "NebulaInput.h"
#include "FrameReader.h"
#include "NebulaLog.h"
#include "VideoDecoder.h"
#include "AudioDecoder.h"

#include <atomic>

#define TAG "cwa"

namespace nebula {

namespace {
constexpr uint8_t kChControl = (uint8_t)ITransport::Channel::Control;
constexpr uint8_t kChVideo   = (uint8_t)ITransport::Channel::Video;
constexpr uint8_t kChAudio   = (uint8_t)ITransport::Channel::Audio;
} // namespace

struct CwaClient::Impl {
    std::unique_ptr<ITransport> transport = CreateTransport();
    std::unique_ptr<ISignaling>     signaling;
    std::unique_ptr<IVideoDecoder>  vdec      = CreateVideoDecoder();
    std::unique_ptr<IAudioDecoder>  adec      = CreateAudioDecoder();
    std::unique_ptr<FrameReader>    controlReader;
    std::unique_ptr<FrameReader>    videoReader;
    std::unique_ptr<FrameReader>    audioReader;

    bool useRelay = false;
    std::string relayHost, deviceId, token;
    std::string sharedSecret;
    uint16_t relayPort = 7100;

    NebulaCaps requested;
    NebulaCaps negotiated;
    std::atomic<bool> started{false};
    std::atomic<uint32_t> ctrlSeq{0};

    // Application-layer E2E encryption (see NebulaCrypto.h).
    SessionCrypto crypto;
    uint8_t cwaSalt[kSessionSaltLen] = {};

    VideoFrameCb videoCb;
    AudioPcmCb   audioCb;
    ReadyCb      readyCb;

    void sendControl(MsgType type, uint16_t flags, const uint8_t* plaintext, size_t len) {
        auto msg = BuildEncryptedMessage(crypto, kChControl, /*senderIsVda=*/false,
                                         type, flags, ctrlSeq++, 0, plaintext, len);
        transport->send(ITransport::Channel::Control, msg.data(), msg.size());
    }

    // First message ever sent: a cleartext random salt that lets the VDA
    // derive the same session key we will once it replies with its own salt.
    void sendKeyInit() {
        SecureRandomBytes(cwaSalt, sizeof(cwaSalt));
        auto msg = BuildMessage(MsgType::KeyInit, Flag_None, 0, 0, cwaSalt, sizeof(cwaSalt));
        transport->send(ITransport::Channel::Control, msg.data(), msg.size());
        NEBULA_LOGI(TAG, "KeyInit sent");
    }

    void onControl(const NebulaFrameHeader& h, const uint8_t* p, size_t l) {
        if (h.type == (uint8_t)MsgType::KeyInitAck) {
            if (l < kSessionSaltLen) {
                NEBULA_LOGE(TAG, "malformed KeyInitAck (short salt)");
                return;
            }
            crypto.deriveKey(sharedSecret, cwaSalt, /*saltB=VDA*/ p);
            ctrlSeq = 0;
            NEBULA_LOGI(TAG, "session key established (KeyInitAck received)");
            sendHello();
            return;
        }

        if (!crypto.ready()) {
            NEBULA_LOGW(TAG, "dropping control message type=%u before KeyInit handshake", h.type);
            return;
        }
        std::vector<uint8_t> plain;
        if (!OpenEncryptedMessage(crypto, kChControl, /*senderIsVda=*/true, h, p, l, plain)) {
            NEBULA_LOGE(TAG, "control message failed to decrypt (bad PSK, tampering, or replay) — dropping");
            return;
        }

        if (h.type == (uint8_t)MsgType::Bye) {
            NEBULA_LOGW(TAG, "VDA rejected our HELLO (BYE received)");
            return;
        }
        if (h.type == (uint8_t)MsgType::HelloAck) {
            DecodeCaps(plain.data(), plain.size(), negotiated);
            NEBULA_LOGI(TAG, "HELLO_ACK: %ux%u @%ufps", negotiated.video.width,
                      negotiated.video.height, negotiated.video.fps);
            vdec->start(negotiated.video);
            adec->start(negotiated.audio);
            vdec->setOutput([this](const RawVideoFrame& f) {
                if (videoCb) videoCb(f);
            });
            adec->setOutput([this](const RawAudioFrame& f) {
                if (audioCb) audioCb(f);
            });
            if (readyCb) readyCb(negotiated);
            started = true;
        }
    }

    void onVideo(const NebulaFrameHeader& h, const uint8_t* p, size_t l) {
        if (!crypto.ready()) return;
        std::vector<uint8_t> plain;
        if (!OpenEncryptedMessage(crypto, kChVideo, /*senderIsVda=*/true, h, p, l, plain)) {
            NEBULA_LOGW(TAG, "dropping video frame that failed to decrypt/verify");
            return;
        }
        if (h.flags & Flag_Config) { vdec->setConfig(plain.data(), plain.size()); return; }
        vdec->decode(plain.data(), plain.size(), (h.flags & Flag_Keyframe) != 0, h.timestampUs);
    }

    void onAudio(const NebulaFrameHeader& h, const uint8_t* p, size_t l) {
        if (!crypto.ready()) return;
        std::vector<uint8_t> plain;
        if (!OpenEncryptedMessage(crypto, kChAudio, /*senderIsVda=*/true, h, p, l, plain)) {
            NEBULA_LOGW(TAG, "dropping audio frame that failed to decrypt/verify");
            return;
        }
        if (h.flags & Flag_Config) { adec->setMagicCookie(plain.data(), plain.size()); return; }
        adec->decode(plain.data(), plain.size());
    }

    void sendHello() {
        auto caps = EncodeCaps(requested);
        sendControl(MsgType::Hello, Flag_None, caps.data(), caps.size());
        NEBULA_LOGI(TAG, "HELLO sent");
    }

    void sendInput(const NebulaInputEvent& e) {
        auto payload = EncodeInput(e);
        sendControl(MsgType::Input, Flag_None, payload.data(), payload.size());
    }
};

CwaClient::CwaClient() : m_impl(std::make_unique<Impl>()) {}
CwaClient::~CwaClient() { stop(); }

void CwaClient::setPreSharedKey(const std::string& psk) {
    m_impl->sharedSecret = psk;
    m_impl->transport->setSharedSecret(psk);
}

void CwaClient::useRelay(const std::string& relayHost, uint16_t relayPort,
                         const std::string& deviceId, const std::string& token,
                         const std::string& ticket) {
    m_impl->useRelay = true;
    m_impl->relayHost = relayHost;
    m_impl->relayPort = relayPort;
    m_impl->deviceId = deviceId;
    m_impl->token = token;
    m_impl->transport = CreateTransport(TransportType::Relay);
    m_impl->transport->setSharedSecret(m_impl->sharedSecret);
    m_impl->transport->configureRelay(relayHost, relayPort, deviceId, token, /*isVda=*/false, ticket);
}

std::string CwaClient::relayTicket() const { return m_impl->transport->relayTicket(); }

void CwaClient::setVideoFrameCallback(VideoFrameCb cb) { m_impl->videoCb = std::move(cb); }
void CwaClient::setAudioPcmCallback(AudioPcmCb cb) { m_impl->audioCb = std::move(cb); }
void CwaClient::setReadyCallback(ReadyCb cb) { m_impl->readyCb = std::move(cb); }
void CwaClient::sendInput(const NebulaInputEvent& e) { m_impl->sendInput(e); }

bool CwaClient::connect(const std::string& host, uint16_t port, const NebulaCaps& requested) {
    if (!requested.video.width || !requested.video.height) {
        NEBULA_LOGE(TAG, "HELLO requires local screen dimensions");
        return false;
    }
    m_impl->requested = requested;
    m_impl->controlReader = std::make_unique<FrameReader>(
        [this](const NebulaFrameHeader& h, const uint8_t* p, size_t l) { m_impl->onControl(h, p, l); });
    m_impl->videoReader = std::make_unique<FrameReader>(
        [this](const NebulaFrameHeader& h, const uint8_t* p, size_t l) { m_impl->onVideo(h, p, l); });
    m_impl->audioReader = std::make_unique<FrameReader>(
        [this](const NebulaFrameHeader& h, const uint8_t* p, size_t l) { m_impl->onAudio(h, p, l); });

    m_impl->transport->setOnReceive(
        [this](ITransport::Channel ch, const uint8_t* data, size_t len) {
            switch (ch) {
                case ITransport::Channel::Control: m_impl->controlReader->feed(data, len); break;
                case ITransport::Channel::Video:   m_impl->videoReader->feed(data, len); break;
                case ITransport::Channel::Audio:   m_impl->audioReader->feed(data, len); break;
                case ITransport::Channel::Upgrade: break; // transport-internal
            }
        });

    if (m_impl->useRelay) {
        // Relay mode: the crypto handshake must wait until the relay reports
        // the bridge is up.
        m_impl->transport->setOnState([this](bool connected) {
            NEBULA_LOGI(TAG, "transport %s", connected ? "connected (relay bridged)" : "disconnected");
            if (connected) m_impl->sendKeyInit();
        });
        return m_impl->transport->connect(m_impl->relayHost, m_impl->relayPort);
    }

    m_impl->transport->setOnState([this](bool connected) {
        NEBULA_LOGI(TAG, "transport %s", connected ? "connected" : "disconnected");
    });

    // Direct: resolve the peer via signaling, then dial. A future ICE backend
    // would gather candidates and negotiate before invoking this callback.
    bool dialed = false;
    m_impl->signaling = CreateDirectSignaling(PeerDescriptor{host, port, {}});
    m_impl->signaling->start(SignalingRole::Offerer,
        [this, &dialed](const PeerDescriptor& remote) {
            if (m_impl->transport->connect(remote.host, remote.port)) {
                // QUIC completes its handshake once the first stream is opened,
                // so kick off the crypto handshake immediately (opens the
                // control stream, drives 1-RTT). Hello follows once KeyInitAck
                // establishes the session key.
                m_impl->sendKeyInit();
                dialed = true;
            }
        });
    return dialed;
}

void CwaClient::stop() {
    if (!m_impl) return;
    if (m_impl->vdec) m_impl->vdec->stop();
    if (m_impl->adec) m_impl->adec->stop();
    if (m_impl->transport) m_impl->transport->close();
}

} // namespace nebula
