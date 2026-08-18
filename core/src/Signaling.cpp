//
// Signaling.cpp - DirectSignaling implementation (placeholder for future ICE)
//
#include "Signaling.h"

namespace nebula {
namespace {

// Trivial signaling for direct connections: the remote descriptor is already
// known, so starting simply reports it. A future IceSignaling will replace this
// with STUN candidate gathering and offer/answer exchange behind the same API.
class DirectSignaling final : public ISignaling {
public:
    explicit DirectSignaling(const PeerDescriptor& remote) : m_remote(remote) {}

    bool start(SignalingRole, OnRemoteCb onRemote) override {
        if (onRemote) onRemote(m_remote);
        return true;
    }
    void setLocalDescriptor(const PeerDescriptor&) override { /* no-op for direct */ }
    void stop() override {}

private:
    PeerDescriptor m_remote;
};

} // namespace

std::unique_ptr<ISignaling> CreateDirectSignaling(const PeerDescriptor& remote) {
    return std::make_unique<DirectSignaling>(remote);
}

} // namespace nebula
