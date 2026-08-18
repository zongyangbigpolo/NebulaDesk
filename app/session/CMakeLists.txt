# app/session — 查看器 (per-session CWA viewer window).
# Runs as an independent high-refresh process launched by the manager.
add_executable(nebula_session
    main.mm
    AppDelegate.mm
)
target_include_directories(nebula_session PRIVATE ${NEBULA_CORE_INC} ${NEBULA_MAC_INC})
set_source_files_properties(main.mm AppDelegate.mm
    PROPERTIES COMPILE_FLAGS "${NEBULA_OBJCXX_FLAGS}")
target_link_libraries(nebula_session PRIVATE nebula_mac)
