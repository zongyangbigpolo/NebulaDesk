//
// WebRtcSession.cpp - libdatachannel-backed PeerConnection wrapper
//
#include "WebRtcSession.h"
#include "NebulaLog.h"

#include <rtc/rtc.hpp>

#include <atomic>
#include <chrono>
#include <mutex>

#define TAG "webrtc"

namespace nebula {
namespace {

constexpr uint8_t kH264PayloadType = 96;
constexpr uint8_t kOpusPayloadType = 111;
constexpr uint32_t kVideoSsrc = 1001;
constexpr uint32_t kAudioSsrc = 1002;

rtc::IceServer MakeIceServer(const IceServerConfig& cfg) {
    // Strip the scheme (stun:/turn:/turns:) and split host:port ourselves so
    // we can use the credentialed constructor for TURN — STUN/TURN URIs
    // (RFC 7064/7065) don't carry credentials inline.
    std::string rest = cfg.url;
    auto colon = rest.find(':');
    bool isTurns = false;
    if (colon != std::string::npos) {
        std::string scheme = rest.substr(0, colon);
        isTurns = (scheme == "turns");
        rest = rest.substr(colon + 1);
    }
    std::string host = rest;
    std::string port = "3478";
    auto lastColon = rest.rfind(':');
    if (lastColon != std::string::npos) {
        host = rest.substr(0, lastColon);
        port = rest.substr(lastColon + 1);
    }
    if (!cfg.username.empty()) {
        return rtc::IceServer(host, port, cfg.username, cfg.password,
                              isTurns ? rtc::IceServer::RelayType::TurnTls
                                      : rtc::IceServer::RelayType::TurnUdp);
    }
    return rtc::IceServer(cfg.url); // STUN: the plain URL constructor is fine
}

} // namespace

struct WebRtcSession::Impl {
    std::shared_ptr<rtc::PeerConnection> pc;
    std::shared_ptr<rtc::Track> videoTrack, audioTrack;
    std::shared_ptr<rtc::RtcpSrReporter> videoSr, audioSr;
    std::shared_ptr<rtc::DataChannel> inputChannel;

    SdpCb   onLocalDescription;
    IceCb   onLocalCandidate;
    StateCb onStateChange;
    InputCb onInput;
    KeyframeRequestCb onKeyframeRequest;

