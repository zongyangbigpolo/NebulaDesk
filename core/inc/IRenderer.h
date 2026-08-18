//
// Renderer.h - platform-neutral video renderer interface (CWA/session side)
//
// Presents decoded frames. The macOS backend (Metal) consumes
// frame.nativeHandle (CVImageBufferRef) for a zero-copy texture upload; future
// backends use D3D11 (Windows) or Vulkan (Linux).
//
#pragma once

#include "NebulaFrame.h"
#include <memory>

namespace nebula {

class IRenderer {
public:
    virtual ~IRenderer() = default;

    // nativeWindow is a platform window/layer handle the renderer draws into
    // (e.g. a CAMetalLayer* on macOS, HWND on Windows). Opaque to Core.
    virtual bool init(void* nativeWindow, uint32_t width, uint32_t height) = 0;
    virtual void resize(uint32_t width, uint32_t height) = 0;
    virtual void render(const RawVideoFrame& frame) = 0;
    virtual void shutdown() = 0;
};

std::unique_ptr<IRenderer> CreateRenderer();

} // namespace nebula
