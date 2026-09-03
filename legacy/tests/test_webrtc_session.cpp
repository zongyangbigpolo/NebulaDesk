// Sanity test for the WebRTC bridge (P4): a VDA-side WebRtcSession must be
// able to create a local SDP offer entirely on its own (no remote peer
// needed for this — offer creation only requires ICE gathering against the
// configured STUN servers, plus the video/audio/data tracks being set up
// correctly). This is NOT a full ICE/DTLS connectivity test (that needs two
// real peers, see the manual E2E steps in server/nebula_cloud/README.md),
// but it does exercise every non-network line of WebRtcSession::start().
#include "WebRtcSession.h"
#include "NebulaTypes.h"
#include <rtc/rtc.hpp>

#include <cassert>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <mutex>
#include <string>

using namespace nebula;

int main() {
    rtc::InitLogger(rtc::LogLevel::Warning);

    std::mutex m;
    std::condition_variable cv;
    bool gotOffer = false;
    std::string sdpType, sdp;

    WebRtcSession session;
    session.setOnLocalDescription([&](const std::string& type, const std::string& s) {
        std::lock_guard<std::mutex> lk(m);
        sdpType = type;
        sdp = s;
        gotOffer = true;
        cv.notify_all();
    });

    VideoConfig video;
    video.codec = VideoCodec::H264; // WebRTC path requires H264, not HEVC
    AudioConfig audio;

    std::vector<IceServerConfig> ice = { { "stun:stun.l.google.com:19302", "", "" } };
    assert(session.start(video, audio, ice));

    {
        std::unique_lock<std::mutex> lk(m);
        // Offer creation only needs local candidate gathering to start, not
        // full STUN round-trips, so this should resolve quickly even offline.
        bool ok = cv.wait_for(lk, std::chrono::seconds(10), [&] { return gotOffer; });
        assert(ok && "did not receive a local SDP offer in time");
    }

    assert(sdpType == "offer");
    // A real SDP offer must at minimum negotiate our three m-lines.
    assert(sdp.find("m=video") != std::string::npos);
    assert(sdp.find("m=audio") != std::string::npos);
    assert(sdp.find("m=application") != std::string::npos); // the input data channel
    assert(sdp.find("H264") != std::string::npos);
    assert(sdp.find("opus") != std::string::npos);

    session.close();
    printf("webrtc session offer: ALL PASS\n");
    return 0;
}
