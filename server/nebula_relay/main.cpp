//
// main.cpp - nebula_relay: a blind-forwarding QUIC relay (msquic).
//
// Role: bridge a VDA and a CWA that cannot reach each other directly.
//   1. Each peer connects to the relay and opens one bidirectional stream.
//   2. The peer's first bytes are a RelayHello (role + device-id + token).
//   3. A VDA registers its device-id and waits; a CWA targets a device-id.
//   4. Once paired, the relay blind-forwards the byte stream both ways.
//
#include "RelayProtocol.h"

#include <msquic.h>
#include <curl/curl.h>

#include <arpa/inet.h>

#include <csignal>
#include <pthread.h>

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <random>
#include <string>
#include <thread>
#include <vector>

#define RLOG(...) do { std::fprintf(stderr, "[relay] " __VA_ARGS__); std::fprintf(stderr, "\n"); } while (0)

static const QUIC_API_TABLE* MsQuic = nullptr;
static HQUIC gRegistration = nullptr;
static HQUIC gConfiguration = nullptr;

// Optional external SaaS control-plane integration (see server/nebula_cloud).
// When set, a CWA's presented token is authorized via an HTTP callback
// instead of a local static-token compare — see SaasAuthorize() below.
static std::string gSaasAuthUrl;
static std::string gSaasAuthSecret;

// Optional: report registered VDAs to the SaaS control plane's
// /internal/heartbeat (see server/nebula_cloud) so its `online`/`lastSeenAt`
// reflects the QUIC-path relay session too — previously only the WebRTC
// signaling path ever refreshed that. Reuses gSaasAuthSecret as the shared
// secret (same cloud instance, same X-Relay-Secret contract). Empty = off.
static std::string gSaasHeartbeatUrl;
static int gSaasHeartbeatIntervalSecs = 30;

// ALPN must match what the client uses for relay connections.
static const QUIC_BUFFER kAlpn = { sizeof("nebula-relay") - 1, (uint8_t*)"nebula-relay" };

