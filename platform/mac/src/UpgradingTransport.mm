//
// UpgradingTransport.mm - relay-first transport that auto-upgrades to direct.
//
// Composes a RelayTransport (immediate connectivity, blind-forwarded) with a
// direct QuicTransport. The relay hands each side the OTHER side's public
// ip:port for free — observed directly off their QUIC connections to the
// relay, no external STUN server involved (see RelayProtocol.h's
// PeerAddrWire and server/nebula_relay/main.cpp's GetRemoteAddr). Using that:
//   - the CWA dials the VDA's LAN address (if advertised in time) and/or its
//     relay-observed public address, in that preference order;
//   - the VDA fires a best-effort NAT-punch packet toward the CWA's
//     relay-observed address, so a port/address-restricted NAT on the VDA's
//     side has already seen outbound traffic to the CWA before the CWA's
//     direct-connect attempt arrives (classic UDP hole punching; see
//     firePunchToPeer()).
// Once a direct attempt is probe-validated, media migrates to it while the
// relay is kept as a fallback. To the app this is a single ITransport whose
// active path switches transparently; media never stops.
//
// NOTE: this only helps on cone/port-preserving and (with the punch) on
// address/port-restricted NATs. A symmetric NAT on either side still can't
// be traversed this way — the relay's blind-forwarding fallback (kept alive
// throughout) is the permanent, secure (end-to-end encrypted) answer for
// that case; see NAT_TRAVERSAL.md.
//
#include "Transport.h"
#include "LocalAddr.h"
#include "NebulaLog.h"

#import <Network/Network.h>
#import <dispatch/dispatch.h>

#include <algorithm>
#include <atomic>
#include <mutex>
#include <string>
#include <vector>

#define TAG "upgrade"

namespace nebula {

std::unique_ptr<ITransport> CreateRelayTransport();

namespace {

// A candidate the VDA advertises for the CWA to dial directly.
struct Candidate { std::string ip; uint16_t port; };

// Internal Upgrade-channel message types.
enum class UpMsg : uint8_t {
    Candidate = 1, // [count:1] { [port:2 LE][ip_len:1][ip ascii] } * count
    Probe     = 2, // sent over the DIRECT path to validate it
    ProbeAck  = 3, // reply over the DIRECT path
    Switch    = 4, // sent over RELAY to tell the peer to go direct
};

// How long the CWA waits for the VDA's LAN-candidate Upgrade message before
// starting its direct attempt with just the relay-observed public candidate.
// The VDA sends its LAN candidate immediately on relay-bridge (no STUN round
// trip anymore), so this only needs to cover normal network jitter.
constexpr int64_t kLanCandidateGraceMs = 150;

// Fire-and-forget UDP datagrams sent to punch a hole in the sender's own NAT
// (see module comment). No response is expected or parsed.
void FirePunchPackets(uint16_t localPort, const std::string& peerIp, uint16_t peerPort) {
    nw_parameters_t params = nw_parameters_create_secure_udp(
        NW_PARAMETERS_DISABLE_PROTOCOL, NW_PARAMETERS_DEFAULT_CONFIGURATION);
    char localPortStr[8];
    snprintf(localPortStr, sizeof(localPortStr), "%u", localPort);
    nw_endpoint_t localEp = nw_endpoint_create_host("0.0.0.0", localPortStr);
    nw_parameters_set_local_endpoint(params, localEp);
    nw_parameters_set_reuse_local_address(params, true);
    nw_release(localEp);

    char peerPortStr[8];
    snprintf(peerPortStr, sizeof(peerPortStr), "%u", peerPort);
    nw_endpoint_t peerEp = nw_endpoint_create_host(peerIp.c_str(), peerPortStr);
    nw_connection_t conn = nw_connection_create(peerEp, params);
    nw_release(peerEp);
    nw_release(params);
    if (!conn) return;
    nw_retain(conn);

    dispatch_queue_t queue = dispatch_queue_create("com.nebula.punch", DISPATCH_QUEUE_SERIAL);
    nw_connection_set_queue(conn, queue);
    // A tiny, meaningless payload — the NAT only cares that a packet went
    // out toward peerIp:peerPort from our listener's local port.
    static const uint8_t kPunchPayload[1] = {0};
    nw_connection_set_state_changed_handler(conn, ^(nw_connection_state_t st, nw_error_t) {
        if (st == nw_connection_state_ready) {
            for (int i = 0; i < 3; ++i) {
                dispatch_data_t dd = dispatch_data_create(kPunchPayload, sizeof(kPunchPayload),
                                                          queue, DISPATCH_DATA_DESTRUCTOR_DEFAULT);
                nw_connection_send(conn, dd, NW_CONNECTION_DEFAULT_MESSAGE_CONTEXT, false, ^(nw_error_t){});
                dispatch_release(dd);
            }
        }
    });
    nw_connection_start(conn);
    NEBULA_LOGI(TAG, "fired NAT-punch packets toward %s:%u (from local port %u)",
              peerIp.c_str(), peerPort, localPort);

    // Best-effort: tear down shortly after, regardless of outcome.
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)800 * NSEC_PER_MSEC), queue, ^{
        nw_connection_cancel(conn);
        nw_release(conn);
        dispatch_release(queue);
    });
}

