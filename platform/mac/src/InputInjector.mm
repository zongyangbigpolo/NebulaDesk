//
// InputInjector.mm - CGEvent-based input injection (VDA side)
//
#include "InputInjector.h"
#include "NebulaLog.h"

#import <CoreGraphics/CoreGraphics.h>
#import <AppKit/AppKit.h>

#include <atomic>

#define TAG "inject"

namespace nebula {
namespace {

CGEventFlags ToCGFlags(uint16_t mods) {
    CGEventFlags f = 0;
    if (mods & Mod_Shift)    f |= kCGEventFlagMaskShift;
    if (mods & Mod_Control)  f |= kCGEventFlagMaskControl;
    if (mods & Mod_Option)   f |= kCGEventFlagMaskAlternate;
    if (mods & Mod_Command)  f |= kCGEventFlagMaskCommand;
    if (mods & Mod_CapsLock) f |= kCGEventFlagMaskAlphaShift;
    if (mods & Mod_Fn)       f |= kCGEventFlagMaskSecondaryFn;
    return f;
}

class InputInjector final : public IInputInjector {
public:
    InputInjector() {
        m_source = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
    }
    ~InputInjector() override {
        if (m_source) CFRelease(m_source);
    }

    void setTargetDisplay(uint32_t displayId) override {
        m_displayId.store((CGDirectDisplayID)displayId);
        NEBULA_LOGI(TAG, "targeting captured display id=%u", displayId);
    }

    void inject(const NebulaInputEvent& e) override {
        if (!m_displayId.load()) {
            NEBULA_LOGW(TAG, "dropping input before captured display is ready");
            return;
        }
        switch ((InputType)e.type) {
            case InputType::MouseMove:  postMouse(e, kCGEventMouseMoved); break;
            case InputType::MouseDrag:  postMouse(e, dragType(e.button)); break;
            case InputType::MouseDown:  postMouse(e, downType(e.button)); break;
            case InputType::MouseUp:    postMouse(e, upType(e.button));   break;
            case InputType::Wheel:      postWheel(e); break;
            case InputType::KeyDown:    postKey(e, true);  break;
            case InputType::KeyUp:      postKey(e, false); break;
        }
    }

private:
    CGPoint mapPoint(const NebulaInputEvent& e) const {
        CGRect bounds = CGDisplayBounds(m_displayId.load());
        CGFloat x = bounds.origin.x + e.x * bounds.size.width;
        CGFloat y = bounds.origin.y + e.y * bounds.size.height;
        m_lastPoint = CGPointMake(x, y);
        return m_lastPoint;
    }

    static CGEventType downType(uint8_t btn) {
        if (btn == Btn_Right)  return kCGEventRightMouseDown;
        if (btn == Btn_Middle) return kCGEventOtherMouseDown;
        return kCGEventLeftMouseDown;
    }
    static CGEventType upType(uint8_t btn) {
        if (btn == Btn_Right)  return kCGEventRightMouseUp;
        if (btn == Btn_Middle) return kCGEventOtherMouseUp;
        return kCGEventLeftMouseUp;
    }
    static CGEventType dragType(uint8_t btn) {
        if (btn == Btn_Right)  return kCGEventRightMouseDragged;
        if (btn == Btn_Middle) return kCGEventOtherMouseDragged;
        return kCGEventLeftMouseDragged;
    }
    static CGMouseButton cgButton(uint8_t btn) {
        if (btn == Btn_Right)  return kCGMouseButtonRight;
        if (btn == Btn_Middle) return kCGMouseButtonCenter;
        return kCGMouseButtonLeft;
    }

    void postMouse(const NebulaInputEvent& e, CGEventType type) {
        CGPoint pt = mapPoint(e);
        CGEventRef ev = CGEventCreateMouseEvent(m_source, type, pt, cgButton(e.button));
        if (!ev) return;
        CGEventSetFlags(ev, ToCGFlags(e.modifiers));
        CGEventPost(kCGHIDEventTap, ev);
        CFRelease(ev);
    }

    void postWheel(const NebulaInputEvent& e) {
        CGEventRef ev = CGEventCreateScrollWheelEvent(
            m_source, kCGScrollEventUnitLine, 2, e.wheelY, e.wheelX);
        if (!ev) return;
        CGEventSetFlags(ev, ToCGFlags(e.modifiers));
        CGEventPost(kCGHIDEventTap, ev);
        CFRelease(ev);
    }

    void postKey(const NebulaInputEvent& e, bool down) {
        CGEventRef ev = CGEventCreateKeyboardEvent(m_source, (CGKeyCode)e.keyCode, down);
        if (!ev) return;
        CGEventSetFlags(ev, ToCGFlags(e.modifiers));
        CGEventPost(kCGHIDEventTap, ev);
        CFRelease(ev);
    }

    CGEventSourceRef    m_source = nullptr;
    std::atomic<CGDirectDisplayID> m_displayId{0};
    mutable CGPoint     m_lastPoint{};
};

} // namespace

std::unique_ptr<IInputInjector> CreateInputInjector() {
    return std::make_unique<InputInjector>();
}

} // namespace nebula
