//
// RelayTransport.mm - ITransport that tunnels all channels through a relay.
//
// Opens ONE QUIC connection (ALPN "nebula-relay") to the relay and ONE bidi
// stream. Sends a RelayHello (role + device-id + token); after the relay reports
// Paired, multiplexes the Control/Video/Audio channels over that single stream
// using a tiny frame header [channel:1][len:4][payload]. The relay blind-forwards
// the stream to the partner, whose RelayTransport demuxes it back into channels.
//
#include "Transport.h"
#include "RelayProtocol.h"
#include "NebulaLog.h"

#import <Network/Network.h>
#import <Security/Security.h>

#include <algorithm>
#include <mutex>
#include <string>
#include <vector>

#define TAG "relay-tp"

namespace nebula {
namespace {

dispatch_data_t MakeData(const void* bytes, size_t len) {
    return dispatch_data_create(bytes, len, dispatch_get_global_queue(0, 0),
                                DISPATCH_DATA_DESTRUCTOR_DEFAULT);
}

std::vector<uint8_t> Flatten(dispatch_data_t data) {
    std::vector<uint8_t> out;
    if (!data) return out;
    const void* ptr = nullptr; size_t size = 0;
    dispatch_data_t mapped = dispatch_data_create_map(data, &ptr, &size);
    out.assign((const uint8_t*)ptr, (const uint8_t*)ptr + size);
    if (mapped) dispatch_release(mapped);
    return out;
}

class RelayTransport final : public ITransport {
public:
    RelayTransport() {
        m_queue = dispatch_queue_create("com.nebula.relaytp", DISPATCH_QUEUE_SERIAL);
    }
    ~RelayTransport() override { close(); if (m_queue) dispatch_release(m_queue); }

    void setSharedSecret(const std::string& s) override { m_secret = s; }

    void configureRelay(const std::string& host, uint16_t port,
                        const std::string& deviceId, const std::string& token,
                        bool isVda, const std::string& ticket) override {
        m_relayHost = host; m_relayPort = port;
        m_deviceId = deviceId; m_token = token; m_isVda = isVda;
        m_ticket = ticket;
    }

    std::string relayTicket() const override {
        std::lock_guard<std::mutex> lk(m_mutex);
        return m_ticket;
    }

    bool peerObservedAddr(std::string& ip, uint16_t& port) const override {
        std::lock_guard<std::mutex> lk(m_mutex);
        if (m_peerIp.empty()) return false;
        ip = m_peerIp;
        port = m_peerPort;
        return true;
    }

    // Both roles dial the relay; startListener (VDA) and connect (CWA) converge.
    bool startListener(uint16_t) override { return dialRelay(); }
    bool connect(const std::string&, uint16_t) override { return dialRelay(); }

    bool send(Channel ch, const uint8_t* data, size_t len) override {
        std::lock_guard<std::mutex> lk(m_mutex);
        // Frame: [channel:1][len:4 LE][payload]
        std::vector<uint8_t> framed;
        framed.reserve(5 + len);
        framed.push_back((uint8_t)ch);
        for (int i = 0; i < 4; ++i) framed.push_back((len >> (i*8)) & 0xff);
        framed.insert(framed.end(), data, data + len);

        if (!m_paired || !m_stream) { m_pending.insert(m_pending.end(), framed.begin(), framed.end()); return true; }
        rawSend(framed.data(), framed.size());
        return true;
    }

    void setOnReceive(RecvCb cb) override { m_onRecv = std::move(cb); }
    void setOnState(StateCb cb) override { m_onState = std::move(cb); }

