# core: portable C++ — protocol framing, logging, signaling abstraction.
# Capability interfaces (I*) live in core/inc and carry no platform types.
add_library(nebula_core STATIC
    src/NebulaProtocol.cpp
    src/NebulaCrypto.cpp
    src/NebulaLog.cpp
    src/Signaling.cpp
    src/OpusAudioEncoder.cpp
    src/WebRtcSession.cpp
    src/WebRtcGateway.cpp
)
target_include_directories(nebula_core PUBLIC inc)
target_link_libraries(nebula_core PUBLIC "-framework Foundation")

# Application-layer session encryption (ChaCha20-Poly1305 via libsodium).
# `brew install libsodium` on macOS; pkg-config is the portable discovery path
# so the same CMake also works on a future Linux/Windows build of core/.
find_package(PkgConfig REQUIRED)
pkg_check_modules(SODIUM REQUIRED IMPORTED_TARGET libsodium)
target_link_libraries(nebula_core PUBLIC PkgConfig::SODIUM)

# Opus audio codec (WebRTC browser path only needs Opus, not AAC).
# `brew install opus`.
pkg_check_modules(OPUS REQUIRED IMPORTED_TARGET opus)
target_link_libraries(nebula_core PUBLIC PkgConfig::OPUS)

# WebRTC (browser interop, P4 — see ROADMAP.md §12). Provided by the
# top-level CMakeLists.txt's FetchContent(libdatachannel).
target_link_libraries(nebula_core PUBLIC LibDataChannel::LibDataChannelStatic)

# Signaling messages are small JSON blobs (see WebRtcGateway.h).
pkg_check_modules(NLOHMANN_JSON REQUIRED IMPORTED_TARGET nlohmann_json)
target_link_libraries(nebula_core PUBLIC PkgConfig::NLOHMANN_JSON)