    std::mutex mutex; // guards videoTrack/audioTrack/inputChannel readiness checks
    std::atomic<bool> connected{false};
};

WebRtcSession::WebRtcSession() : m_impl(std::make_unique<Impl>()) {}
WebRtcSession::~WebRtcSession() { close(); }

void WebRtcSession::setOnLocalDescription(SdpCb cb) { m_impl->onLocalDescription = std::move(cb); }
void WebRtcSession::setOnLocalCandidate(IceCb cb) { m_impl->onLocalCandidate = std::move(cb); }
void WebRtcSession::setOnStateChange(StateCb cb) { m_impl->onStateChange = std::move(cb); }
void WebRtcSession::setOnInput(InputCb cb) { m_impl->onInput = std::move(cb); }
void WebRtcSession::setOnKeyframeRequest(KeyframeRequestCb cb) { m_impl->onKeyframeRequest = std::move(cb); }

bool WebRtcSession::start(const VideoConfig& video, const AudioConfig& audio,
                          const std::vector<IceServerConfig>& iceServers) {
    rtc::Configuration config;
    for (auto& s : iceServers) {
        try {
            config.iceServers.push_back(MakeIceServer(s));
        } catch (const std::exception& e) {
            NEBULA_LOGW(TAG, "skipping malformed ICE server '%s': %s", s.url.c_str(), e.what());
        }
    }
    (void)video; (void)audio;

    m_impl->pc = std::make_shared<rtc::PeerConnection>(config);

    m_impl->pc->onStateChange([this](rtc::PeerConnection::State st) {
        bool up = (st == rtc::PeerConnection::State::Connected);
        bool wasUp = m_impl->connected.exchange(up);
        if (up != wasUp && m_impl->onStateChange) m_impl->onStateChange(up);
        NEBULA_LOGI(TAG, "peer connection state: %d", (int)st);
    });
    m_impl->pc->onLocalDescription([this](rtc::Description desc) {
        if (m_impl->onLocalDescription) m_impl->onLocalDescription(desc.typeString(), std::string(desc));
    });
    m_impl->pc->onLocalCandidate([this](rtc::Candidate cand) {
        if (m_impl->onLocalCandidate) m_impl->onLocalCandidate(cand.candidate(), cand.mid());
    });

    // --- Video track: H264 Annex-B (matches VdaServer's --h264 output) ---
    rtc::Description::Video videoDesc("video", rtc::Description::Direction::SendOnly);
    videoDesc.addH264Codec(kH264PayloadType);
    videoDesc.addSSRC(kVideoSsrc, "nebula-video");
    m_impl->videoTrack = m_impl->pc->addTrack(videoDesc);

    auto videoRtpConfig = std::make_shared<rtc::RtpPacketizationConfig>(
        kVideoSsrc, "nebula-video", kH264PayloadType, rtc::H264RtpPacketizer::ClockRate);
    auto videoPacketizer = std::make_shared<rtc::H264RtpPacketizer>(
        rtc::NalUnit::Separator::StartSequence, videoRtpConfig);
    m_impl->videoSr = std::make_shared<rtc::RtcpSrReporter>(videoRtpConfig);
    videoPacketizer->addToChain(m_impl->videoSr);
    videoPacketizer->addToChain(std::make_shared<rtc::RtcpNackResponder>());
    videoPacketizer->addToChain(std::make_shared<rtc::PliHandler>([this]() {
        NEBULA_LOGI(TAG, "keyframe requested by viewer (PLI/FIR)");
        if (m_impl->onKeyframeRequest) m_impl->onKeyframeRequest();
    }));
    m_impl->videoTrack->setMediaHandler(videoPacketizer);

    // --- Audio track: Opus ---
    rtc::Description::Audio audioDesc("audio", rtc::Description::Direction::SendOnly);
    audioDesc.addOpusCodec(kOpusPayloadType);
    audioDesc.addSSRC(kAudioSsrc, "nebula-audio");
    m_impl->audioTrack = m_impl->pc->addTrack(audioDesc);

    auto audioRtpConfig = std::make_shared<rtc::RtpPacketizationConfig>(
        kAudioSsrc, "nebula-audio", kOpusPayloadType, rtc::OpusRtpPacketizer::DefaultClockRate);
    auto audioPacketizer = std::make_shared<rtc::OpusRtpPacketizer>(audioRtpConfig);
    m_impl->audioSr = std::make_shared<rtc::RtcpSrReporter>(audioRtpConfig);
    audioPacketizer->addToChain(m_impl->audioSr);
    audioPacketizer->addToChain(std::make_shared<rtc::RtcpNackResponder>());
    m_impl->audioTrack->setMediaHandler(audioPacketizer);

    // --- Data channel: mouse/keyboard input, reusing the existing wire format ---
    m_impl->inputChannel = m_impl->pc->createDataChannel("input");
    m_impl->inputChannel->onMessage([this](rtc::message_variant data) {
        if (!std::holds_alternative<rtc::binary>(data)) return; // ignore text frames
        auto& bin = std::get<rtc::binary>(data);
        NebulaInputEvent ev;
        if (DecodeInput(reinterpret_cast<const uint8_t*>(bin.data()), bin.size(), ev)) {
            if (m_impl->onInput) m_impl->onInput(ev);
        }
    });

    m_impl->pc->setLocalDescription(); // we added tracks/datachannel -> creates an OFFER
    return true;
}

void WebRtcSession::setRemoteDescription(const std::string& sdpType, const std::string& sdp) {
    if (!m_impl->pc) return;
    try {
        m_impl->pc->setRemoteDescription(rtc::Description(sdp, sdpType));
    } catch (const std::exception& e) {
        NEBULA_LOGE(TAG, "setRemoteDescription failed: %s", e.what());
    }
}

void WebRtcSession::addRemoteCandidate(const std::string& candidate, const std::string& mid) {
    if (!m_impl->pc) return;
    try {
        m_impl->pc->addRemoteCandidate(rtc::Candidate(candidate, mid));
    } catch (const std::exception& e) {
        NEBULA_LOGW(TAG, "addRemoteCandidate failed: %s", e.what());
    }
}

void WebRtcSession::sendVideoFrame(const EncodedFrame& frame) {
    if (!m_impl->videoTrack || !m_impl->videoTrack->isOpen()) return;
    rtc::FrameInfo info(std::chrono::duration<double>(frame.timestampUs / 1'000'000.0));
    try {
        m_impl->videoTrack->sendFrame(
            reinterpret_cast<const std::byte*>(frame.data.data()), frame.data.size(), info);
    } catch (const std::exception& e) {
        NEBULA_LOGW(TAG, "sendVideoFrame failed: %s", e.what());
    }
}

void WebRtcSession::sendAudioFrame(const EncodedFrame& frame) {
    if (!m_impl->audioTrack || !m_impl->audioTrack->isOpen()) return;
    rtc::FrameInfo info(std::chrono::duration<double>(frame.timestampUs / 1'000'000.0));
    try {
        m_impl->audioTrack->sendFrame(
            reinterpret_cast<const std::byte*>(frame.data.data()), frame.data.size(), info);
    } catch (const std::exception& e) {
        NEBULA_LOGW(TAG, "sendAudioFrame failed: %s", e.what());
    }
}

bool WebRtcSession::connected() const { return m_impl->connected.load(); }

void WebRtcSession::close() {
    if (m_impl->inputChannel) { m_impl->inputChannel->close(); m_impl->inputChannel.reset(); }
    if (m_impl->videoTrack) { m_impl->videoTrack->close(); m_impl->videoTrack.reset(); }
    if (m_impl->audioTrack) { m_impl->audioTrack->close(); m_impl->audioTrack.reset(); }
    if (m_impl->pc) { m_impl->pc->close(); m_impl->pc.reset(); }
}

} // namespace nebula
