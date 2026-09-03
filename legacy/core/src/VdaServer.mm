//
// VdaServer.mm - orchestrates the VDA streaming pipeline
//
#include "VdaServer.h"
#include "Transport.h"
#include "NebulaProtocol.h"
#include "NebulaCrypto.h"
#include "FrameReader.h"
#include "NebulaLog.h"
#include "ScreenCapture.h"
#include "VideoEncoder.h"
#include "AudioEncoder.h"
#include "OpusAudioEncoder.h"
#include "WebRtcGateway.h"
#include "InputInjector.h"
#include "NebulaInput.h"
#include "SessionStatus.h"

#include <atomic>
#include <thread>

#define TAG "vda"

namespace nebula {

namespace {
constexpr uint8_t kChControl = (uint8_t)ITransport::Channel::Control;
constexpr uint8_t kChVideo   = (uint8_t)ITransport::Channel::Video;
constexpr uint8_t kChAudio   = (uint8_t)ITransport::Channel::Audio;
} // namespace

struct VdaServer::Impl {
    std::unique_ptr<ITransport> transport = CreateTransport();
    std::unique_ptr<IScreenCapture> capture   = CreateScreenCapture();
    std::unique_ptr<IVideoEncoder>  venc      = CreateVideoEncoder();
    std::unique_ptr<IAudioEncoder>  aenc      = CreateAudioEncoder();
    std::unique_ptr<IOpusAudioEncoder> opusEnc; // WebRTC path only, lazily created
    std::unique_ptr<WebRtcGateway>  webrtc;     // WebRTC path only, lazily created
    std::unique_ptr<IInputInjector> injector  = CreateInputInjector();
    std::unique_ptr<FrameReader>    controlReader;

    bool useRelay = false;
    std::string relayHost, deviceId, token;
    std::string sharedSecret;
    uint16_t relayPort = 7100;

    VideoConfig video;
    AudioConfig audio;
    std::atomic<uint32_t> videoSeq{0};
    std::atomic<uint32_t> audioSeq{0};
    std::atomic<uint32_t> ctrlSeq{0};
    std::atomic<bool> pipelineStarted{false};

    // Application-layer E2E encryption (see NebulaCrypto.h). Established by
    // the KeyInit/KeyInitAck handshake before any Hello/Video/Audio/Input.
    SessionCrypto crypto;

    void sendControl(MsgType type, uint16_t flags, const uint8_t* plaintext, size_t len) {
        auto msg = BuildEncryptedMessage(crypto, kChControl, /*senderIsVda=*/true,
                                         type, flags, ctrlSeq++, 0, plaintext, len);
        transport->send(ITransport::Channel::Control, msg.data(), msg.size());
    }

    void startPipeline() {
        bool expected = false;
        if (!pipelineStarted.compare_exchange_strong(expected, true)) return;
        NEBULA_LOGI(TAG, "starting mandatory virtual-display capture pipeline");

        venc->setOutput([this](const EncodedFrame& f) {
            uint16_t flags = Flag_None;
            if (f.keyframe) flags |= Flag_Keyframe;
            if (f.config)   flags |= Flag_Config;
            auto msg = BuildEncryptedMessage(crypto, kChVideo, /*senderIsVda=*/true,
                                             MsgType::Video, flags, videoSeq++, f.timestampUs,
                                             f.data.data(), f.data.size());
            transport->send(ITransport::Channel::Video, msg.data(), msg.size());
            if (webrtc) webrtc->pushVideoFrame(f); // fan-out; independent of the QUIC path
        });
        aenc->setOutput([this](const EncodedFrame& f) {
            uint16_t flags = f.config ? Flag_Config : Flag_None;
            auto msg = BuildEncryptedMessage(crypto, kChAudio, /*senderIsVda=*/true,
                                             MsgType::Audio, flags, audioSeq++, f.timestampUs,
                                             f.data.data(), f.data.size());
            transport->send(ITransport::Channel::Audio, msg.data(), msg.size());
        });
        if (opusEnc) {
            opusEnc->setOutput([this](const EncodedFrame& f) {
                if (webrtc) webrtc->pushAudioFrame(f);
            });
        }

        capture->setVideoOutput([this](const RawVideoFrame& f) {
            venc->encode(f);
        });
        capture->setAudioOutput([this](const RawAudioFrame& f) {
            aenc->encode(f);
            if (opusEnc) opusEnc->encode(f); // portable path: reads frame.samples, not nativeHandle
        });

        // ScreenCaptureKit setup performs blocking waits; run it off the QUIC
        // serial queue so transport I/O (and keepalives) keep flowing.
        std::thread([this] {
            if (!venc->start(video)) {
                NEBULA_LOGE(TAG, "video encoder failed to start");
                ReportSessionStatus(SessionState::Error);
                return;
            }
            if (!aenc->start(audio))
                NEBULA_LOGW(TAG, "audio encoder failed to start; continuing without audio");
            if (opusEnc && !opusEnc->start(audio))
                NEBULA_LOGW(TAG, "opus encoder failed to start; WebRTC viewers get video-only");
            if (!capture->start(video, audio)) {
                NEBULA_LOGE(TAG, "mandatory virtual display capture failed");
                ReportSessionStatus(SessionState::Error);
                return;
            }
            injector->setTargetDisplay(capture->displayId());
            // A viewer's HELLO is what triggered this pipeline start, so by
            // the time capture is actually up there's an active viewer —
            // mirrors nebula_session's "Streaming" (first frame) semantics
            // closely enough for the manager's status display.
            ReportSessionStatus(SessionState::Streaming);
        }).detach();
    }

