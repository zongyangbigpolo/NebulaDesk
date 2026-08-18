//
// InputCapture.h - platform-neutral input capture interface (CWA/session side)
//
// Captures local mouse/keyboard inside the session window and emits normalized
// NebulaInputEvents to forward to the VDA. macOS backend = NSView event methods;
// Windows = Win32 message loop; Linux = X11/Wayland.
//
#pragma once

#include "NebulaInput.h"
#include <functional>
#include <memory>

namespace nebula {

class IInputCapture {
public:
    using EventCb = std::function<void(const NebulaInputEvent&)>;

    virtual ~IInputCapture() = default;

    // nativeView is the platform view/window to attach event capture to.
    virtual bool attach(void* nativeView) = 0;
    virtual void setOnEvent(EventCb cb) = 0;
    virtual void detach() = 0;
};

std::unique_ptr<IInputCapture> CreateInputCapture();

} // namespace nebula
