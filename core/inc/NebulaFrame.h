//
// NebulaFrame.h - platform-neutral raw media frame descriptors
//
// These let capability interfaces (IVideoEncoder/Decoder, IScreenCapture,
// IRenderer, IAudio*) cross the Core boundary without leaking Apple types
// (CVPixelBufferRef / CMSampleBufferRef). Each platform backend converts its
// native buffer to/from these structs at the edge.
//
// The structs are non-owning *views* over memory owned by the backend; the
// callee must copy if it needs to retain the data past the callback.
//
#pragma once

#include <cstdint>
#include <cstddef>

namespace nebula {

// Pixel layout of a raw (decoded/captured) video frame.
enum class PixelFormat : uint8_t {
    Unknown = 0,
    NV12    = 1, // bi-planar Y + interleaved CbCr (4:2:0), the hot path
    BGRA    = 2, // 32-bit interleaved
};

// A raw video frame as up-to-3 planes (NV12 uses 2). Non-owning.
struct RawVideoFrame {
    PixelFormat format = PixelFormat::NV12;
    uint32_t    width  = 0;
    uint32_t    height = 0;
    uint64_t    timestampUs = 0;

    static constexpr int kMaxPlanes = 3;
    const uint8_t* planeData[kMaxPlanes]   = { nullptr, nullptr, nullptr };
    uint32_t       planeStride[kMaxPlanes] = { 0, 0, 0 }; // bytes per row
    uint32_t       planeCount = 0;

    // Optional opaque backend handle (e.g. a CVPixelBufferRef) for zero-copy
    // paths where the renderer can consume the native buffer directly. Core
    // code never dereferences this; only the same-platform backend does.
    void* nativeHandle = nullptr;
};

// A raw PCM audio buffer (interleaved Float32). Non-owning.
struct RawAudioFrame {
    const float* samples   = nullptr; // interleaved (may be null if nativeHandle used)
    uint32_t     frames    = 0;       // per channel
    uint32_t     channels  = 0;
    uint32_t     sampleRate = 0;
    uint64_t     timestampUs = 0;

    // Optional opaque backend handle (e.g. a CMSampleBufferRef) for same-platform
    // capture->encode handoff. Core code never dereferences this.
    void* nativeHandle = nullptr;
};

} // namespace nebula