namespace {

using Clock = std::chrono::steady_clock;

// How long a still-connected CWA is held (without being torn down) waiting
// for its VDA to reconnect after a network blip, and how long an issued
// reconnect ticket remains valid.
constexpr auto kVdaReattachGrace = std::chrono::seconds(30);
constexpr auto kTicketValidity   = std::chrono::minutes(10);

struct PeerLink;

// Registry of VDAs that have registered. A VDA stays here for as long as its
// connection is alive — pairing a viewer no longer consumes the entry — so a
// second (or Nth) CWA can always find and connect to the same device without
// requiring a VDA restart. `partner` (on the PeerLink) tracks who, if anyone,
// is the CURRENT viewer; a new CWA targeting an already-paired VDA supersedes
// the previous viewer rather than being rejected.
std::mutex gMutex;
std::map<std::string, PeerLink*> gWaitingVdas; // deviceId -> registered VDA link

// A CWA left waiting, still connected, because its VDA dropped and might
// reconnect shortly (e.g. the VDA process restarted or had a brief network
// blip). Reattached automatically if the VDA re-registers within the grace
// window; torn down for real once the deadline passes.
struct PendingReattach {
    PeerLink* cwa = nullptr;
    Clock::time_point deadline;
};
std::map<std::string, PendingReattach> gPendingReattach; // deviceId -> pending CWA

// Short-lived reconnect tickets issued after a successful token-based pairing.
// A CWA that presents a still-valid ticket for the right device is authorized
// without needing to resend the long-lived token, so a client-side network
// blip doesn't require the token to be re-entered/re-derived.
struct TicketRecord {
    std::string deviceId;
    Clock::time_point expiry;
};
std::map<std::string, TicketRecord> gTickets; // ticket (hex) -> record

// Per-stream context. One PeerLink per peer (VDA or CWA).
struct PeerLink {
    HQUIC connection = nullptr;
    HQUIC stream     = nullptr;
    nebula::RelayRole role{};
    std::string deviceId;
    std::string token;
    std::vector<uint8_t> helloBuf;     // accumulates until kRelayHelloSize
    bool helloDone = false;
    PeerLink* partner = nullptr;        // the bridged peer once paired
    std::atomic<bool> closed{false};
    // This peer's public ip:port as observed directly off its QUIC connection
    // to us (see GetRemoteAddr) — handed to the *other* side of a pairing as
    // a free NAT-punch candidate, no external STUN server involved.
    std::string observedIp;
    uint16_t    observedPort = 0;
};

void SendStatus(PeerLink* link, nebula::RelayStatus st);
void ForwardToPartner(PeerLink* from, const uint8_t* data, size_t len);
void TeardownPair(PeerLink* link);
void CloseConnectionSoon(PeerLink* link);

// Send raw bytes on a peer's stream. Copies into a heap buffer freed on SEND_COMPLETE.
void StreamSendCopy(HQUIC stream, const uint8_t* data, size_t len) {
    if (!stream || !data || !len) return;
    // Layout: [QUIC_BUFFER][payload] in one allocation.
    auto* buf = (QUIC_BUFFER*)malloc(sizeof(QUIC_BUFFER) + len);
    if (!buf) return;
    buf->Buffer = (uint8_t*)(buf + 1);
    buf->Length = (uint32_t)len;
    memcpy(buf->Buffer, data, len);
    if (QUIC_FAILED(MsQuic->StreamSend(stream, buf, 1, QUIC_SEND_FLAG_NONE, buf))) {
        free(buf);
    }
}

// Reads the remote (public, as seen by us) ip:port off a live QUIC connection.
// This is the relay's free substitute for an external STUN server: whatever
// address this peer's packets actually arrived from IS its NAT-mapped public
// endpoint, no extra round trip needed. Returns false if the connection has
// no usable address yet (shouldn't normally happen post-handshake).
bool GetRemoteAddr(HQUIC connection, std::string& outIp, uint16_t& outPort) {
    QUIC_ADDR addr{};
    uint32_t size = sizeof(addr);
    if (QUIC_FAILED(MsQuic->GetParam(connection, QUIC_PARAM_CONN_REMOTE_ADDRESS, &size, &addr))) {
        return false;
    }
    char buf[INET6_ADDRSTRLEN] = {0};
    if (addr.Ip.sa_family == QUIC_ADDRESS_FAMILY_INET) {
        if (!inet_ntop(AF_INET, &addr.Ipv4.sin_addr, buf, sizeof(buf))) return false;
    } else if (addr.Ip.sa_family == QUIC_ADDRESS_FAMILY_INET6) {
        if (!inet_ntop(AF_INET6, &addr.Ipv6.sin6_addr, buf, sizeof(buf))) return false;
    } else {
        return false;
    }
    outIp = buf;
    outPort = QuicAddrGetPort(&addr);
    return true;
}

std::string RandomHex(size_t bytes) {
    static thread_local std::mt19937_64 rng{std::random_device{}()};
    std::string out;
    out.reserve(bytes * 2);
    static const char* hex = "0123456789abcdef";
    for (size_t i = 0; i < bytes; ++i) {
        uint8_t b = (uint8_t)(rng() & 0xff);
        out.push_back(hex[b >> 4]);
        out.push_back(hex[b & 0xf]);
    }
    return out;
}

// Issue+store a fresh reconnect ticket for `deviceId`, returning its raw
// string form (fits in RelayHello.ticket / kTicketLen bytes as hex text).
std::string IssueTicket(const std::string& deviceId) {
    std::lock_guard<std::mutex> lk(gMutex);
    std::string ticket = RandomHex(nebula::kTicketLen / 2);
    gTickets[ticket] = TicketRecord{deviceId, Clock::now() + kTicketValidity};
    return ticket;
}

// Validate a presented ticket against `deviceId`. Consumes nothing (tickets
// may be reused until they expire — reconnect can happen more than once).
bool TicketValid(const std::string& ticket, const std::string& deviceId) {
    if (ticket.empty()) return false;
    std::lock_guard<std::mutex> lk(gMutex);
    auto it = gTickets.find(ticket);
    if (it == gTickets.end()) return false;
    if (it->second.deviceId != deviceId) return false;
    if (Clock::now() > it->second.expiry) { gTickets.erase(it); return false; }
    return true;
}

std::string JsonEscape(const std::string& s) {
    std::string out;
    out.reserve(s.size());
    for (char c : s) {
        if (c == '"' || c == '\\') out.push_back('\\');
        out.push_back(c);
    }
    return out;
}

size_t CurlWriteToString(char* ptr, size_t size, size_t nmemb, void* userdata) {
    auto* out = (std::string*)userdata;
    out->append(ptr, size * nmemb);
    return size * nmemb;
}

// Ask an external SaaS control plane (see server/nebula_cloud) whether a
// presented token authorizes pairing to `deviceId`. `token` here is whatever
// the CWA put in RelayHello.token — in SaaS mode this is a short-lived JWT
// issued by the control plane, NOT a static shared secret. Blocking with a
// short timeout: this only runs once per connection attempt (not on the
// media hot path), so a bounded stall here is an acceptable trade-off for
// keeping the relay's own logic simple (no separate async HTTP executor).
bool SaasAuthorize(const std::string& deviceId, const std::string& token) {
    CURL* curl = curl_easy_init();
    if (!curl) return false;

    std::string body = "{\"deviceId\":\"" + JsonEscape(deviceId) + "\",\"token\":\"" + JsonEscape(token) + "\"}";
    std::string response;
    struct curl_slist* headers = nullptr;
    headers = curl_slist_append(headers, "Content-Type: application/json");
    std::string secretHeader = "X-Relay-Secret: " + gSaasAuthSecret;
    headers = curl_slist_append(headers, secretHeader.c_str());

    curl_easy_setopt(curl, CURLOPT_URL, gSaasAuthUrl.c_str());
    curl_easy_setopt(curl, CURLOPT_POST, 1L);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body.c_str());
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, CurlWriteToString);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA, &response);
    curl_easy_setopt(curl, CURLOPT_TIMEOUT_MS, 3000L);
    curl_easy_setopt(curl, CURLOPT_CONNECTTIMEOUT_MS, 1500L);

    CURLcode rc = curl_easy_perform(curl);
    long httpCode = 0;
    curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &httpCode);
    curl_slist_free_all(headers);
    curl_easy_cleanup(curl);

    if (rc != CURLE_OK) {
        RLOG("SaaS authorize call failed: %s", curl_easy_strerror(rc));
        return false;
    }
    if (httpCode == 401) {
        RLOG("SaaS authorize rejected our --saas-auth-secret (401) — check configuration");
        return false;
    }
    if (httpCode != 200) {
        RLOG("SaaS authorize returned unexpected HTTP %ld", httpCode);
        return false;
    }
    // Minimal, tolerant parse of {"authorized": true|false, ...} — avoids a
    // full JSON dependency for a single boolean field in a controlled,
    // internal, self-documented contract (see server/nebula_cloud/README.md).
    auto pos = response.find("\"authorized\"");
    if (pos == std::string::npos) { RLOG("SaaS authorize response missing 'authorized' field"); return false; }
    auto truePos = response.find("true", pos);
    auto falsePos = response.find("false", pos);
    return truePos != std::string::npos && (falsePos == std::string::npos || truePos < falsePos);
}

