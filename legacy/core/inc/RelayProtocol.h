//
// RelayProtocol.h - control messages between client and nebula_relay (header-only)
//
// The relay multiplexes many sessions. When a peer connects to the relay it
// first sends a fixed-size RelayHello identifying its role and the device it is
// registering (VDA) or targeting (CWA). After the relay pairs a VDA and a CWA,
// it blind-forwards the subsequent byte stream between them.
//
// NAT punch candidates: the relay also observes each peer's public ip:port
// directly off its own QUIC connection to that peer (no external STUN
// server needed) and hands it to the *other* side as part of the Paired
// reply (see PeerAddrWire below). UpgradingTransport.mm uses this to attempt
// a direct connection and, on the VDA side, to fire a best-effort NAT-punch
// packet toward the CWA's observed address.
//
#pragma once

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

namespace nebula {

constexpr uint32_t kRelayMagic   = 0x594C524E; // "NRLY" on the little-endian wire
constexpr uint8_t  kRelayVersion = 2;

enum class RelayRole : uint8_t {
    Vda = 1, // registers/announces a device-id and waits for a viewer
    Cwa = 2, // targets a device-id (with a pairing token) to be bridged
};

// Fixed identifiers: 32-byte device id + 32-byte pairing token (zero-padded).
constexpr size_t kDeviceIdLen = 32;
constexpr size_t kTokenLen    = 32;
// Optional short-lived reconnect ticket (see RelayStatus::Paired handling in
// the relay and RelayTransport). Zero-filled/absent on a first-time connect.
constexpr size_t kTicketLen   = 32;

#pragma pack(push, 1)
struct RelayHello {
    uint32_t magic;                 // kRelayMagic
    uint8_t  version;               // kRelayVersion
    uint8_t  role;                  // RelayRole
    uint16_t reserved;
    char     deviceId[kDeviceIdLen];// device being registered (VDA) / targeted (CWA)
    char     token[kTokenLen];      // pairing token (must match for CWA<->VDA)
    char     ticket[kTicketLen];    // optional: a ticket from a prior Paired reply;
                                     // if valid, authorizes the CWA without matching token
};
#pragma pack(pop)

static_assert(sizeof(RelayHello) == 8 + kDeviceIdLen + kTokenLen + kTicketLen, "RelayHello size");

constexpr size_t kRelayHelloSize = sizeof(RelayHello);

// Relay's reply to a hello (1 byte status, optionally followed by stream data).
enum class RelayStatus : uint8_t {
    Registered = 1, // VDA: device registered, waiting for viewer
    Paired     = 2, // bridge established, byte-forwarding begins.
                    // This status byte is ALWAYS followed by a fixed-size
                    // PeerAddr block (see below) carrying the *partner's*
                    // observed public ip:port, as seen by the relay on its
                    // own QUIC connection to that partner — this is how the
                    // relay hands out NAT-punch candidates for free, without
                    // either side needing an external STUN server (see
                    // UpgradingTransport.mm). For a CWA specifically, the
                    // PeerAddr block is itself followed by kTicketLen more
                    // bytes: a fresh reconnect ticket the CWA can present
                    // next time instead of the long-lived token.
    NoSuchDevice = 3,
    BadToken   = 4,
    Busy       = 5,
    BadHello   = 6,
    Superseded = 7, // a newer CWA took over this device's pairing
};

// A peer's observed public endpoint, as seen by the relay on the QUIC
// connection that peer used to reach it. Fixed-size so both sides can parse
// it without a length-prefixed round trip. `ipLen == 0` means "unavailable"
// (e.g. the relay couldn't resolve a usable address) — the receiving side
// should treat that as "no relay-observed candidate", not an error.
constexpr size_t kPeerAddrIpCap = 45; // fits an IPv6 text address + NUL

#pragma pack(push, 1)
struct PeerAddrWire {
    uint8_t  ipLen;                 // valid bytes in `ip`; 0 = unavailable
    char     ip[kPeerAddrIpCap];     // ASCII, NOT null-padded beyond ipLen
    uint16_t port;                   // host byte order
};
#pragma pack(pop)

constexpr size_t kPeerAddrWireSize = sizeof(PeerAddrWire);
static_assert(kPeerAddrWireSize == 1 + kPeerAddrIpCap + 2, "PeerAddrWire size");

inline std::vector<uint8_t> BuildPeerAddr(const std::string& ip, uint16_t port) {
    PeerAddrWire w{};
    w.ipLen = (uint8_t)std::min(ip.size(), kPeerAddrIpCap);
    std::memcpy(w.ip, ip.data(), w.ipLen);
    w.port = port;
    const uint8_t* p = reinterpret_cast<const uint8_t*>(&w);
    return std::vector<uint8_t>(p, p + sizeof(w));
}

// Parses a fixed-size PeerAddrWire block at `data` (must have >= kPeerAddrWireSize
// bytes available). Returns false only if `ipLen` says "unavailable" (0) or is
// out of range; `ip`/`port` are left untouched in that case.
inline bool ParsePeerAddr(const uint8_t* data, std::string& ip, uint16_t& port) {
    PeerAddrWire w{};
    std::memcpy(&w, data, sizeof(w));
    if (w.ipLen == 0 || w.ipLen > kPeerAddrIpCap) return false;
    ip.assign(w.ip, w.ipLen);
    port = w.port;
    return true;
}

inline void FillFixed(char* dst, size_t cap, const std::string& s) {
    std::memset(dst, 0, cap);
    std::memcpy(dst, s.data(), s.size() < cap ? s.size() : cap);
}

inline std::string ReadFixed(const char* src, size_t cap) {
    size_t n = 0;
    while (n < cap && src[n] != '\0') ++n;
    return std::string(src, n);
}

inline std::vector<uint8_t> BuildRelayHello(RelayRole role,
                                            const std::string& deviceId,
                                            const std::string& token,
                                            const std::string& ticket = "") {
    RelayHello h{};
    h.magic = kRelayMagic;
    h.version = kRelayVersion;
    h.role = static_cast<uint8_t>(role);
    h.reserved = 0;
    FillFixed(h.deviceId, kDeviceIdLen, deviceId);
    FillFixed(h.token, kTokenLen, token);
    FillFixed(h.ticket, kTicketLen, ticket);
    const uint8_t* p = reinterpret_cast<const uint8_t*>(&h);
    return std::vector<uint8_t>(p, p + sizeof(h));
}

inline bool ParseRelayHello(const uint8_t* data, size_t len, RelayHello& out) {
    if (!data || len < kRelayHelloSize) return false;
    std::memcpy(&out, data, kRelayHelloSize);
    return out.magic == kRelayMagic && out.version == kRelayVersion;
}

} // namespace nebula
