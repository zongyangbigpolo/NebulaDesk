# Generates a self-signed dev cert/key in CERT_DIR if not already present.
# Invoked at build time by server/nebula_relay/CMakeLists.txt.
if(NOT EXISTS "${CERT_DIR}/relay_cert.pem")
    find_program(OPENSSL_EXE openssl)
    if(OPENSSL_EXE)
        execute_process(COMMAND ${OPENSSL_EXE} req -x509 -newkey rsa:2048 -nodes
            -keyout ${CERT_DIR}/relay_key.pem
            -out    ${CERT_DIR}/relay_cert.pem
            -days 3650 -subj /CN=nebula-relay
            RESULT_VARIABLE rc OUTPUT_QUIET ERROR_QUIET)
        if(rc EQUAL 0)
            message(STATUS "nebula_relay: generated dev cert in ${CERT_DIR}")
        else()
            message(WARNING "nebula_relay: openssl failed to generate dev cert")
        endif()
    else()
        message(WARNING "nebula_relay: openssl not found; provide certs manually (see certs/README.md)")
    endif()
endif()
