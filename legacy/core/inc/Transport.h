//
// Transport.h - generic media transport abstraction for Nebula
//
// The transport delivers three independent logical channels (Control / Video /
// Audio) between VDA and CWA. It hides *how* bytes move: today the only backend
// is QUIC over a direct connection, but the same interface is designed to host a
// future NAT-traversing backend (ICE/STUN/TURN + QUIC) without any change to the
// codec, render, or pipeline code that depends only on ITransport.
//
#pragma once

#include <cstdint>
#include <cstddef>
#include <functional>
#include <memory>
#include <string>

namespace nebula {

// Selects the transport backend.
enum class TransportType : uint8_t {
    Quic    = 0, // Network.framework QUIC, direct peer-to-peer connection
    Relay   = 1, // tunnel all channels through a nebula_relay (blind forward)
    IceQuic = 2, // reserved: ICE-negotiated direct path + QUIC (future)
};

class ITransport {
public:
    // Logical channels, each mapped to an independent flow so video / audio /
    // control never head-of-line block each other.
    enum class Channel : uint32_t {
        Control = 0, // bidirectional: handshake + input events
        Video   = 1, // VDA -> CWA media
        Audio   = 2, // VDA -> CWA media
        Upgrade = 3, // transport-internal: relay->direct candidate exchange + probe
    };

    // Raw channel bytes (feed into nebula::FrameReader to recover framed messages).
    using RecvCb  = std::function<void(Channel, const uint8_t*, size_t)>;
    using StateCb = std::function<void(bool connected)>;

    virtual ~ITransport() = default;

    // A shared session secret. QUIC may use it for PSK-style auth; future
    // backends may use it to authenticate the signaling exchange. Call before start.
    virtual void setSharedSecret(const std::string& secret) = 0;

    // Configure relay mode: instead of connecting peer-to-peer, the endpoint
    // connects to a relay and is bridged to its partner. `isVda` selects whether
    // this endpoint registers the device (VDA) or targets it (CWA). No-op for the
    // direct transport. When set, startListener/connect dial the relay.
    // `ticket` (CWA only) is an optional reconnect ticket from a prior session
    // (see RelayProtocol.h) that can authorize pairing without the long-lived
    // token; pass "" on a first-time connect.
    virtual void configureRelay(const std::string& relayHost, uint16_t relayPort,
                                const std::string& deviceId, const std::string& token,
                                bool isVda, const std::string& ticket = "") {}

    // After a relay pairing, returns the fresh reconnect ticket the relay
    // issued (CWA role only); empty if not in relay mode or not yet paired.
    virtual std::string relayTicket() const { return {}; }

    // After a relay pairing, returns the PARTNER's public ip:port as
    // observed by the relay on its own QUIC connection to that partner (see
    // RelayProtocol.h's PeerAddrWire). This is the relay's free substitute
    // for an external STUN server, used by UpgradingTransport to attempt a
    // direct connection / fire a NAT-punch packet. Returns false if not in
    // relay mode, not yet paired, or the relay had no usable address.
    virtual bool peerObservedAddr(std::string& ip, uint16_t& port) const { return false; }

    // Start as VDA (listen) or CWA (connect). For a future ICE backend these
    // become "gather + offer/answer + connect" behind the same call.
    virtual bool startListener(uint16_t port) = 0;
    virtual bool connect(const std::string& host, uint16_t port) = 0;

    // Send raw bytes on a channel; the underlying flow is opened lazily.
    virtual bool send(Channel ch, const uint8_t* data, size_t len) = 0;

    virtual void setOnReceive(RecvCb cb) = 0;
    virtual void setOnState(StateCb cb) = 0;
    virtual void close() = 0;
};

// Creates a transport of the requested type. Falls back to Quic for any type
// not yet implemented.
std::unique_ptr<ITransport> CreateTransport(TransportType type = TransportType::Quic);

} // namespace nebula
