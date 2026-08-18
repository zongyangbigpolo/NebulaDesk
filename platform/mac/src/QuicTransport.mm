//
// QuicTransport.mm - Network.framework QUIC transport (MRR, no ARC)
//
// Model: each logical channel (Control / Video / Audio) is carried on its own
// client-initiated QUIC connection. QUIC connections are bidirectional, so the
// server streams media back on the Video/Audio connections the client opened.
// Independent connections mean video / audio / control never head-of-line block
// each other. TLS uses a pre-shared key so no certificate provisioning is needed.
//
#include "Transport.h"
#include "NebulaLog.h"
#include "NebulaIdentityP12.h"

#import <Network/Network.h>
#import <Security/Security.h>

#include <map>
#include <mutex>
#include <condition_variable>
#include <chrono>
#include <string>
#include <vector>

#define TAG "quic"

namespace nebula {

namespace {

dispatch_data_t MakeData(const void* bytes, size_t len) {
    return dispatch_data_create(bytes, len, dispatch_get_global_queue(0, 0),
                                DISPATCH_DATA_DESTRUCTOR_DEFAULT);
}

std::vector<uint8_t> Flatten(dispatch_data_t data) {
    std::vector<uint8_t> out;
    if (!data) return out;
    const void* ptr = nullptr;
    size_t size = 0;
    dispatch_data_t mapped = dispatch_data_create_map(data, &ptr, &size);
    out.assign(static_cast<const uint8_t*>(ptr),
               static_cast<const uint8_t*>(ptr) + size);
    if (mapped) dispatch_release(mapped);
    return out;
}

// Load the embedded self-signed identity for the QUIC server's TLS credential.
sec_identity_t LoadServerIdentity() {
    CFDataRef p12 = CFDataCreate(nullptr, kNebulaIdentityP12, (CFIndex)kNebulaIdentityP12Len);
    const void* keys[] = { kSecImportExportPassphrase };
    const UniChar passwordChars[] = { 98, 97, 103, 97, 100, 101, 109, 111 };
    CFStringRef password = CFStringCreateWithCharacters(
        nullptr, passwordChars, sizeof(passwordChars) / sizeof(passwordChars[0]));
    const void* vals[] = { password };
    CFDictionaryRef opts = CFDictionaryCreate(nullptr, keys, vals, 1, nullptr, nullptr);
    CFArrayRef items = nullptr;
    OSStatus st = SecPKCS12Import(p12, opts, &items);
    sec_identity_t result = nullptr;
    if (st == errSecSuccess && items && CFArrayGetCount(items) > 0) {
        CFDictionaryRef item = (CFDictionaryRef)CFArrayGetValueAtIndex(items, 0);
        SecIdentityRef secIdentity =
            (SecIdentityRef)CFDictionaryGetValue(item, kSecImportItemIdentity);
        if (secIdentity) result = sec_identity_create(secIdentity);
    } else {
        NEBULA_LOGE(TAG, "SecPKCS12Import failed: %d", (int)st);
    }
    if (items) CFRelease(items);
    CFRelease(opts);
    CFRelease(password);
    CFRelease(p12);
    return result;
}

class QuicTransport final : public ITransport {
public:
    QuicTransport() {
        m_queue = dispatch_queue_create("com.nebula.quic", DISPATCH_QUEUE_SERIAL);
    }
    ~QuicTransport() override {
        close();
        if (m_queue) dispatch_release(m_queue);
    }

    void setSharedSecret(const std::string& key) override { m_psk = key; }
    void setOnReceive(RecvCb cb) override { m_onRecv = std::move(cb); }
    void setOnState(StateCb cb) override { m_onState = std::move(cb); }

