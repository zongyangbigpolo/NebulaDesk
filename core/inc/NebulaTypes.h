//
// NebulaTypes.h - shared types for Nebula (macOS CWA<->VDA over QUIC)
//
#pragma once

#include <cstdint>
#include <cstddef>
#include <vector>

namespace nebula {

enum class VideoCodec : uint8_t {
    H264 = 1,
    HEVC = 2,
};

enum class AudioCodec : uint8_t {
    AAC = 1,
};

// Encoder/decoder video configuration negotiated at HELLO time.
struct VideoConfig {
    VideoCodec codec   = VideoCodec::HEVC;
    uint32_t   width   = 1920;
    uint32_t   height  = 1080;
    uint32_t   fps     = 60;
    uint32_t   bitrate = 20'000'000; // bits/sec
    // VDA capture always targets a virtual display sized to width×height.
    bool       useVirtualDisplay = true;
};

struct AudioConfig {
    AudioCodec codec      = AudioCodec::AAC;
    uint32_t   sampleRate = 48000;
    uint32_t   channels   = 2;
    uint32_t   bitrate    = 192'000;
};

// Capabilities exchanged in the HELLO / HELLO_ACK handshake payload.
struct NebulaCaps {
    VideoConfig video;
    AudioConfig audio;
};

// An encoded media unit handed from encoder -> transport, and transport -> decoder.
struct EncodedFrame {
    std::vector<uint8_t> data;
    uint64_t timestampUs = 0;
    bool     keyframe    = false;
    bool     config      = false; // carries codec config (e.g. HEVC VPS/SPS/PPS, AAC ASC)
};

} // namespace nebula
