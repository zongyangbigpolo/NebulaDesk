//
// WebRtcGateway.h - manages the VDA side of browser/WebRTC viewers (P4)
//
// Owns one outbound WebSocket signaling connection to a Nebula Cloud-style
// signaling endpoint (see server/nebula_cloud) and one WebRtcSession per
// connected browser viewer. Unlike the QUIC/relay path (which currently
// supports only one active viewer at a time, see ROADMAP.md §3), each
// WebRtcSession has its own independent DTLS/SRTP keys — there is no shared-
// key nonce-reuse concern here — so this gateway supports genuinely
// concurrent multiple browser viewers.
//
// Signaling wire format (JSON over the WebSocket), matching
// server/nebula_cloud's WS endpoint:
//   VDA -> server (once, on connect):
//     {"type":"hello","role":"vda","deviceId":"...","token":"..."}
//   server -> VDA (a browser wants to watch):
//     {"type":"viewer-join","viewerId":"..."}
//   VDA -> server -> browser:
//     {"type":"offer","viewerId":"...","sdp":"..."}
//     {"type":"ice","viewerId":"...","candidate":"...","mid":"..."}
//   browser -> server -> VDA:
//     {"type":"answer","viewerId":"...","sdp":"..."}
//     {"type":"ice","viewerId":"...","candidate":"...","mid":"..."}
//   either -> server -> other:
//     {"type":"viewer-leave","viewerId":"..."}
//
#pragma once

#include "NebulaTypes.h"
#include "NebulaFrame.h"
#include "NebulaInput.h"
#include "WebRtcSession.h"
#include <functional>
#include <memory>
#include <string>
#include <vector>

namespace nebula {

class WebRtcGateway {
public:
    using InputCb = std::function<void(const NebulaInputEvent&)>;
    using ViewerCountCb = std::function<void(size_t count)>;

    WebRtcGateway();
    ~WebRtcGateway();

    void configure(std::vector<IceServerConfig> iceServers, const VideoConfig& video,
                  const AudioConfig& audio);
    // Any injected input from ANY connected viewer surfaces here — the VDA
    // applies it the same way as native-client Control-channel input.
    void setOnInput(InputCb cb);
    // Fires whenever the number of connected viewers changes from/to zero,
    // so VdaServer knows when to start/can choose to keep the capture
    // pipeline running.
    void setOnViewerCountChanged(ViewerCountCb cb);
    // Wired to IVideoEncoder::forceKeyframe() by the caller.
    void setOnKeyframeRequest(std::function<void()> cb);

    // Connects out to the signaling server and registers as this device.
    bool connectSignaling(const std::string& wsUrl, const std::string& deviceId,
                          const std::string& token);

    // Fan-out from the existing encoder output callbacks (H264 Annex-B video,
    // Opus audio) — called once per frame, forwarded to every open viewer.
    void pushVideoFrame(const EncodedFrame& frame);
    void pushAudioFrame(const EncodedFrame& frame);

    size_t viewerCount() const;
    void close();

private:
    struct Impl;
    std::unique_ptr<Impl> m_impl;
};

} // namespace nebula