    void close() override {
        std::lock_guard<std::mutex> lk(m_mutex);
        if (m_stream) { nw_connection_cancel(m_stream); nw_release(m_stream); m_stream = nullptr; }
    }

private:
    bool dialRelay() {
        nw_parameters_t params = nw_parameters_create_quic(^(nw_protocol_options_t quic) {
            nw_quic_add_tls_application_protocol(quic, "nebula-relay");
            nw_quic_set_idle_timeout(quic, 60000);
            nw_quic_set_initial_max_streams_bidirectional(quic, 4);
            nw_quic_set_initial_max_data(quic, 16 * 1024 * 1024);
            nw_quic_set_initial_max_stream_data_bidirectional_local(quic, 8 * 1024 * 1024);
            nw_quic_set_initial_max_stream_data_bidirectional_remote(quic, 8 * 1024 * 1024);
            sec_protocol_options_t sec = nw_quic_copy_sec_protocol_options(quic);
            sec_protocol_options_set_min_tls_protocol_version(sec, tls_protocol_version_TLSv13);
            // Dev: trust the relay's self-signed certificate.
            sec_protocol_options_set_verify_block(sec,
                ^(sec_protocol_metadata_t, sec_trust_t, sec_protocol_verify_complete_t complete) {
                    complete(true);
                }, dispatch_get_global_queue(0, 0));
            nw_release(sec);
        });

        char portStr[8]; snprintf(portStr, sizeof(portStr), "%u", m_relayPort);
        nw_endpoint_t ep = nw_endpoint_create_host(m_relayHost.c_str(), portStr);
        m_stream = nw_connection_create(ep, params);
        nw_release(ep); nw_release(params);
        if (!m_stream) { NEBULA_LOGE(TAG, "failed to create relay connection"); return false; }
        nw_retain(m_stream);
        nw_connection_set_queue(m_stream, m_queue);
        nw_connection_set_state_changed_handler(m_stream, ^(nw_connection_state_t st, nw_error_t err) {
            if (st == nw_connection_state_ready) { NEBULA_LOGI(TAG, "relay connection ready"); sendHello(); }
            if (st == nw_connection_state_failed) { NEBULA_LOGE(TAG, "relay connection failed"); if (m_onState) m_onState(false); }
        });
        nw_connection_start(m_stream);
        receiveLoop();
        NEBULA_LOGI(TAG, "dialing relay %s:%u as %s (device=%s)",
                  m_relayHost.c_str(), m_relayPort, m_isVda ? "VDA" : "CWA", m_deviceId.c_str());
        return true;
    }

    void sendHello() {
        auto hello = BuildRelayHello(m_isVda ? RelayRole::Vda : RelayRole::Cwa, m_deviceId, m_token, m_ticket);
        rawSend(hello.data(), hello.size());
    }

    void rawSend(const uint8_t* data, size_t len) {
        if (!m_stream) return;
        dispatch_data_t dd = MakeData(data, len);
        nw_connection_send(m_stream, dd, NW_CONNECTION_DEFAULT_MESSAGE_CONTEXT, false, ^(nw_error_t){});
        dispatch_release(dd);
    }

    void receiveLoop() {
        nw_connection_receive(m_stream, 1, 262144,
            ^(dispatch_data_t content, nw_content_context_t, bool isComplete, nw_error_t err) {
            if (content) { auto bytes = Flatten(content); onBytes(bytes.data(), bytes.size()); }
            if (err || isComplete) { if (m_onState) m_onState(false); return; }
            receiveLoop();
        });
    }

