//
// Signaling.h - out-of-band session establishment abstraction (placeholder)
//
// Signaling is the side channel two peers use to find and agree on how to reach
// each other *before* media flows. For the current direct-connection transport
// it is trivial (the CWA already knows the VDA's host:port). It exists now so a
// future NAT-traversal backend (ICE/STUN/TURN) can plug in without touching the
// VdaServer / CwaClient pipeline:
//
//   * gather local ICE candidates (host / server-reflexive via STUN / relay via TURN)
//   * exchange an offer/answer (candidates + transport params) with the peer
//   * hand the negotiated path to the transport
//
// Today only DirectSignaling is implemented: it carries a fixed host:port and
// performs no negotiation.
//
#pragma once

#include <cstdint>
#include <functional>
#include <memory>
#include <string>

namespace nebula {

// A peer's reachability description. With ICE this grows to a list of candidates
// and credentials; today it is just a host:port plus an opaque blob slot.
struct PeerDescriptor {
    std::string host;     // direct backend: dotted IP / hostname
    uint16_t    port = 0; // direct backend: UDP port
    std::string blob;     // reserved: serialized ICE candidates / SDP-like payload
};

enum class SignalingRole : uint8_t { Offerer /*CWA*/, Answerer /*VDA*/ };

class ISignaling {
public:
    // Called when the remote peer's descriptor is known and media can start.
    using OnRemoteCb = std::function<void(const PeerDescriptor& remote)>;

    virtual ~ISignaling() = default;

    // Begin signaling. For DirectSignaling this immediately reports the known
    // remote. For a future ICE backend this triggers gather + offer/answer.
    virtual bool start(SignalingRole role, OnRemoteCb onRemote) = 0;

    // Publish our local descriptor to the peer (no-op for direct).
    virtual void setLocalDescriptor(const PeerDescriptor& local) = 0;

    virtual void stop() = 0;
};

// Direct (no-NAT) signaling: the answerer's host:port is already known to the
// offerer, so start() reports it back immediately with no exchange.
std::unique_ptr<ISignaling> CreateDirectSignaling(const PeerDescriptor& remote);

} // namespace nebula
