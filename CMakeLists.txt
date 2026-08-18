cmake_minimum_required(VERSION 3.21)

# Must be set BEFORE project() so the compiler is probed with this floor;
# otherwise the binary inherits the build host's OS version as its minimum.
set(CMAKE_OSX_DEPLOYMENT_TARGET "26.0" CACHE STRING "Minimum macOS version")

project(Nebula LANGUAGES CXX OBJCXX)

if(NOT APPLE)
    message(FATAL_ERROR "Nebula currently builds only on macOS (Win/Linux backends are P3)")
endif()

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE Release)
endif()

# Network.framework C objects use manual nw_retain/nw_release -> no ARC for .mm.
set(NEBULA_OBJCXX_FLAGS "-fno-objc-arc")

# Portable interface headers (core) + mac-specific headers (platform/mac).
set(NEBULA_CORE_INC "${CMAKE_CURRENT_SOURCE_DIR}/core/inc")
set(NEBULA_MAC_INC  "${CMAKE_CURRENT_SOURCE_DIR}/platform/mac/inc")

# --- WebRTC (browser interop, P4 — see ROADMAP.md §12) ---------------------
# libdatachannel (MPL 2.0) provides ICE/DTLS/SRTP/Data-Channels compatible
# with real browsers, without pulling in Google's full libwebrtc. Built
# against system libraries (all permissively/weak-copyleft licensed, no
# GPL/LGPL): `brew install libjuice srtp libusrsctp opus nlohmann-json plog`.
include(FetchContent)
set(PREFER_SYSTEM_LIB ON CACHE BOOL "" FORCE)
set(USE_SYSTEM_PLOG ON CACHE BOOL "" FORCE)
set(USE_SYSTEM_JSON ON CACHE BOOL "" FORCE)
set(NO_EXAMPLES ON CACHE BOOL "" FORCE)
set(NO_TESTS ON CACHE BOOL "" FORCE)
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
FetchContent_Declare(libdatachannel
    GIT_REPOSITORY https://github.com/paullouisageneau/libdatachannel.git
    GIT_TAG        v0.23.1
    GIT_SHALLOW    TRUE
)
FetchContent_MakeAvailable(libdatachannel)

add_subdirectory(core)
add_subdirectory(platform/mac)
add_subdirectory(app/vda)
add_subdirectory(app/session)
if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/server/nebula_relay/CMakeLists.txt")
    add_subdirectory(server/nebula_relay)
endif()

enable_testing()
add_executable(nebula_input_test tests/test_input.cpp)
target_link_libraries(nebula_input_test PRIVATE nebula_core)
add_test(NAME nebula_input_protocol COMMAND nebula_input_test)

add_executable(nebula_crypto_test tests/test_crypto.cpp)
target_link_libraries(nebula_crypto_test PRIVATE nebula_core)
add_test(NAME nebula_session_crypto COMMAND nebula_crypto_test)

add_executable(nebula_webrtc_test tests/test_webrtc_session.cpp)
target_link_libraries(nebula_webrtc_test PRIVATE nebula_core)
add_test(NAME nebula_webrtc_offer COMMAND nebula_webrtc_test)
