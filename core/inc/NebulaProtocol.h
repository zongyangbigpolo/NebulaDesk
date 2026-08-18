//
// NebulaProtocol.h - wire framing for Nebula messages over QUIC
//
#pragma once

#include "NebulaTypes.h"
#include <cstdint>
#include <vector>

namespace nebula {

constexpr uint32_t kNebulaMagic   = 0x5542454E; // "NEBU" on the little-endian wire
constexpr uint8_t  kNebulaVersion = 2;

enum class MsgType : uint8_t {
    Hello     = 1,
    HelloAck  = 2,
    Video     = 3,
    Audio     = 4,
    Input     = 5, // phase 2
    Bye       = 6,
    // Session-crypto handshake (see NebulaCrypto.h). These two are the ONLY
    // message types ever sent in cleartext; every other type's payload is
    // ChaCha20-Poly1305 sealed once the handshake completes.
    KeyInit    = 7,
    KeyInitAck = 8,
};

enum FrameFlags : uint16_t {
    Flag_None     = 0,
    Flag_Keyframe = 1 << 0,
    Flag_Config   = 1 << 1, // payload is codec config (parameter sets / ASC)
};

// 24-byte fixed little-endian header preceding every message payload.
#pragma pack(push, 1)
struct NebulaFrameHeader {
    uint32_t magic;       // kNebulaMagic
    uint8_t  version;     // kNebulaVersion
    uint8_t  type;        // MsgType
    uint16_t flags;       // FrameFlags
    uint32_t length;      // payload byte count
    uint32_t seq;         // per-stream sequence number
    uint64_t timestampUs; // capture / presentation time (microseconds)
};
#pragma pack(pop)

static_assert(sizeof(NebulaFrameHeader) == 24, "NebulaFrameHeader must be 24 bytes");

constexpr size_t kHeaderSize = sizeof(NebulaFrameHeader);

// Serialize a header + payload into a single buffer ready for QUIC send.
std::vector<uint8_t> BuildMessage(MsgType type, uint16_t flags, uint32_t seq,
                                  uint64_t timestampUs,
                                  const uint8_t* payload, size_t payloadLen);

// Parse a header from a buffer. Returns false if magic/version/size invalid.
bool ParseHeader(const uint8_t* data, size_t len, NebulaFrameHeader& out);

// Encode / decode capability handshake payloads.
std::vector<uint8_t> EncodeCaps(const NebulaCaps& caps);
bool DecodeCaps(const uint8_t* data, size_t len, NebulaCaps& out);

} // namespace nebula
