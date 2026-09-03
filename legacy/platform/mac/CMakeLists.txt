# platform/mac: macOS backends (capture, codec, render, audio, input, transport)
# plus the pipeline orchestration (.mm here for now; becomes portable .cpp in P3).
add_library(nebula_mac STATIC
    src/QuicTransport.mm
    src/RelayTransport.mm
    src/UpgradingTransport.mm
    src/ScreenCapture.mm
    src/VirtualDisplay.mm
    src/VideoEncoder.mm
    src/AudioEncoder.mm
    src/InputInjector.mm
    src/VideoDecoder.mm
    src/AudioDecoder.mm
    src/MetalRenderer.mm
    src/AudioPlayer.mm
    src/InputView.mm
    src/ProcessSpawner.mm
    ${CMAKE_SOURCE_DIR}/core/src/VdaServer.mm
    ${CMAKE_SOURCE_DIR}/core/src/CwaClient.mm
)
target_include_directories(nebula_mac PUBLIC inc ${NEBULA_CORE_INC})
set_source_files_properties(
    src/QuicTransport.mm src/RelayTransport.mm src/UpgradingTransport.mm src/ScreenCapture.mm src/VirtualDisplay.mm src/VideoEncoder.mm src/AudioEncoder.mm
    src/InputInjector.mm src/VideoDecoder.mm src/AudioDecoder.mm src/MetalRenderer.mm
    src/AudioPlayer.mm src/InputView.mm src/ProcessSpawner.mm
    ${CMAKE_SOURCE_DIR}/core/src/VdaServer.mm ${CMAKE_SOURCE_DIR}/core/src/CwaClient.mm
    PROPERTIES COMPILE_FLAGS "${NEBULA_OBJCXX_FLAGS}")

target_link_libraries(nebula_mac PUBLIC
    nebula_core
    "-framework Foundation"
    "-framework AppKit"
    "-framework CoreGraphics"
    "-framework QuartzCore"
    "-framework Metal"
    "-framework CoreMedia"
    "-framework CoreVideo"
    "-framework VideoToolbox"
    "-framework AudioToolbox"
    "-framework AVFoundation"
    "-framework ScreenCaptureKit"
    "-framework Network"
    "-framework Security"
)
