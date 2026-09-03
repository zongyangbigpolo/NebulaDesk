//
// VirtualDisplay.h - create/destroy a macOS virtual display (VDA side).
//
// Backed by the private CoreGraphics CGVirtualDisplay API (the same one used by
// BetterDisplay / Luna Display). There is no public macOS API for virtual
// displays; this lets the VDA present a viewer-chosen resolution without
// changing its physical monitor. Falls back gracefully if creation fails.
//
#pragma once

#include <cstdint>
#include <memory>

namespace nebula {

class IVirtualDisplay {
public:
    virtual ~IVirtualDisplay() = default;
    // The CGDirectDisplayID of the created virtual display (0 if creation failed).
    virtual uint32_t displayId() const = 0;
    virtual bool valid() const = 0;
};

// Creates a virtual display of width×height (logical points) at the given refresh
// rate. `hidpi` doubles the backing pixels for Retina-like sharpness. Returns
// null on failure (mandatory virtual-display capture must fail).
std::unique_ptr<IVirtualDisplay> CreateVirtualDisplay(uint32_t width, uint32_t height,
                                                      uint32_t fps, bool hidpi);

} // namespace nebula