// Reports a registered VDA as alive to the SaaS control plane (see
// server/nebula_cloud's POST /internal/heartbeat), keyed by relayDeviceId —
// the only identifier the relay itself ever has. Fire-and-forget: a failed
// or unreachable call just logs and moves on, it never affects the relay's
// own bridging/pairing behavior (heartbeat is purely for the cloud's
// online/lastSeenAt display, not an authorization decision).
void SendHeartbeat(const std::string& deviceId) {
    CURL* curl = curl_easy_init();
    if (!curl) return;

    std::string body = "{\"deviceId\":\"" + JsonEscape(deviceId) + "\"}";
    std::string response;
    struct curl_slist* headers = nullptr;
    headers = curl_slist_append(headers, "Content-Type: application/json");
    std::string secretHeader = "X-Relay-Secret: " + gSaasAuthSecret;
    headers = curl_slist_append(headers, secretHeader.c_str());

    curl_easy_setopt(curl, CURLOPT_URL, gSaasHeartbeatUrl.c_str());
    curl_easy_setopt(curl, CURLOPT_POST, 1L);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body.c_str());
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, CurlWriteToString);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA, &response);
    curl_easy_setopt(curl, CURLOPT_TIMEOUT_MS, 3000L);
    curl_easy_setopt(curl, CURLOPT_CONNECTTIMEOUT_MS, 1500L);

    CURLcode rc = curl_easy_perform(curl);
    long httpCode = 0;
    curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &httpCode);
    curl_slist_free_all(headers);
    curl_easy_cleanup(curl);

    if (rc != CURLE_OK) {
        RLOG("SaaS heartbeat call failed for device-id=%s: %s", deviceId.c_str(), curl_easy_strerror(rc));
        return;
    }
    if (httpCode == 401) {
        RLOG("SaaS heartbeat rejected our --saas-auth-secret (401) — check configuration");
        return;
    }
    if (httpCode == 404) {
        RLOG("SaaS heartbeat: cloud doesn't know device-id=%s (deleted?)", deviceId.c_str());
        return;
    }
    if (httpCode != 200) {
        RLOG("SaaS heartbeat returned unexpected HTTP %ld for device-id=%s", httpCode, deviceId.c_str());
    }
}