    bool startListener(uint16_t port) override {
        m_isServer = true;
        {
            std::lock_guard<std::mutex> lk(m_listenerStateMutex);
            m_listenerStartupSettled = false;
            m_listenerReady = false;
        }

        nw_parameters_t params = makeParams();
        if (!params) return false;

        char portStr[8];
        snprintf(portStr, sizeof(portStr), "%u", port);
        m_listener = nw_listener_create_with_port(portStr, params);
        nw_release(params);
        if (!m_listener) { NEBULA_LOGE(TAG, "create listener failed on %u", port); return false; }

        nw_listener_set_queue(m_listener, m_queue);
        nw_listener_set_new_connection_handler(m_listener, ^(nw_connection_t conn) {
            acceptInbound(conn);
        });
        nw_listener_set_state_changed_handler(m_listener, ^(nw_listener_state_t st, nw_error_t err) {
            if (st == nw_listener_state_ready) {
                NEBULA_LOGI(TAG, "listener ready");
                markListenerStartup(true);
            }
            if (st == nw_listener_state_failed) {
                NEBULA_LOGE(TAG, "listener failed err=%ld",
                          err ? (long)nw_error_get_error_code(err) : 0);
                markListenerStartup(false);
                notifyState(false);
            }
        });
        nw_listener_start(m_listener);
        NEBULA_LOGI(TAG, "listening on port %u", port);

        std::unique_lock<std::mutex> lk(m_listenerStateMutex);
        m_listenerStateCv.wait_for(lk, std::chrono::seconds(2), [this] {
            return m_listenerStartupSettled;
        });
        if (m_listenerStartupSettled && !m_listenerReady) {
            lk.unlock();
            close();
            return false;
        }
        return true;
    }

    bool connect(const std::string& host, uint16_t port) override {
        m_isServer = false;
        m_host = host;
        m_port = port;
        NEBULA_LOGI(TAG, "client ready to open channels to %s:%u", host.c_str(), port);
        // Open all three channels up front. QUIC connections are bidirectional,
        // so the server streams Video/Audio back on the connections we open here.
        std::lock_guard<std::mutex> lk(m_mutex);
        outboundConnLocked(Channel::Control);
        outboundConnLocked(Channel::Video);
        outboundConnLocked(Channel::Audio);
        return true;
    }

    bool send(Channel ch, const uint8_t* data, size_t len) override {
        std::lock_guard<std::mutex> lk(m_mutex);
        // Fast path: the channel connection exists (or the client can open it
        // now) and nothing is buffered -> send directly, avoiding a copy into
        // m_pending on the per-frame video/audio hot path.
        nw_connection_t c = nullptr;
        auto it = m_conns.find((uint32_t)ch);
        if (it != m_conns.end()) c = it->second;
        else if (!m_isServer) c = outboundConnLocked(ch);
        if (c && m_pending[(uint32_t)ch].empty()) {
            sendOnLocked(c, ch, data, len);
            return true;
        }
        // Slow path: buffer until the channel connection exists (server pre-accept).
        auto& pend = m_pending[(uint32_t)ch];
        pend.insert(pend.end(), data, data + len);
        flushLocked(ch);
        return true;
    }

    void close() override {
        std::lock_guard<std::mutex> lk(m_mutex);
        for (auto& kv : m_conns) { nw_connection_cancel(kv.second); nw_release(kv.second); }
        m_conns.clear();
        if (m_listener) { nw_listener_cancel(m_listener); nw_release(m_listener); m_listener = nullptr; }
    }

private:
    nw_parameters_t makeParams() {
        const bool server = m_isServer;
        return nw_parameters_create_quic(^(nw_protocol_options_t quic) {
            nw_quic_add_tls_application_protocol(quic, "nebula");
            nw_quic_set_initial_max_data(quic, 16 * 1024 * 1024);
            nw_quic_set_initial_max_streams_bidirectional(quic, 4);
            nw_quic_set_initial_max_streams_unidirectional(quic, 4);
            nw_quic_set_initial_max_stream_data_bidirectional_local(quic, 8 * 1024 * 1024);
            nw_quic_set_initial_max_stream_data_bidirectional_remote(quic, 8 * 1024 * 1024);
            nw_quic_set_initial_max_stream_data_unidirectional(quic, 8 * 1024 * 1024);
            nw_quic_set_idle_timeout(quic, 30000);

            sec_protocol_options_t sec = nw_quic_copy_sec_protocol_options(quic);
            sec_protocol_options_set_min_tls_protocol_version(sec, tls_protocol_version_TLSv13);
            if (server) {
                sec_identity_t identity = LoadServerIdentity();
                if (identity) { sec_protocol_options_set_local_identity(sec, identity); NEBULA_LOGI(TAG, "server identity loaded"); }
                else NEBULA_LOGE(TAG, "server identity NULL - TLS will fail");
            } else {
                // Dev: trust the self-signed server certificate.
                sec_protocol_options_set_verify_block(sec,
                    ^(sec_protocol_metadata_t, sec_trust_t, sec_protocol_verify_complete_t complete) {
                        complete(true);
                    }, dispatch_get_global_queue(0, 0));
            }
            nw_release(sec);
        });
    }

