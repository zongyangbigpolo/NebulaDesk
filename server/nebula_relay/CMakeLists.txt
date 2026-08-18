# server/nebula_relay — blind-forwarding QUIC relay (msquic, pure C++).
# Cross-platform by design; only built on macOS for now.
find_path(MSQUIC_INCLUDE_DIR msquic.h
    HINTS /opt/homebrew/include /usr/local/include /usr/include)
find_library(MSQUIC_LIB msquic
    HINTS /opt/homebrew/lib /usr/local/lib /usr/lib)

if(NOT MSQUIC_INCLUDE_DIR OR NOT MSQUIC_LIB)
    message(WARNING "msquic not found — skipping nebula_relay. Install via: brew install libmsquic")
    return()
endif()

# Optional SaaS integration (--saas-auth-url / --saas-auth-secret): the relay
# calls out to an external control plane's /internal/authorize over HTTPS
# instead of comparing a static token. Uses the system libcurl (ships with
# macOS), no extra brew dependency required.
find_package(CURL REQUIRED)

add_executable(nebula_relay main.cpp)
target_include_directories(nebula_relay PRIVATE ${NEBULA_CORE_INC} ${MSQUIC_INCLUDE_DIR})
target_link_libraries(nebula_relay PRIVATE ${MSQUIC_LIB} CURL::libcurl)

# Generate dev cert/key next to the binary if absent (they are git-ignored).
add_custom_command(TARGET nebula_relay POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E make_directory $<TARGET_FILE_DIR:nebula_relay>/certs
    COMMAND ${CMAKE_COMMAND}
        -DCERT_DIR=$<TARGET_FILE_DIR:nebula_relay>/certs
        -P ${CMAKE_CURRENT_SOURCE_DIR}/gen_dev_cert.cmake)