    // Consume relay status byte, then demux channel frames. User callbacks are
    // invoked WITHOUT holding m_mutex (they re-enter send() which locks it).
    void onBytes(const uint8_t* data, size_t len) {
        std::vector<std::pair<Channel, std::vector<uint8_t>>> deliver;
        bool firePaired = false;
        bool fireFail = false;
        {
            std::lock_guard<std::mutex> lk(m_mutex);
            m_rx.insert(m_rx.end(), data, data + len);

            if (!m_paired) {
                if (m_rx.empty()) return;
                RelayStatus st = (RelayStatus)m_rx.front();
                if (st == RelayStatus::Paired) {
                    // Both roles' Paired reply carries a fixed-size PeerAddr
                    // trailer (the partner's relay-observed public endpoint,
                    // see RelayProtocol.h); the CWA's is further followed by
                    // a fresh reconnect ticket. Wait for the whole thing.
                    size_t need = 1 + kPeerAddrWireSize + (m_isVda ? 0 : kTicketLen);
                    if (m_rx.size() < need) return;
                    std::string peerIp; uint16_t peerPort = 0;
                    if (ParsePeerAddr(m_rx.data() + 1, peerIp, peerPort)) {
                        m_peerIp = peerIp;
                        m_peerPort = peerPort;
                    }
                    if (!m_isVda) {
                        size_t ticketOff = 1 + kPeerAddrWireSize;
                        m_ticket.assign(m_rx.begin() + ticketOff, m_rx.begin() + ticketOff + kTicketLen);
                        // Trim trailing zero padding for a clean string.
                        auto nul = std::find(m_ticket.begin(), m_ticket.end(), '\0');
                        m_ticket.erase(nul, m_ticket.end());
                    }
                    m_rx.erase(m_rx.begin(), m_rx.begin() + need);
                    m_paired = true;
                    NEBULA_LOGI(TAG, "relay paired — bridge established%s (peer observed %s:%u)",
                              m_isVda ? "" : ", reconnect ticket received",
                              m_peerIp.empty() ? "?" : m_peerIp.c_str(), m_peerPort);
                    firePaired = true;
                } else {
                    m_rx.erase(m_rx.begin());
                    if (st == RelayStatus::Registered) {
                        NEBULA_LOGI(TAG, "registered with relay, waiting for peer");
                        return; // VDA waits for Paired next
                    } else if (st == RelayStatus::Superseded) {
                        NEBULA_LOGW(TAG, "a newer viewer took over this device — disconnecting");
                        fireFail = true;
                    } else {
                        NEBULA_LOGE(TAG, "relay error status=%d", (int)st);
                        fireFail = true;
                    }
                }
            }

            // Demux [channel:1][len:4][payload] frames.
            if (m_paired) {
                for (;;) {
                    if (m_rx.size() < 5) break;
                    uint8_t ch = m_rx[0];
                    uint32_t plen = 0;
                    for (int i = 0; i < 4; ++i) plen |= (uint32_t)m_rx[1 + i] << (i*8);
                    if (m_rx.size() < 5 + plen) break;
                    deliver.emplace_back((Channel)ch,
                        std::vector<uint8_t>(m_rx.begin() + 5, m_rx.begin() + 5 + plen));
                    m_rx.erase(m_rx.begin(), m_rx.begin() + 5 + plen);
                }
            }
        }
        // Callbacks outside the lock.
        if (firePairedFlush(firePaired)) {}
        if (firePaired && m_onState) m_onState(true);
        if (fireFail && m_onState) m_onState(false);
        if (m_onRecv) for (auto& d : deliver) m_onRecv(d.first, d.second.data(), d.second.size());
    }

    // Flush queued sends right after pairing (under lock), returns true always.
    bool firePairedFlush(bool paired) {
        if (!paired) return false;
        std::lock_guard<std::mutex> lk(m_mutex);
        if (!m_pending.empty()) { rawSend(m_pending.data(), m_pending.size()); m_pending.clear(); }
        return true;
    }

    dispatch_queue_t m_queue = nullptr;
    nw_connection_t  m_stream = nullptr;
    std::string m_relayHost, m_deviceId, m_token, m_secret;
    std::string m_ticket; // in: presented on connect; out: refreshed after Paired
    std::string m_peerIp; // out: partner's relay-observed public endpoint
    uint16_t    m_peerPort = 0;
    uint16_t    m_relayPort = 7100;
    bool        m_isVda = false;
    bool        m_paired = false;
    std::vector<uint8_t> m_rx;       // receive reassembly
    std::vector<uint8_t> m_pending;  // queued sends before paired
    mutable std::mutex  m_mutex;
    RecvCb m_onRecv;
    StateCb m_onState;
};

} // namespace

std::unique_ptr<ITransport> CreateRelayTransport() {
    return std::make_unique<RelayTransport>();
}

} // namespace nebula
