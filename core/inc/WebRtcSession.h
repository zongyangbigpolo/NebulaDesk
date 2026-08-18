//
// WebRtcSession.h - one browser/WebRTC viewer's session on the VDA (P4)
//
// Wraps libdatachannel's rtc::PeerConnection so the rest of Nebula (capture,
// encoders, input injection) never needs to know about ICE/DTLS/SRTP or
// libdatachannel types. A WebRtcSession is fed the SAME encoded H264/Opus
// frames the QUIC path already produces (pure fan-out — the native
// nebula_session/nebula_relay path is completely unaffected), and emits
// input events through the same NebulaInputEvent path as the Control
// channel does today.
//
// Signaling (SDP offer/answer + trickle ICE candidates) is transport-
// agnostic here: the caller wires setOnLocalDescription/setOnLocalCandidate
// to whatever carries them to the browser (see server/nebula_cloud's WS
// signaling endpoint) and calls setRemoteDescription/addRemoteCandidate with
// what comes back.
//
#pragma once

#include "NebulaTypes.h"
#include "NebulaFrame.h"
#include "NebulaInput.h"
#include <functional>
#include <memory>
#include <string>
#include <vector>

namespace nebula {

// Mirrors rtc::IceServer minimally so core/inc never has to include
// libdatachannel's headers.
struct IceServerConfig {
    std::string url;      // e.g. "stun:stun.l.google.com:19302" or "turn:host:port"
    std::string username; // TURN only; empty for STUN
    std::string password; // TURN only; empty for STUN
};

class WebRtcSession {
public:
    using SdpCb   = std::function<void(const std::string& sdpType, const std::string& sdp)>;
    using IceCb   = std::function<void(const std::string& candidate, const std::string& mid)>;
    using StateCb = std::function<void(bool connected)>;
    using InputCb = std::function<void(const NebulaInputEvent&)>;
    // Fired when the browser requests a keyframe (RTCP PLI/FIR) — wire this
    // to IVideoEncoder::forceKeyframe() so the very next encoded frame is an
    // IDR, letting a late-joining/recovering viewer start decoding.
    using KeyframeRequestCb = std::function<void()>;

    WebRtcSession();
    ~WebRtcSession();

    void setOnLocalDescription(SdpCb cb);
    void setOnLocalCandidate(IceCb cb);
    void setOnStateChange(StateCb cb);
    void setOnInput(InputCb cb);
    void setOnKeyframeRequest(KeyframeRequestCb cb);

    // VDA always plays the offerer role: creates video/audio tracks + an
    // input data channel, then produces a local SDP offer via setOnLocalDescription.
    bool start(const VideoConfig& video, const AudioConfig& audio,
              const std::vector<IceServerConfig>& iceServers);

    // Feed the browser's SDP answer / trickled ICE candidates.
    void setRemoteDescription(const std::string& sdpType, const std::string& sdp);
    void addRemoteCandidate(const std::string& candidate, const std::string& mid);

    // Fan-out from the existing VideoEncoder/OpusAudioEncoder output
    // callbacks. `frame.data` must be Annex-B H264 for video (matches
    // VdaServer's --h264 output) and Opus for audio.
    void sendVideoFrame(const EncodedFrame& frame);
    void sendAudioFrame(const EncodedFrame& frame);

    bool connected() const;
    void close();

private:
    struct Impl;
    std::unique_ptr<Impl> m_impl;
};

} // namespace nebula
