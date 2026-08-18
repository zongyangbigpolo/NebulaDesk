//
// VdaServer.h - VDA pipeline: capture -> encode -> QUIC
//
#pragma once

#include "NebulaTypes.h"
#include "WebRtcSession.h"
#include <cstdint>
#include <memory>
#include <string>
#include <vector>

namespace nebula {

class VdaServer {
public:
    VdaServer();
    ~VdaServer();

    void setPreSharedKey(const std::string& psk);
    // Route through a relay instead of listening directly. Call before run().
    void useRelay(const std::string& relayHost, uint16_t relayPort,
                  const std::string& deviceId, const std::string& token);
    // Opt-in browser/WebRTC path (P4, see ROADMAP.md §12) — fully independent
    // of the QUIC path above. MUST be called AFTER run() (run() sets the
    // VideoConfig/AudioConfig this reuses for the WebRTC tracks).
    // video.codec MUST be H264 (the only video codec real browsers' WebRTC
    // negotiates broadly) — callers must have already forced --h264.
    bool enableWebRtc(const std::string& signalingUrl, const std::string& deviceId,
                      const std::string& token, std::vector<IceServerConfig> iceServers);
    // Starts listening (or dials the relay); the pipeline begins on CWA HELLO
    // (QUIC path) or on the first WebRTC viewer joining, whichever is first.
    bool run(uint16_t port, const VideoConfig& video, const AudioConfig& audio);
    void stop();

private:
    struct Impl;
    std::unique_ptr<Impl> m_impl;
};

} // namespace nebula