    void onControl(const NebulaFrameHeader& h, const uint8_t* payload, size_t len) {
        // KeyInit is the ONLY message ever sent in cleartext: it carries the
        // CWA's random salt so both sides can derive a fresh session key. A
        // new KeyInit always (re)starts the crypto session — this is what
        // lets a fresh CWA connection re-key cleanly (e.g. after a relay
        // hand-off to a new viewer), rather than requiring a VDA restart.
        if (h.type == (uint8_t)MsgType::KeyInit) {
            if (len < kSessionSaltLen) {
                NEBULA_LOGE(TAG, "malformed KeyInit (short salt)");
                return;
            }
            uint8_t vdaSalt[kSessionSaltLen];
            SecureRandomBytes(vdaSalt, sizeof(vdaSalt));
            crypto.deriveKey(sharedSecret, /*saltA=CWA*/ payload, /*saltB=VDA*/ vdaSalt);
            ctrlSeq = 0;
            auto msg = BuildMessage(MsgType::KeyInitAck, Flag_None, 0, 0, vdaSalt, sizeof(vdaSalt));
            transport->send(ITransport::Channel::Control, msg.data(), msg.size());
            NEBULA_LOGI(TAG, "session key established (KeyInit -> KeyInitAck)");
            return;
        }

        if (!crypto.ready()) {
            NEBULA_LOGW(TAG, "dropping control message type=%u before KeyInit handshake", h.type);
            return;
        }
        std::vector<uint8_t> plain;
        if (!OpenEncryptedMessage(crypto, kChControl, /*senderIsVda=*/false, h, payload, len, plain)) {
            NEBULA_LOGE(TAG, "control message failed to decrypt (bad PSK, tampering, or replay) — dropping");
            return;
        }

        if (h.type == (uint8_t)MsgType::Input) {
            NebulaInputEvent ev;
            if (DecodeInput(plain.data(), plain.size(), ev)) injector->inject(ev);
            return;
        }
        if (h.type == (uint8_t)MsgType::Hello) {
            NebulaCaps caps;
            if (!DecodeCaps(plain.data(), plain.size(), caps) ||
                !caps.video.width || !caps.video.height) {
                NEBULA_LOGE(TAG, "rejecting HELLO without valid screen dimensions");
                sendControl(MsgType::Bye, Flag_None, nullptr, 0);
                return;
            }
            // Only apply the CWA's requested geometry if the shared capture
            // pipeline hasn't already been started (e.g. by an earlier
            // WebRTC viewer) — one pipeline, one virtual display, first
            // client to actually start it wins the resolution.
            if (!pipelineStarted.load()) {
                video.useVirtualDisplay = true;
                video.width  = caps.video.width;
                video.height = caps.video.height;
            }
            NEBULA_LOGI(TAG, "viewer requires virtual display %ux%u", video.width, video.height);
            NebulaCaps reply{video, audio};
            auto encoded = EncodeCaps(reply);
            sendControl(MsgType::HelloAck, Flag_None, encoded.data(), encoded.size());
            NEBULA_LOGI(TAG, "HELLO received -> HELLO_ACK sent (%ux%u virtual)",
                       video.width, video.height);
            startPipeline();
        }
    }
};

VdaServer::VdaServer() : m_impl(std::make_unique<Impl>()) {}
VdaServer::~VdaServer() { stop(); }

void VdaServer::setPreSharedKey(const std::string& psk) {
    m_impl->sharedSecret = psk;
    m_impl->transport->setSharedSecret(psk);
}

void VdaServer::useRelay(const std::string& relayHost, uint16_t relayPort,
                         const std::string& deviceId, const std::string& token) {
    m_impl->useRelay = true;
    m_impl->relayHost = relayHost;
    m_impl->relayPort = relayPort;
    m_impl->deviceId = deviceId;
    m_impl->token = token;
    m_impl->transport = CreateTransport(TransportType::Relay);
    m_impl->transport->setSharedSecret(m_impl->sharedSecret);
    m_impl->transport->configureRelay(relayHost, relayPort, deviceId, token, /*isVda=*/true);
}

bool VdaServer::enableWebRtc(const std::string& signalingUrl, const std::string& deviceId,
                             const std::string& token, std::vector<IceServerConfig> iceServers) {
    if (m_impl->video.codec != VideoCodec::H264) {
        NEBULA_LOGE(TAG, "WebRTC requires H264 (video.codec must be H264, not HEVC) — refusing to enable");
        return false;
    }
    m_impl->opusEnc = CreateOpusAudioEncoder();
    m_impl->webrtc = std::make_unique<WebRtcGateway>();
    m_impl->webrtc->configure(iceServers, m_impl->video, m_impl->audio);
    m_impl->webrtc->setOnInput([this](const NebulaInputEvent& ev) {
        m_impl->injector->inject(ev);
    });
    m_impl->webrtc->setOnKeyframeRequest([this]() {
        m_impl->venc->forceKeyframe();
    });
    m_impl->webrtc->setOnViewerCountChanged([this](size_t n) {
        NEBULA_LOGI(TAG, "WebRTC viewer count: %zu", n);
        if (n > 0) m_impl->startPipeline(); // idempotent; QUIC HELLO may have already started it
    });
    return m_impl->webrtc->connectSignaling(signalingUrl, deviceId, token);
}

bool VdaServer::run(uint16_t port, const VideoConfig& video, const AudioConfig& audio) {
    m_impl->video = video;
    m_impl->audio = audio;
    ReportSessionStatus(SessionState::Connecting);
    m_impl->controlReader = std::make_unique<FrameReader>(
        [this](const NebulaFrameHeader& h, const uint8_t* p, size_t l) {
            m_impl->onControl(h, p, l);
        });

    m_impl->transport->setOnReceive(
        [this](ITransport::Channel ch, const uint8_t* data, size_t len) {
            if (ch == ITransport::Channel::Control)
                m_impl->controlReader->feed(data, len);
        });
    m_impl->transport->setOnState([this](bool connected) {
        NEBULA_LOGI(TAG, "transport %s", connected ? "connected" : "disconnected");
        // "Connected" here means registered with the relay (or listening
        // directly) and waiting for a viewer — startPipeline()'s Streaming
        // report above supersedes it once one actually shows up.
        ReportSessionStatus(connected ? SessionState::Connected : SessionState::Disconnected);
    });

    // Relay mode dials the relay; direct mode listens on the port.
    if (m_impl->useRelay) return m_impl->transport->startListener(port);
    return m_impl->transport->startListener(port);
}

void VdaServer::stop() {
    if (!m_impl) return;
    if (m_impl->webrtc) m_impl->webrtc->close();
    if (m_impl->opusEnc) m_impl->opusEnc->stop();
    if (m_impl->capture) m_impl->capture->stop();
    if (m_impl->venc) m_impl->venc->stop();
    if (m_impl->aenc) m_impl->aenc->stop();
    if (m_impl->transport) m_impl->transport->close();
}

} // namespace nebula