// Send a Paired status to a VDA, followed by a fixed-size PeerAddr block
// carrying the CWA's relay-observed public endpoint (see RelayProtocol.h) —
// the VDA's free NAT-punch candidate for that viewer.
void SendPairedToVda(PeerLink* vda, const std::string& cwaIp, uint16_t cwaPort) {
    std::vector<uint8_t> reply;
    reply.push_back((uint8_t)nebula::RelayStatus::Paired);
    auto addr = nebula::BuildPeerAddr(cwaIp, cwaPort);
    reply.insert(reply.end(), addr.begin(), addr.end());
    StreamSendCopy(vda->stream, reply.data(), reply.size());
}

// Send a Paired status to a CWA, followed by the VDA's relay-observed
// PeerAddr block and then a fresh reconnect ticket — see
// RelayStatus::Paired's documentation in RelayProtocol.h.
void SendPairedToCwa(PeerLink* cwa, const std::string& deviceId,
                      const std::string& vdaIp, uint16_t vdaPort) {
    std::string ticket = IssueTicket(deviceId);
    std::vector<uint8_t> reply;
    reply.push_back((uint8_t)nebula::RelayStatus::Paired);
    auto addr = nebula::BuildPeerAddr(vdaIp, vdaPort);
    reply.insert(reply.end(), addr.begin(), addr.end());
    reply.resize(reply.size() + nebula::kTicketLen, 0);
    std::memcpy(reply.data() + reply.size() - nebula::kTicketLen, ticket.data(),
                std::min(ticket.size(), nebula::kTicketLen));
    StreamSendCopy(cwa->stream, reply.data(), reply.size());
}

