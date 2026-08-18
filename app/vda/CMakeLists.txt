# app/vda — 被控端 (the shared Mac): capture + encode + serve over QUIC.
add_executable(nebula_vda main.mm)
target_include_directories(nebula_vda PRIVATE ${NEBULA_CORE_INC} ${NEBULA_MAC_INC})
set_source_files_properties(main.mm PROPERTIES COMPILE_FLAGS "${NEBULA_OBJCXX_FLAGS}")
target_link_libraries(nebula_vda PRIVATE nebula_mac)