    void notifyState(bool connected) {
        // Channels each report ready/failed; collapse to a single transition so
        // higher layers see one connected/disconnected edge (runs on m_queue).
        if (connected == m_connected) return;
        m_connected = connected;
        if (m_onState) m_onState(connected);
    }

    void markListenerStartup(bool ready) {
        {
            std::lock_guard<std::mutex> lk(m_listenerStateMutex);
            if (m_listenerStartupSettled) return;
            m_listenerReady = ready;
            m_listenerStartupSettled = true;
        }
        m_listenerStateCv.notify_all();
    }

    // Send raw bytes on an established connection. Assumes m_mutex held.
    void sendOnLocked(nw_connection_t c, Channel ch, const uint8_t* data, size_t len) {
        dispatch_data_t dd = MakeData(data, len);
        nw_connection_send(c, dd, NW_CONNECTION_DEFAULT_MESSAGE_CONTEXT, false, ^(nw_error_t err) {
            if (err) NEBULA_LOGW(TAG, "send error on channel %u", (uint32_t)ch);
        });
        dispatch_release(dd);
    }

    // --- Server: accept an inbound channel connection -----------------------
    void acceptInbound(nw_connection_t conn) {
        nw_retain(conn);
        nw_connection_set_queue(conn, m_queue);
        nw_connection_set_state_changed_handler(conn, ^(nw_connection_state_t st, nw_error_t) {
            if (st == nw_connection_state_ready)  notifyState(true);
            if (st == nw_connection_state_failed) notifyState(false);
        });
        nw_connection_start(conn);
        // Read the 4-byte channel preamble, then loop.
        nw_connection_receive(conn, 4, 4,
            ^(dispatch_data_t content, nw_content_context_t, bool, nw_error_t err) {
            if (err || !content) { nw_connection_cancel(conn); nw_release(conn); return; }
            std::vector<uint8_t> pre = Flatten(content);
            uint32_t ch = 0;
            for (int i = 0; i < 4 && i < (int)pre.size(); ++i) ch |= (uint32_t)pre[i] << (i * 8);
            {
                std::lock_guard<std::mutex> lk(m_mutex);
                if (m_conns.count(ch)) { nw_connection_cancel(m_conns[ch]); nw_release(m_conns[ch]); }
                nw_retain(conn);    // map holds its own reference, independent of receiveLoop's
                m_conns[ch] = conn;
                flushLocked((Channel)ch);
            }
            NEBULA_LOGI(TAG, "inbound channel %u established", ch);
            receiveLoop(conn, (Channel)ch);
        });
    }

