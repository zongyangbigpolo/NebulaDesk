//
// CwaClient.h - CWA pipeline: QUIC receive -> decode -> render/play
//
#pragma once

#include "NebulaTypes.h"
#include "NebulaFrame.h"
#include "NebulaInput.h"
#include <cstdint>
#include <functional>
#include <memory>
#include <string>

namespace nebula {

class CwaClient {
public:
    using VideoFrameCb = std::function<void(const RawVideoFrame& frame)>;
    using AudioPcmCb   = std::function<void(const RawAudioFrame& pcm)>;
    using ReadyCb      = std::function<void(const NebulaCaps& negotiated)>;

    CwaClient();
    ~CwaClient();

    void setPreSharedKey(const std::string& psk);
    // Route through a relay instead of connecting directly. Call before connect().
    // `ticket` is an optional reconnect ticket from a prior session (see
    // RelayProtocol.h); pass "" (default) on a first-time connect.
    void useRelay(const std::string& relayHost, uint16_t relayPort,
                  const std::string& deviceId, const std::string& token,
                  const std::string& ticket = "");
    void setVideoFrameCallback(VideoFrameCb cb);
    void setAudioPcmCallback(AudioPcmCb cb);
    void setReadyCallback(ReadyCb cb);

    // In relay mode host/port are ignored (the relay was set via useRelay).
    bool connect(const std::string& host, uint16_t port, const NebulaCaps& requested);
    // Send a mouse/keyboard event to the VDA over the control channel.
    void sendInput(const NebulaInputEvent& e);
    // The fresh reconnect ticket issued by the relay after pairing (relay
    // mode only); empty otherwise. Callers should cache this and pass it to
    // the next useRelay() call so a later reconnect can skip the long-lived
    // token. Only meaningful after the ready callback has fired.
    std::string relayTicket() const;
    void stop();

private:
    struct Impl;
    std::unique_ptr<Impl> m_impl;
};

} // namespace nebula