class UpgradingTransport final : public ITransport {
public:
    UpgradingTransport()
        : m_relay(CreateRelayTransport()),
          m_direct(CreateTransport(TransportType::Quic)) {}
    ~UpgradingTransport() override { close(); }

    void setSharedSecret(const std::string& s) override {
        m_secret = s;
        m_relay->setSharedSecret(s);
        m_direct->setSharedSecret(s);
    }

    void configureRelay(const std::string& host, uint16_t port,
                        const std::string& deviceId, const std::string& token,
                        bool isVda, const std::string& ticket) override {
        m_isVda = isVda;
        m_relay->configureRelay(host, port, deviceId, token, isVda, ticket);
    }

    std::string relayTicket() const override { return m_relay->relayTicket(); }

    bool peerObservedAddr(std::string& ip, uint16_t& port) const override {
        return m_relay->peerObservedAddr(ip, port);
    }

    bool startListener(uint16_t port) override { m_directPort = port; return begin(); }
    bool connect(const std::string&, uint16_t) override { return begin(); }

    bool send(Channel ch, const uint8_t* data, size_t len) override {
        // App media/control follow the active path; Upgrade is internal-only.
        ITransport* path = (m_active.load() == Path::Direct) ? m_direct.get() : m_relay.get();
        return path->send(ch, data, len);
    }

    void setOnReceive(RecvCb cb) override { m_appRecv = std::move(cb); }
    void setOnState(StateCb cb) override { m_appState = std::move(cb); }

    void close() override {
        if (m_relay) m_relay->close();
        if (m_direct) m_direct->close();
    }

private:
    enum class Path { Relay, Direct };

    bool begin() {
        // Wire relay callbacks.
        m_relay->setOnReceive([this](Channel ch, const uint8_t* d, size_t n) {
            if (ch == Channel::Upgrade) onUpgradeRelay(d, n);
            else if (m_appRecv) m_appRecv(ch, d, n);
        });
        m_relay->setOnState([this](bool up) {
            if (up && !m_relayUp.exchange(true)) {
                if (m_appState) m_appState(true); // app sees "connected" as soon as relay bridges
                onRelayBridged();
            } else if (!up && m_appState && m_active.load() == Path::Relay) {
                m_appState(false);
            }
        });

        wireDirectCallbacks();

        // Relay path drives both roles (VDA registers, CWA targets).
        return m_isVda ? m_relay->startListener(0) : m_relay->connect("", 0);
    }

    // Wire the direct transport's callbacks. Split out from begin() so a
    // recreated m_direct (see onDirectStateChanged) can be rewired identically.
    void wireDirectCallbacks() {
        m_direct->setOnReceive([this](Channel ch, const uint8_t* d, size_t n) {
            if (ch == Channel::Upgrade) onUpgradeDirect(d, n);
            else if (m_appRecv) m_appRecv(ch, d, n);
        });
        m_direct->setOnState([this](bool up) { onDirectStateChanged(up); });
    }

