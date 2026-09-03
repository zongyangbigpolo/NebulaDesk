//
// WebRtcGateway.cpp - signaling WebSocket client + multi-viewer session map
//
#include "WebRtcGateway.h"
#include "NebulaLog.h"

#include <rtc/rtc.hpp>
#include <nlohmann/json.hpp>

#include <map>
#include <mutex>

#define TAG "webrtc-gw"

namespace nebula {
namespace {
using json = nlohmann::json;
}

struct WebRtcGateway::Impl {
    std::shared_ptr<rtc::WebSocket> ws;
    std::vector<IceServerConfig> iceServers;
    VideoConfig video;
    AudioConfig audio;

    InputCb onInput;
    ViewerCountCb onViewerCount;
    std::function<void()> onKeyframeRequest;

    mutable std::mutex mutex;
    std::map<std::string, std::shared_ptr<WebRtcSession>> sessions; // viewerId -> session

    void notifyViewerCount() {
        size_t n;
        { std::lock_guard<std::mutex> lk(mutex); n = sessions.size(); }
        if (onViewerCount) onViewerCount(n);
    }

    void sendJson(const json& j) {
        if (!ws || !ws->isOpen()) return;
        auto s = j.dump();
        ws->send(s);
    }

    void handleViewerJoin(const std::string& viewerId) {
        NEBULA_LOGI(TAG, "viewer-join %s", viewerId.c_str());
        auto session = std::make_shared<WebRtcSession>();

        session->setOnLocalDescription([this, viewerId](const std::string& type, const std::string& sdp) {
            sendJson({{"type", type}, {"viewerId", viewerId}, {"sdp", sdp}});
        });
        session->setOnLocalCandidate([this, viewerId](const std::string& candidate, const std::string& mid) {
            sendJson({{"type", "ice"}, {"viewerId", viewerId}, {"candidate", candidate}, {"mid", mid}});
        });
        session->setOnInput([this](const NebulaInputEvent& ev) {
            if (onInput) onInput(ev);
        });
        session->setOnKeyframeRequest([this]() {
            if (onKeyframeRequest) onKeyframeRequest();
        });
        session->setOnStateChange([this, viewerId](bool up) {
            NEBULA_LOGI(TAG, "viewer %s %s", viewerId.c_str(), up ? "connected" : "disconnected");
            if (!up) removeSession(viewerId);
        });

        if (!session->start(video, audio, iceServers)) {
            NEBULA_LOGE(TAG, "failed to start WebRtcSession for viewer %s", viewerId.c_str());
            return;
        }
        {
            std::lock_guard<std::mutex> lk(mutex);
            sessions[viewerId] = session;
        }
        notifyViewerCount();
    }

    void removeSession(const std::string& viewerId) {
        std::shared_ptr<WebRtcSession> victim;
        {
            std::lock_guard<std::mutex> lk(mutex);
            auto it = sessions.find(viewerId);
            if (it == sessions.end()) return;
            victim = it->second;
            sessions.erase(it);
        }
        victim->close();
        notifyViewerCount();
    }

    void handleMessage(const std::string& text) {
        json j;
        try {
            j = json::parse(text);
        } catch (const std::exception& e) {
            NEBULA_LOGW(TAG, "signaling: malformed JSON: %s", e.what());
            return;
        }
        std::string type = j.value("type", "");
        std::string viewerId = j.value("viewerId", "");

        if (type == "viewer-join") {
            handleViewerJoin(viewerId);
            return;
        }
        if (type == "viewer-leave") {
            removeSession(viewerId);
            return;
        }

        std::shared_ptr<WebRtcSession> session;
        {
            std::lock_guard<std::mutex> lk(mutex);
            auto it = sessions.find(viewerId);
            if (it != sessions.end()) session = it->second;
        }
        if (!session) {
            NEBULA_LOGW(TAG, "signaling: message type=%s for unknown viewer=%s", type.c_str(), viewerId.c_str());
            return;
        }
        if (type == "answer") {
            session->setRemoteDescription("answer", j.value("sdp", ""));
        } else if (type == "ice") {
            session->addRemoteCandidate(j.value("candidate", ""), j.value("mid", ""));
        }
    }
};

WebRtcGateway::WebRtcGateway() : m_impl(std::make_unique<Impl>()) {}
WebRtcGateway::~WebRtcGateway() { close(); }

void WebRtcGateway::configure(std::vector<IceServerConfig> iceServers, const VideoConfig& video,
                              const AudioConfig& audio) {
    m_impl->iceServers = std::move(iceServers);
    m_impl->video = video;
    m_impl->audio = audio;
}

void WebRtcGateway::setOnInput(InputCb cb) { m_impl->onInput = std::move(cb); }
void WebRtcGateway::setOnViewerCountChanged(ViewerCountCb cb) { m_impl->onViewerCount = std::move(cb); }
void WebRtcGateway::setOnKeyframeRequest(std::function<void()> cb) { m_impl->onKeyframeRequest = std::move(cb); }

bool WebRtcGateway::connectSignaling(const std::string& wsUrl, const std::string& deviceId,
                                     const std::string& token) {
    m_impl->ws = std::make_shared<rtc::WebSocket>();
    m_impl->ws->onOpen([this, deviceId, token]() {
        NEBULA_LOGI(TAG, "signaling connected; registering as VDA device=%s", deviceId.c_str());
        m_impl->sendJson({{"type", "hello"}, {"role", "vda"}, {"deviceId", deviceId}, {"token", token}});
    });
    m_impl->ws->onMessage([this](rtc::message_variant data) {
        if (std::holds_alternative<std::string>(data)) m_impl->handleMessage(std::get<std::string>(data));
    });
    m_impl->ws->onClosed([this]() { NEBULA_LOGW(TAG, "signaling connection closed"); });
    m_impl->ws->onError([](std::string err) { NEBULA_LOGE(TAG, "signaling error: %s", err.c_str()); });

    try {
        m_impl->ws->open(wsUrl);
    } catch (const std::exception& e) {
        NEBULA_LOGE(TAG, "failed to open signaling websocket %s: %s", wsUrl.c_str(), e.what());
        return false;
    }
    return true;
}

void WebRtcGateway::pushVideoFrame(const EncodedFrame& frame) {
    std::vector<std::shared_ptr<WebRtcSession>> targets;
    { std::lock_guard<std::mutex> lk(m_impl->mutex);
      for (auto& kv : m_impl->sessions) targets.push_back(kv.second); }
    for (auto& s : targets) s->sendVideoFrame(frame);
}

void WebRtcGateway::pushAudioFrame(const EncodedFrame& frame) {
    std::vector<std::shared_ptr<WebRtcSession>> targets;
    { std::lock_guard<std::mutex> lk(m_impl->mutex);
      for (auto& kv : m_impl->sessions) targets.push_back(kv.second); }
    for (auto& s : targets) s->sendAudioFrame(frame);
}

size_t WebRtcGateway::viewerCount() const {
    std::lock_guard<std::mutex> lk(m_impl->mutex);
    return m_impl->sessions.size();
}

void WebRtcGateway::close() {
    std::map<std::string, std::shared_ptr<WebRtcSession>> sessions;
    { std::lock_guard<std::mutex> lk(m_impl->mutex); sessions.swap(m_impl->sessions); }
    for (auto& kv : sessions) kv.second->close();
    if (m_impl->ws) { m_impl->ws->close(); m_impl->ws.reset(); }
}

} // namespace nebula
