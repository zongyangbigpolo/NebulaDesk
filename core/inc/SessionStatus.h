//
// SessionStatus.h - manager<->session status contract (header-only)
//
// A session process reports lifecycle state to its parent (the Flutter manager)
// by writing a single line "NEBULA_STATUS:<state>\n" to stdout and flushing.
// stderr carries human logs; stdout is reserved for this machine-readable channel.
//
#pragma once

#include <cstdio>
#include <string>

namespace nebula {

enum class SessionState {
    Connecting,
    Connected,    // transport up, caps negotiated
    Streaming,    // first media frame rendered
    DirectUpgraded, // relay->direct upgrade succeeded (P2)
    Error,
    Disconnected,
};

inline const char* SessionStateName(SessionState s) {
    switch (s) {
        case SessionState::Connecting:     return "connecting";
        case SessionState::Connected:      return "connected";
        case SessionState::Streaming:      return "streaming";
        case SessionState::DirectUpgraded: return "direct";
        case SessionState::Error:          return "error";
        case SessionState::Disconnected:   return "disconnected";
    }
    return "unknown";
}

inline void ReportSessionStatus(SessionState s) {
    std::fprintf(stdout, "NEBULA_STATUS:%s\n", SessionStateName(s));
    std::fflush(stdout);
}

} // namespace nebula