void HandleHello(PeerLink* link) {
    nebula::RelayHello h{};
    if (!nebula::ParseRelayHello(link->helloBuf.data(), link->helloBuf.size(), h)) {
        SendStatus(link, nebula::RelayStatus::BadHello);
        return;
    }
    link->role     = (nebula::RelayRole)h.role;
    link->deviceId = nebula::ReadFixed(h.deviceId, nebula::kDeviceIdLen);
    link->token    = nebula::ReadFixed(h.token, nebula::kTokenLen);
    std::string presentedTicket = nebula::ReadFixed(h.ticket, nebula::kTicketLen);
    // Free NAT-punch candidate: whatever address this hello actually arrived
    // from IS this peer's NAT-mapped public endpoint. No STUN round trip.
    if (!GetRemoteAddr(link->connection, link->observedIp, link->observedPort)) {
        RLOG("device-id=%s: could not read observed remote address (no punch candidate)",
             link->deviceId.c_str());
    }

    if (link->role == nebula::RelayRole::Vda) {
        PendingReattach pending{};
        bool hasPending = false;
        {
            std::lock_guard<std::mutex> lk(gMutex);
            gWaitingVdas[link->deviceId] = link;
            auto it = gPendingReattach.find(link->deviceId);
            if (it != gPendingReattach.end() && !it->second.cwa->closed.load()) {
                pending = it->second;
                hasPending = true;
                gPendingReattach.erase(it);
            }
        }
        RLOG("VDA registered device-id=%s (observed %s:%u)", link->deviceId.c_str(),
             link->observedIp.c_str(), link->observedPort);
        SendStatus(link, nebula::RelayStatus::Registered);
        if (!gSaasHeartbeatUrl.empty()) {
            // Report immediately on (re)registration rather than waiting for
            // the next periodic tick, so the cloud's "online" flag flips
            // promptly instead of lagging by up to gSaasHeartbeatIntervalSecs.
            std::thread(SendHeartbeat, link->deviceId).detach();
        }
        if (hasPending) {
            // The VDA reconnected within the grace window while its previous
            // viewer was still waiting — silently resume the same session
            // instead of forcing the CWA to redo the whole handshake.
            link->partner = pending.cwa;
            pending.cwa->partner = link;
            RLOG("device-id=%s: VDA reattached to waiting viewer", link->deviceId.c_str());
            SendPairedToVda(link, pending.cwa->observedIp, pending.cwa->observedPort);
            SendPairedToCwa(pending.cwa, link->deviceId, link->observedIp, link->observedPort);
        }
        return;
    }

    // CWA: find the registered VDA and pair. Authorization is layered:
    // 1) a still-valid local reconnect ticket (fast path, no network call);
    // 2) if configured, an external SaaS control plane's live yes/no
    //    (server/nebula_cloud's /internal/authorize — revocable, auditable);
    // 3) otherwise, the plain static-token compare (self-hosted/dev mode).
    PeerLink* vda = nullptr;
    {
        std::lock_guard<std::mutex> lk(gMutex);
        auto it = gWaitingVdas.find(link->deviceId);
        if (it != gWaitingVdas.end()) vda = it->second;
    }
    if (!vda) {
        RLOG("CWA targeted unknown/offline device-id=%s", link->deviceId.c_str());
        SendStatus(link, nebula::RelayStatus::NoSuchDevice);
        return;
    }
    bool authorized = TicketValid(presentedTicket, link->deviceId);
    if (!authorized) {
        authorized = !gSaasAuthUrl.empty() ? SaasAuthorize(link->deviceId, link->token)
                                           : (vda->token == link->token);
    }
    if (!authorized) {
        RLOG("CWA auth failed for device-id=%s (bad token/ticket%s)", link->deviceId.c_str(),
             gSaasAuthUrl.empty() ? "" : "/SaaS denied");
        SendStatus(link, nebula::RelayStatus::BadToken);
        return;
    }

    // A viewer is already attached: this new CWA takes over (e.g. the user
    // reconnected from a new window, or a different authorized viewer is
    // taking a turn). Tell the old one it was superseded and close it — do
    // NOT tear down the VDA side, whose registration is long-lived now.
    PeerLink* previousCwa = vda->partner;
    if (previousCwa && !previousCwa->closed.load()) {
        RLOG("device-id=%s: new viewer supersedes previous one", link->deviceId.c_str());
        SendStatus(previousCwa, nebula::RelayStatus::Superseded);
        previousCwa->partner = nullptr;
        CloseConnectionSoon(previousCwa);
    }

    link->partner = vda;
    vda->partner  = link;
    RLOG("paired device-id=%s (bridging; VDA observed %s:%u, CWA observed %s:%u)",
         link->deviceId.c_str(), vda->observedIp.c_str(), vda->observedPort,
         link->observedIp.c_str(), link->observedPort);
    SendPairedToVda(vda, link->observedIp, link->observedPort);
    SendPairedToCwa(link, link->deviceId, vda->observedIp, vda->observedPort);
}

void SendStatus(PeerLink* link, nebula::RelayStatus st) {
    uint8_t b = (uint8_t)st;
    StreamSendCopy(link->stream, &b, 1);
}

void ForwardToPartner(PeerLink* from, const uint8_t* data, size_t len) {
    PeerLink* to = from->partner;
    if (to && !to->closed.load()) StreamSendCopy(to->stream, data, len);
}

// Process received bytes: accumulate hello first, then blind-forward the rest.
void OnStreamData(PeerLink* link, const uint8_t* data, size_t len) {
    size_t off = 0;
    if (!link->helloDone) {
        size_t need = nebula::kRelayHelloSize - link->helloBuf.size();
        size_t take = len < need ? len : need;
        link->helloBuf.insert(link->helloBuf.end(), data, data + take);
        off += take;
        if (link->helloBuf.size() < nebula::kRelayHelloSize) return; // wait for more
        link->helloDone = true;
        HandleHello(link);
    }
    if (off < len) ForwardToPartner(link, data + off, len - off);
}

void CloseConnectionSoon(PeerLink* link) {
    if (link->closed.exchange(true)) return;
    MsQuic->ConnectionShutdown(link->connection, QUIC_CONNECTION_SHUTDOWN_FLAG_NONE, 0);
}

