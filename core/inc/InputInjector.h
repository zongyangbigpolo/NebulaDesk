//
// InputInjector.h - injects remote input events into the local macOS session
//
#pragma once

#include "NebulaInput.h"
#include <memory>

namespace nebula {

// Maps normalized [0,1] coordinates to the captured display and synthesizes
// mouse / keyboard events via CGEvent.
class IInputInjector {
public:
    virtual ~IInputInjector() = default;
    virtual void setTargetDisplay(uint32_t displayId) = 0;
    virtual void inject(const NebulaInputEvent& e) = 0;
};

std::unique_ptr<IInputInjector> CreateInputInjector();

} // namespace nebula