    // Relay bridge established: begin the upgrade attempt. Both roles already
    // know the OTHER side's relay-observed public address at this point (see
    // RelayTransport::peerObservedAddr) — no STUN round trip needed.
    void onRelayBridged() {
        NEBULA_LOGI(TAG, "relay bridged; attempting direct upgrade");
        if (m_isVda) {
            m_direct->startListener(m_directPort);
            sendLanCandidate(); // fast now: no STUN wait gates this anymore
            firePunchToPeer();
        } else {
            // Give the VDA's LAN candidate a short grace window to arrive
            // (it's advertised immediately, so this only covers normal
            // network jitter) before starting with just the relay-observed
            // public candidate.
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, kLanCandidateGraceMs * (int64_t)NSEC_PER_MSEC),
                           dispatch_get_global_queue(0, 0), ^{
                bool expected = false;
                if (m_directTried.compare_exchange_strong(expected, true)) {
                    startDirectAttempt();
                }
            });
        }
    }

    // CWA only: builds the candidate list (LAN candidate received from the
    // VDA, if any arrived in time, tried first; relay-observed public
    // candidate always appended) and starts dialing.
    void startDirectAttempt() {
        std::vector<Candidate> cands;
        {
            std::lock_guard<std::mutex> lk(m_lanMutex);
            cands = m_lanCandidates;
        }
        std::string ip; uint16_t port = 0;
        if (m_relay->peerObservedAddr(ip, port)) cands.push_back(Candidate{ip, port});
        if (cands.empty()) {
            NEBULA_LOGW(TAG, "no usable candidate; staying on relay");
            m_directTried.store(false);
            return;
        }
        tryDirectCandidates(std::move(cands), 0);
    }

    // VDA only: advertise our LAN address so a same-network CWA can dial it
    // directly (the relay can't know this — it only sees our public egress).
    void sendLanCandidate() {
        std::string lan = LocalIPv4();
        if (lan.empty()) { NEBULA_LOGW(TAG, "no LAN address to advertise"); return; }
        std::vector<uint8_t> m;
        m.push_back((uint8_t)UpMsg::Candidate);
        m.push_back(1);
        m.push_back(m_directPort & 0xff);
        m.push_back((m_directPort >> 8) & 0xff);
        m.push_back((uint8_t)std::min<size_t>(lan.size(), 255));
        m.insert(m.end(), lan.begin(), lan.begin() + std::min<size_t>(lan.size(), 255));
        m_relay->send(Channel::Upgrade, m.data(), m.size());
        NEBULA_LOGI(TAG, "sent LAN candidate %s:%u", lan.c_str(), m_directPort);
    }

    // VDA only: best-effort NAT punch toward the CWA's relay-observed
    // address (see FirePunchPackets / module comment). No-op if the relay
    // couldn't observe a usable address for the CWA.
    void firePunchToPeer() {
        std::string ip; uint16_t port = 0;
        if (!m_relay->peerObservedAddr(ip, port)) {
            NEBULA_LOGI(TAG, "no relay-observed peer address; skipping NAT punch");
            return;
        }
        FirePunchPackets(m_directPort, ip, port);
    }

    // Upgrade messages arriving over the RELAY path.
    void onUpgradeRelay(const uint8_t* d, size_t n) {
        if (n < 1) return;
        switch ((UpMsg)d[0]) {
            case UpMsg::Candidate: {
                if (n < 2) return;
                uint8_t count = d[1];
                std::vector<Candidate> cands;
                size_t off = 2;
                for (uint8_t i = 0; i < count && off + 3 <= n; ++i) {
                    uint16_t port = d[off] | (d[off + 1] << 8);
                    uint8_t ipLen = d[off + 2];
                    off += 3;
                    if (off + ipLen > n) break;
                    cands.push_back(Candidate{std::string((const char*)d + off, ipLen), port});
                    off += ipLen;
                }
                for (auto& c : cands) NEBULA_LOGI(TAG, "got peer LAN candidate %s:%u", c.ip.c_str(), c.port);
                if (m_isVda || cands.empty()) break;
                {
                    std::lock_guard<std::mutex> lk(m_lanMutex);
                    m_lanCandidates = cands; // tried first, ahead of the relay-observed one
                }
                // If the grace-window timer already fired and started an
                // attempt with just the public candidate, a late LAN
                // candidate is simply missed this round (rare, since the VDA
                // sends it with no STUN delay now); the periodic re-advertise
                // on any future relay-fallback still gives it another shot.
                bool expected = false;
                if (m_directTried.compare_exchange_strong(expected, true)) {
                    startDirectAttempt();
                }
                break;
            }
            case UpMsg::Switch:
                NEBULA_LOGI(TAG, "peer says switch -> direct");
                activateDirect();
                break;
            default: break;
        }
    }


    // Upgrade messages arriving over the DIRECT path (probe handshake).
    void onUpgradeDirect(const uint8_t* d, size_t n) {
        if (n < 1) return;
        switch ((UpMsg)d[0]) {
            case UpMsg::Probe: {
                uint8_t ack = (uint8_t)UpMsg::ProbeAck;
                m_direct->send(Channel::Upgrade, &ack, 1); // VDA replies on direct
                break;
            }
            case UpMsg::ProbeAck: {
                // CWA: direct path validated -> tell VDA to switch, then switch.
                NEBULA_LOGI(TAG, "direct probe ack — upgrading to direct");
                uint8_t sw = (uint8_t)UpMsg::Switch;
                m_relay->send(Channel::Upgrade, &sw, 1);
                activateDirect();
                break;
            }
            default: break;
        }
    }

    // CWA: try each candidate in turn (LAN first, since it's advertised
    // first and is by far the common case), giving each a short window to
    // probe-ack before moving on to the next. Stops as soon as one succeeds
    // (activateDirect() flips m_active) or the list is exhausted.
    void tryDirectCandidates(std::vector<Candidate> cands, size_t index) {
        if (m_active.load() == Path::Direct) return; // already upgraded
        if (index >= cands.size()) {
            NEBULA_LOGW(TAG, "no candidate produced a working direct path; staying on relay");
            m_directTried.store(false); // allow a future candidate broadcast to retry
            return;
        }
        const Candidate& c = cands[index];
        NEBULA_LOGI(TAG, "trying candidate %zu/%zu: %s:%u", index + 1, cands.size(), c.ip.c_str(), c.port);
        if (!m_direct->connect(c.ip, c.port)) {
            NEBULA_LOGW(TAG, "direct connect to %s:%u failed", c.ip.c_str(), c.port);
            tryNextCandidateSoon(std::move(cands), index + 1);
            return;
        }
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(300 * NSEC_PER_MSEC)),
                       dispatch_get_global_queue(0, 0), ^{
            if (m_active.load() == Path::Direct) return;
            uint8_t probe = (uint8_t)UpMsg::Probe;
            m_direct->send(Channel::Upgrade, &probe, 1);
            NEBULA_LOGI(TAG, "sent direct probe to %s:%u", c.ip.c_str(), c.port);
        });
        // Give this candidate ~1s total (300ms settle + 700ms for a round
        // trip probe/ack) before giving up and trying the next one.
        tryNextCandidateSoon(std::move(cands), index + 1, 1000);
    }

    void tryNextCandidateSoon(std::vector<Candidate> cands, size_t nextIndex, int64_t delayMs = 0) {
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, delayMs * (int64_t)NSEC_PER_MSEC),
                       dispatch_get_global_queue(0, 0), ^{
            if (m_active.load() == Path::Direct) return;
            // Start the next attempt from a clean transport instance — see
            // onDirectStateChanged for why a used QuicTransport can't simply
            // dial a second endpoint.
            m_direct = CreateTransport(TransportType::Quic);
            m_direct->setSharedSecret(m_secret);
            wireDirectCallbacks();
            tryDirectCandidates(std::move(cands), nextIndex);
        });
    }

    void activateDirect() {
        if (m_active.exchange(Path::Direct) != Path::Direct) {
            NEBULA_LOGI(TAG, "media path is now DIRECT (relay kept as fallback)");
        }
    }

    // Direct transport reports connectivity changes. Losing the direct path
    // while it was the active one must NOT drop the session — fall back to
    // relay (which has been kept alive as a fallback all along) and, best
    // effort, retry the upgrade so a transient blip can re-promote to direct
    // again without any app-visible interruption.
    void onDirectStateChanged(bool up) {
        if (up) return; // direct readiness for a NEW attempt is tracked via probe/ack, not here.
        if (m_active.exchange(Path::Relay) == Path::Direct) {
            NEBULA_LOGW(TAG, "direct path lost — falling back to relay (relay kept the media flowing)");
        }
        // Allow a fresh candidate/probe cycle; without this a one-time upgrade
        // failure would permanently pin the session to relay.
        m_directTried.store(false);
        if (!m_isVda) {
            // Client role: Network.framework's per-channel connection objects
            // stay recorded (failed) inside the old QuicTransport, so a second
            // connect() on the SAME instance would reuse dead connections
            // instead of dialing fresh ones. Start clean for the next retry.
            m_direct = CreateTransport(TransportType::Quic);
            m_direct->setSharedSecret(m_secret);
            wireDirectCallbacks();
        }
        if (m_isVda && m_relayUp.load()) {
            // Re-advertise our LAN candidate and re-fire the NAT punch
            // shortly after the drop so the CWA can retry the handshake once
            // our direct listener is healthy again.
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(2 * NSEC_PER_SEC)),
                           dispatch_get_global_queue(0, 0), ^{
                if (m_relayUp.load()) { sendLanCandidate(); firePunchToPeer(); }
            });
        }
    }

    std::unique_ptr<ITransport> m_relay;
    std::unique_ptr<ITransport> m_direct;
    std::string m_secret;
    bool        m_isVda = false;
    uint16_t    m_directPort = 7000;
    std::atomic<bool> m_relayUp{false};
    // Guards against two overlapping tryDirectCandidates() sequences (e.g. a
    // second candidate broadcast arriving while one is still being tried).
    std::atomic<bool> m_directTried{false};
    std::atomic<Path> m_active{Path::Relay};
    // CWA only: the VDA's LAN candidate(s), if received before the grace
    // window in startDirectAttempt() elapsed.
    std::mutex m_lanMutex;
    std::vector<Candidate> m_lanCandidates;
    RecvCb  m_appRecv;
    StateCb m_appState;
};

} // namespace

std::unique_ptr<ITransport> CreateUpgradingTransport() {
    return std::make_unique<UpgradingTransport>();
}

} // namespace nebula