void TeardownPair(PeerLink* link) {
    if (link->closed.exchange(true)) return;

    if (link->role == nebula::RelayRole::Vda) {
        std::lock_guard<std::mutex> lk(gMutex);
        auto it = gWaitingVdas.find(link->deviceId);
        if (it != gWaitingVdas.end() && it->second == link) gWaitingVdas.erase(it);

        PeerLink* cwa = link->partner;
        if (cwa && !cwa->closed.load() && !link->deviceId.empty()) {
            // Don't drop the viewer immediately: the VDA may just be
            // restarting (crash/relaunch, brief network loss). Hold the CWA
            // connected and give the VDA a grace window to reattach. This
            // VDA PeerLink is about to be deleted (its own connection really
            // did go away), so the CWA must not keep pointing at it.
            cwa->partner = nullptr;
            RLOG("device-id=%s: VDA link lost, holding viewer for %llds in case it reconnects",
                 link->deviceId.c_str(), (long long)kVdaReattachGrace.count());
            gPendingReattach[link->deviceId] = PendingReattach{cwa, Clock::now() + kVdaReattachGrace};
            return; // do NOT shut down the CWA's connection.
        }
    } else {
        // A CWA disconnecting cleanly (not superseded) just detaches from its
        // VDA; the VDA registration itself is unaffected and stays waiting.
        if (link->partner && link->partner->partner == link) link->partner->partner = nullptr;
        // If this CWA was the one being held in gPendingReattach, drop the
        // entry now — its PeerLink is about to be deleted (own connection
        // teardown), so nothing must keep referencing it afterward.
        std::lock_guard<std::mutex> lk(gMutex);
        auto it = gPendingReattach.find(link->deviceId);
        if (it != gPendingReattach.end() && it->second.cwa == link) gPendingReattach.erase(it);
    }
}

// Background reaper: expires stale pending-reattach entries (VDA never came
// back) and stale tickets. Runs for the life of the process.
void ReaperLoop() {
    for (;;) {
        std::this_thread::sleep_for(std::chrono::seconds(5));
        std::vector<PeerLink*> toClose;
        {
            std::lock_guard<std::mutex> lk(gMutex);
            auto now = Clock::now();
            for (auto it = gPendingReattach.begin(); it != gPendingReattach.end();) {
                if (now > it->second.deadline) {
                    if (!it->second.cwa->closed.load()) toClose.push_back(it->second.cwa);
                    it = gPendingReattach.erase(it);
                } else {
                    ++it;
                }
            }
            for (auto it = gTickets.begin(); it != gTickets.end();) {
                if (now > it->second.expiry) it = gTickets.erase(it);
                else ++it;
            }
        }
        for (auto* cwa : toClose) {
            RLOG("device-id=%s: VDA reattach grace period expired, closing viewer",
                 cwa->deviceId.c_str());
            CloseConnectionSoon(cwa);
        }
    }
}

// Background: periodically reports every currently-registered VDA to the
// SaaS control plane's /internal/heartbeat (see SendHeartbeat above), so a
// long-lived connection stays "online" between the one-shot heartbeats fired
// on (re)registration. No-op loop (never started) if --saas-heartbeat-url is
// unset. Each call runs on its own detached thread so one slow/unreachable
// cloud instance can't delay the next tick or block other devices' calls.
void HeartbeatLoop() {
    for (;;) {
        std::this_thread::sleep_for(std::chrono::seconds(gSaasHeartbeatIntervalSecs));
        std::vector<std::string> deviceIds;
        {
            std::lock_guard<std::mutex> lk(gMutex);
            for (auto& [deviceId, link] : gWaitingVdas) deviceIds.push_back(deviceId);
        }
        for (auto& deviceId : deviceIds) std::thread(SendHeartbeat, deviceId).detach();
    }
}

// ----- msquic callbacks ----------------------------------------------------