    // --- Client: create an outbound channel connection ----------------------
    nw_connection_t outboundConnLocked(Channel ch) {
        auto it = m_conns.find((uint32_t)ch);
        if (it != m_conns.end()) return it->second;

        nw_parameters_t params = makeParams();
        if (!params) return nullptr;
        char portStr[8];
        snprintf(portStr, sizeof(portStr), "%u", m_port);
        nw_endpoint_t ep = nw_endpoint_create_host(m_host.c_str(), portStr);
        nw_connection_t conn = nw_connection_create(ep, params);
        nw_release(ep);
        nw_release(params);
        if (!conn) return nullptr;
        nw_retain(conn);
        nw_connection_set_queue(conn, m_queue);
        nw_connection_set_state_changed_handler(conn, ^(nw_connection_state_t st, nw_error_t err) {
            if (st == nw_connection_state_ready)  { NEBULA_LOGI(TAG, "channel %u ready", (uint32_t)ch); notifyState(true); }
            if (st == nw_connection_state_waiting) NEBULA_LOGW(TAG, "channel %u waiting err=%ld", (uint32_t)ch, err ? (long)nw_error_get_error_code(err) : 0);
            if (st == nw_connection_state_failed) { NEBULA_LOGE(TAG, "channel %u failed err=%ld", (uint32_t)ch, err ? (long)nw_error_get_error_code(err) : 0); notifyState(false); }
        });
        nw_connection_start(conn);

        // Send 4-byte channel preamble so the server can route this connection.
        uint8_t pre[4];
        uint32_t id = (uint32_t)ch;
        for (int i = 0; i < 4; ++i) pre[i] = (id >> (i * 8)) & 0xff;
        dispatch_data_t dd = MakeData(pre, sizeof(pre));
        nw_connection_send(conn, dd, NW_CONNECTION_DEFAULT_MESSAGE_CONTEXT, false, ^(nw_error_t){});
        dispatch_release(dd);

        m_conns[(uint32_t)ch] = conn;
        receiveLoop(conn, ch);
        return conn;
    }

    void receiveLoop(nw_connection_t c, Channel ch) {
        nw_connection_receive(c, 1, 262144,
            ^(dispatch_data_t content, nw_content_context_t, bool isComplete, nw_error_t err) {
            if (content) {
                std::vector<uint8_t> bytes = Flatten(content);
                if (!bytes.empty() && m_onRecv) m_onRecv(ch, bytes.data(), bytes.size());
            }
            if (err || isComplete) { nw_connection_cancel(c); nw_release(c); return; }
            receiveLoop(c, ch);
        });
    }

    // Flush buffered bytes for a channel. Assumes m_mutex held.
    void flushLocked(Channel ch) {
        auto& pend = m_pending[(uint32_t)ch];
        if (pend.empty()) return;
        nw_connection_t c = nullptr;
        auto it = m_conns.find((uint32_t)ch);
        if (it != m_conns.end()) c = it->second;
        else if (!m_isServer) c = outboundConnLocked(ch); // client opens lazily
        if (!c) return; // server: wait until the client opens this channel
        sendOnLocked(c, ch, pend.data(), pend.size());
        pend.clear();
    }

    dispatch_queue_t m_queue    = nullptr;
    nw_listener_t    m_listener = nullptr;
    bool             m_isServer = false;
    std::string      m_host;
    uint16_t         m_port = 0;

    std::map<uint32_t, nw_connection_t>      m_conns;
    std::map<uint32_t, std::vector<uint8_t>> m_pending;
    std::mutex       m_mutex;
    std::mutex       m_listenerStateMutex;
    std::condition_variable m_listenerStateCv;
    bool             m_listenerStartupSettled = false;
    bool             m_listenerReady = false;
    bool             m_connected = false;
    std::string      m_psk;
    RecvCb           m_onRecv;
    StateCb          m_onState;
};

} // namespace

// Implemented in RelayTransport.mm / UpgradingTransport.mm.
std::unique_ptr<ITransport> CreateRelayTransport();
std::unique_ptr<ITransport> CreateUpgradingTransport();

std::unique_ptr<ITransport> CreateTransport(TransportType type) {
    switch (type) {
        case TransportType::Relay:
            // Relay mode auto-upgrades to a direct path when possible.
            return CreateUpgradingTransport();
        case TransportType::Quic:
        case TransportType::IceQuic: // not yet implemented -> fall back to direct QUIC
        default:
            return std::make_unique<QuicTransport>();
    }
}

} // namespace nebula
