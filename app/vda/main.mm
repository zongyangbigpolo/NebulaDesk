//
// main.mm - VDA server entry point
//
#include "VdaServer.h"
#include "NebulaLog.h"

#import <Foundation/Foundation.h>
#include <cstdlib>
#include <string>
#include <vector>

int main(int argc, const char* argv[]) {
    @autoreleasepool {
        nebula::LogInit("com.nebula.vda");

        uint16_t port = 7000;
        std::string psk = "nebula-default-psk";
        std::string relayHost, deviceId, token;
        uint16_t relayPort = 7100;
        nebula::VideoConfig video;
        nebula::AudioConfig audio;

        bool webrtc = false;
        std::string webrtcSignalingUrl;
        uint32_t webrtcWidth = 1920, webrtcHeight = 1080;
        std::vector<nebula::IceServerConfig> iceServers;

        for (int i = 1; i < argc; ++i) {
            std::string a = argv[i];
            auto next = [&](const char* def) { return (i + 1 < argc) ? argv[++i] : def; };
            if (a == "--port")    port = (uint16_t)atoi(next("7000"));
            else if (a == "--psk")     psk = next("");
            else if (a == "--fps")     video.fps    = atoi(next("60"));
            else if (a == "--bitrate") video.bitrate = atoi(next("20000000"));
            else if (a == "--h264")    video.codec = nebula::VideoCodec::H264;
            else if (a == "--relay")   relayHost = next("");          // relay host
            else if (a == "--relay-port") relayPort = (uint16_t)atoi(next("7100"));
            else if (a == "--device")  deviceId = next("");           // device-id (relay + WebRTC signaling)
            else if (a == "--token")   token = next("");              // pairing/auth token (relay + WebRTC signaling)
            // --- WebRTC / browser interop (P4, see ROADMAP.md §12) ---
            else if (a == "--webrtc")  webrtc = true;
            else if (a == "--webrtc-signaling-url") webrtcSignalingUrl = next("");
            else if (a == "--webrtc-width")  webrtcWidth  = (uint32_t)atoi(next("1920"));
            else if (a == "--webrtc-height") webrtcHeight = (uint32_t)atoi(next("1080"));
            else if (a == "--stun")    iceServers.push_back({std::string("stun:") + next(""), "", ""});
            else if (a == "--turn") {
                std::string host = next("");
                std::string user = (i + 1 < argc) ? argv[++i] : "";
                std::string pass = (i + 1 < argc) ? argv[++i] : "";
                iceServers.push_back({std::string("turn:") + host, user, pass});
            }
        }

        if (webrtc && video.codec != nebula::VideoCodec::H264) {
            NEBULA_LOGI("vda", "--webrtc requires H264; overriding codec (HEVC has no broad WebRTC browser support)");
            video.codec = nebula::VideoCodec::H264;
        }
        if (webrtc && webrtcSignalingUrl.empty()) {
            NEBULA_LOGE("vda", "--webrtc requires --webrtc-signaling-url");
            return 1;
        }
        if (webrtc && iceServers.empty()) {
            NEBULA_LOGI("vda", "no --stun/--turn given; defaulting to a public STUN server (LAN/cone NAT only)");
            iceServers.push_back({"stun:stun.l.google.com:19302", "", ""});
        }

        // Width and height are normally supplied by the CWA HELLO before
        // capture starts; when WebRTC-only (no native CWA expected), the
        // virtual display instead uses the fixed --webrtc-width/-height,
        // since one shared capture pipeline serves every connected viewer
        // (native or browser) and can only have one resolution at a time.
        video.width = webrtc ? webrtcWidth : 0;
        video.height = webrtc ? webrtcHeight : 0;
        video.useVirtualDisplay = true;

        nebula::VdaServer server;
        server.setPreSharedKey(psk);
        if (!relayHost.empty() && !deviceId.empty()) {
            NEBULA_LOGI("vda", "VDA via relay %s:%u as device '%s'", relayHost.c_str(), relayPort, deviceId.c_str());
            server.useRelay(relayHost, relayPort, deviceId, token);
        } else {
            NEBULA_LOGI("vda", "Nebula VDA starting on port %u", port);
        }
        if (!server.run(port, video, audio)) {
            NEBULA_LOGE("vda", "failed to start server");
            return 1;
        }
        if (webrtc) {
            NEBULA_LOGI("vda", "enabling WebRTC gateway: signaling=%s device=%s virtual=%ux%u",
                       webrtcSignalingUrl.c_str(), deviceId.c_str(), webrtcWidth, webrtcHeight);
            if (!server.enableWebRtc(webrtcSignalingUrl, deviceId, token, iceServers)) {
                NEBULA_LOGE("vda", "failed to enable WebRTC gateway");
            }
        }

        // Run the main loop so Network.framework / ScreenCaptureKit callbacks fire.
        [[NSRunLoop currentRunLoop] run];
    }
    return 0;
}