QUIC_STATUS QUIC_API StreamCallback(HQUIC stream, void* ctx, QUIC_STREAM_EVENT* ev) {
    auto* link = (PeerLink*)ctx;
    switch (ev->Type) {
        case QUIC_STREAM_EVENT_RECEIVE:
            for (uint32_t i = 0; i < ev->RECEIVE.BufferCount; ++i) {
                OnStreamData(link, ev->RECEIVE.Buffers[i].Buffer, ev->RECEIVE.Buffers[i].Length);
            }
            break;
        case QUIC_STREAM_EVENT_SEND_COMPLETE:
            free(ev->SEND_COMPLETE.ClientContext); // the QUIC_BUFFER allocation
            break;
        case QUIC_STREAM_EVENT_PEER_SEND_SHUTDOWN:
        case QUIC_STREAM_EVENT_PEER_SEND_ABORTED:
            TeardownPair(link);
            break;
        case QUIC_STREAM_EVENT_SHUTDOWN_COMPLETE:
            MsQuic->StreamClose(stream);
            break;
        default: break;
    }
    return QUIC_STATUS_SUCCESS;
}

QUIC_STATUS QUIC_API ConnectionCallback(HQUIC conn, void* ctx, QUIC_CONNECTION_EVENT* ev) {
    auto* link = (PeerLink*)ctx;
    switch (ev->Type) {
        case QUIC_CONNECTION_EVENT_CONNECTED:
            RLOG("connection established");
            break;
        case QUIC_CONNECTION_EVENT_PEER_STREAM_STARTED:
            RLOG("peer stream started");
            link->stream = ev->PEER_STREAM_STARTED.Stream;
            MsQuic->SetCallbackHandler(link->stream, (void*)StreamCallback, link);
            break;
        case QUIC_CONNECTION_EVENT_SHUTDOWN_INITIATED_BY_PEER:
            RLOG("conn shutdown by peer");
            TeardownPair(link);
            break;
        case QUIC_CONNECTION_EVENT_SHUTDOWN_INITIATED_BY_TRANSPORT:
            RLOG("conn shutdown by transport (status=0x%x)", (unsigned)ev->SHUTDOWN_INITIATED_BY_TRANSPORT.Status);
            TeardownPair(link);
            break;
        case QUIC_CONNECTION_EVENT_SHUTDOWN_COMPLETE:
            MsQuic->ConnectionClose(conn);
            delete link;
            break;
        default: break;
    }
    return QUIC_STATUS_SUCCESS;
}

QUIC_STATUS QUIC_API ListenerCallback(HQUIC, void*, QUIC_LISTENER_EVENT* ev) {
    switch (ev->Type) {
        case QUIC_LISTENER_EVENT_NEW_CONNECTION: {
            RLOG("new connection arriving");
            auto* link = new PeerLink();
            link->connection = ev->NEW_CONNECTION.Connection;
            MsQuic->SetCallbackHandler(link->connection, (void*)ConnectionCallback, link);
            QUIC_STATUS s = MsQuic->ConnectionSetConfiguration(link->connection, gConfiguration);
            if (QUIC_FAILED(s)) RLOG("ConnectionSetConfiguration failed 0x%x", (unsigned)s);
            return s;
        }
        default: return QUIC_STATUS_SUCCESS;
    }
}

bool LoadConfiguration(const char* certFile, const char* keyFile) {
    QUIC_SETTINGS settings{};
    settings.IdleTimeoutMs = 60000;
    settings.IsSet.IdleTimeoutMs = TRUE;
    // Real heartbeat: msquic sends a PING on this cadence to keep the
    // connection (and any NAT/firewall mapping) alive and to detect a dead
    // peer well before the 60s idle timeout would.
    settings.KeepAliveIntervalMs = 15000;
    settings.IsSet.KeepAliveIntervalMs = TRUE;
    settings.PeerBidiStreamCount = 8;
    settings.IsSet.PeerBidiStreamCount = TRUE;
    settings.PeerUnidiStreamCount = 8;
    settings.IsSet.PeerUnidiStreamCount = TRUE;
    settings.ServerResumptionLevel = QUIC_SERVER_RESUME_AND_ZERORTT;
    settings.IsSet.ServerResumptionLevel = TRUE;

    QUIC_CERTIFICATE_FILE certFileCfg{};
    certFileCfg.CertificateFile = certFile;
    certFileCfg.PrivateKeyFile  = keyFile;

    QUIC_CREDENTIAL_CONFIG cred{};
    cred.Type = QUIC_CREDENTIAL_TYPE_CERTIFICATE_FILE;
    cred.Flags = QUIC_CREDENTIAL_FLAG_NONE; // server
    cred.CertificateFile = &certFileCfg;

    if (QUIC_FAILED(MsQuic->ConfigurationOpen(gRegistration, &kAlpn, 1, &settings,
                                              sizeof(settings), nullptr, &gConfiguration))) {
        RLOG("ConfigurationOpen failed");
        return false;
    }
    if (QUIC_FAILED(MsQuic->ConfigurationLoadCredential(gConfiguration, &cred))) {
        RLOG("ConfigurationLoadCredential failed (check cert/key paths)");
        return false;
    }
    return true;
}

} // namespace

