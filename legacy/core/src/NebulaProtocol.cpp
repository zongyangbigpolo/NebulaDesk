//
// NebulaProtocol.cpp - wire framing implementation
//
#include "NebulaProtocol.h"
#include <cstring>

namespace nebula {

std::vector<uint8_t> BuildMessage(MsgType type, uint16_t flags, uint32_t seq,
                                  uint64_t timestampUs,
                                  const uint8_t* payload, size_t payloadLen) {
    std::vector<uint8_t> buf(kHeaderSize + payloadLen);
    NebulaFrameHeader h;
    h.magic       = kNebulaMagic;
    h.version     = kNebulaVersion;
    h.type        = static_cast<uint8_t>(type);
    h.flags       = flags;
    h.length      = static_cast<uint32_t>(payloadLen);
    h.seq         = seq;
    h.timestampUs = timestampUs;
    std::memcpy(buf.data(), &h, kHeaderSize);
    if (payload && payloadLen) {
        std::memcpy(buf.data() + kHeaderSize, payload, payloadLen);
    }
    return buf;
}

bool ParseHeader(const uint8_t* data, size_t len, NebulaFrameHeader& out) {
    if (!data || len < kHeaderSize) return false;
    std::memcpy(&out, data, kHeaderSize);
    if (out.magic != kNebulaMagic) return false;
    if (out.version != kNebulaVersion) return false;
    return true;
}

// Caps payload layout (little-endian, packed):
//   videoCodec u8, width u32, height u32, fps u32, vbitrate u32,
//   audioCodec u8, sampleRate u32, channels u32, abitrate u32
std::vector<uint8_t> EncodeCaps(const NebulaCaps& c) {
    std::vector<uint8_t> b;
    auto put8  = [&](uint8_t v){ b.push_back(v); };
    auto put32 = [&](uint32_t v){ for (int i = 0; i < 4; ++i) b.push_back((v >> (i*8)) & 0xff); };

    put8(static_cast<uint8_t>(c.video.codec));
    put32(c.video.width);
    put32(c.video.height);
    put32(c.video.fps);
    put32(c.video.bitrate);
    put8(static_cast<uint8_t>(c.audio.codec));
    put32(c.audio.sampleRate);
    put32(c.audio.channels);
    put32(c.audio.bitrate);
    put8(c.video.useVirtualDisplay ? 1 : 0);
    return b;
}

bool DecodeCaps(const uint8_t* d, size_t len, NebulaCaps& out) {
    const size_t need = 1 + 4*4 + 1 + 4*3 + 1;
    if (!d || len < need) return false;
    size_t p = 0;
    auto get8  = [&]() -> uint8_t { return d[p++]; };
    auto get32 = [&]() -> uint32_t {
        uint32_t v = 0;
        for (int i = 0; i < 4; ++i) v |= static_cast<uint32_t>(d[p++]) << (i*8);
        return v;
    };
    out.video.codec   = static_cast<VideoCodec>(get8());
    out.video.width   = get32();
    out.video.height  = get32();
    out.video.fps     = get32();
    out.video.bitrate = get32();
    out.audio.codec      = static_cast<AudioCodec>(get8());
    out.audio.sampleRate = get32();
    out.audio.channels   = get32();
    out.audio.bitrate    = get32();
    out.video.useVirtualDisplay = d[p++] != 0;
    return true;
}

} // namespace nebula
