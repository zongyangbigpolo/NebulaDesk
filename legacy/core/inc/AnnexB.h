//
// AnnexB.h - AVCC <-> Annex-B NAL conversion (platform-neutral, header-only)
//
// The wire format for H.264/HEVC frame payloads is standardized on **Annex-B**
// (NAL units delimited by 0x00000001 start codes) so any platform's decoder can
// consume the stream. Apple's VideoToolbox uses AVCC (length-prefixed) instead,
// so the macOS backend converts at the edge with these helpers. Windows
// MediaFoundation already produces Annex-B and needs no conversion.
//
#pragma once

#include <cstdint>
#include <cstddef>
#include <vector>

namespace nebula {

// Convert an AVCC elementary stream (each NAL prefixed by a big-endian length of
// `nalLengthSize` bytes) into Annex-B (each NAL prefixed by 0x00000001).
inline std::vector<uint8_t> AvccToAnnexB(const uint8_t* data, size_t len,
                                         int nalLengthSize = 4) {
    std::vector<uint8_t> out;
    out.reserve(len + 16);
    size_t i = 0;
    while (i + (size_t)nalLengthSize <= len) {
        uint32_t nalLen = 0;
        for (int b = 0; b < nalLengthSize; ++b) nalLen = (nalLen << 8) | data[i + b];
        i += nalLengthSize;
        if (nalLen == 0 || i + nalLen > len) break;
        out.push_back(0); out.push_back(0); out.push_back(0); out.push_back(1);
        out.insert(out.end(), data + i, data + i + nalLen);
        i += nalLen;
    }
    return out;
}

// Split an Annex-B stream into its NAL units (start codes of length 3 or 4 both
// accepted). Returns pointers into the original buffer.
inline void SplitAnnexB(const uint8_t* data, size_t len,
                        std::vector<std::pair<const uint8_t*, size_t>>& nals) {
    size_t i = 0;
    auto isStart = [&](size_t p, int& scLen) -> bool {
        if (p + 3 <= len && data[p] == 0 && data[p+1] == 0 && data[p+2] == 1) { scLen = 3; return true; }
        if (p + 4 <= len && data[p] == 0 && data[p+1] == 0 && data[p+2] == 0 && data[p+3] == 1) { scLen = 4; return true; }
        return false;
    };
    int sc = 0;
    // Find first start code.
    while (i < len && !isStart(i, sc)) ++i;
    while (i < len) {
        i += sc; // skip start code
        size_t nalStart = i;
        int sc2 = 0;
        while (i < len && !isStart(i, sc2)) ++i;
        if (i > nalStart) nals.emplace_back(data + nalStart, i - nalStart);
        sc = sc2;
    }
}

// Convert an Annex-B stream into AVCC with 4-byte big-endian length prefixes
// (what VideoToolbox's decoder expects).
inline std::vector<uint8_t> AnnexBToAvcc(const uint8_t* data, size_t len) {
    std::vector<std::pair<const uint8_t*, size_t>> nals;
    SplitAnnexB(data, len, nals);
    std::vector<uint8_t> out;
    for (auto& n : nals) {
        uint32_t l = (uint32_t)n.second;
        out.push_back((l >> 24) & 0xff);
        out.push_back((l >> 16) & 0xff);
        out.push_back((l >> 8) & 0xff);
        out.push_back(l & 0xff);
        out.insert(out.end(), n.first, n.first + n.second);
    }
    return out;
}

} // namespace nebula