int main(int argc, char** argv) {
    uint16_t port = 7100;
    const char* cert = "server/nebula_relay/certs/relay_cert.pem";
    const char* key  = "server/nebula_relay/certs/relay_key.pem";
    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        auto next = [&](const char* def) { return (i + 1 < argc) ? argv[++i] : def; };
        if (a == "--port") port = (uint16_t)atoi(next("7100"));
        else if (a == "--cert") cert = next(cert);
        else if (a == "--key")  key  = next(key);
        else if (a == "--saas-auth-url")    gSaasAuthUrl = next("");
        else if (a == "--saas-auth-secret") gSaasAuthSecret = next("");
        else if (a == "--saas-heartbeat-url") gSaasHeartbeatUrl = next("");
        else if (a == "--saas-heartbeat-interval-secs") gSaasHeartbeatIntervalSecs = atoi(next("30"));
    }
    if (!gSaasAuthUrl.empty() && gSaasAuthSecret.empty()) {
        RLOG("--saas-auth-url requires --saas-auth-secret; refusing to start unauthenticated callback");
        return 1;
    }
    if (!gSaasAuthUrl.empty()) {
        RLOG("SaaS mode: CWA authorization delegated to %s", gSaasAuthUrl.c_str());
    }
    if (!gSaasHeartbeatUrl.empty() && gSaasAuthSecret.empty()) {
        RLOG("--saas-heartbeat-url requires --saas-auth-secret (same shared secret as authorize); refusing to start");
        return 1;
    }
    if (!gSaasHeartbeatUrl.empty()) {
        RLOG("SaaS mode: reporting registered VDAs to %s every %ds", gSaasHeartbeatUrl.c_str(), gSaasHeartbeatIntervalSecs);
    }
    curl_global_init(CURL_GLOBAL_DEFAULT);

    if (QUIC_FAILED(MsQuicOpen2(&MsQuic))) { RLOG("MsQuicOpen2 failed"); return 1; }

    QUIC_REGISTRATION_CONFIG regConfig = { "nebula-relay", QUIC_EXECUTION_PROFILE_LOW_LATENCY };
    if (QUIC_FAILED(MsQuic->RegistrationOpen(&regConfig, &gRegistration))) {
        RLOG("RegistrationOpen failed"); return 1;
    }
    if (!LoadConfiguration(cert, key)) return 1;

    HQUIC listener = nullptr;
    if (QUIC_FAILED(MsQuic->ListenerOpen(gRegistration, ListenerCallback, nullptr, &listener))) {
        RLOG("ListenerOpen failed"); return 1;
    }
    QUIC_ADDR addr{};
    QuicAddrSetFamily(&addr, QUIC_ADDRESS_FAMILY_UNSPEC);
    QuicAddrSetPort(&addr, port);
    if (QUIC_FAILED(MsQuic->ListenerStart(listener, &kAlpn, 1, &addr))) {
        RLOG("ListenerStart failed on port %u", port); return 1;
    }

    RLOG("listening on port %u (ALPN nebula-relay)", port);
    RLOG("Ctrl-C to quit");

    // Background reaper: expires VDA-reattach grace windows and old tickets.
    std::thread(ReaperLoop).detach();

    // Background: periodic VDA heartbeat to the SaaS control plane (opt-in).
    if (!gSaasHeartbeatUrl.empty()) {
        std::thread(HeartbeatLoop).detach();
    }

    // Block until terminated by a signal (robust for a background server).
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGINT);
    sigaddset(&set, SIGTERM);
    pthread_sigmask(SIG_BLOCK, &set, nullptr);
    int sig = 0;
    sigwait(&set, &sig);
    RLOG("signal %d received, shutting down", sig);

    MsQuic->ListenerClose(listener);
    MsQuic->ConfigurationClose(gConfiguration);
    MsQuic->RegistrationClose(gRegistration);
    MsQuicClose(MsQuic);
    curl_global_cleanup();
    return 0;
}
