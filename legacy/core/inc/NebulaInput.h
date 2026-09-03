//
// NebulaInput.h - input event wire format (CWA -> VDA), phase 2
//
#pragma once

#include <cstdint>
#include <cstddef>
#include <cstring>
#include <vector>

namespace nebula {

// Input event categories carried in an INPUT message payload.
enum class InputType : uint8_t {
    MouseMove   = 1, // absolute, normalized position
    MouseDown   = 2,
    MouseUp     = 3,
    MouseDrag   = 4, // move while a button is held
    Wheel       = 5,
    KeyDown     = 6,
    KeyUp       = 7,
};

enum MouseButton : uint8_t {
    Btn_Left   = 0,
    Btn_Right  = 1,
    Btn_Middle = 2,
};

// Modifier bitmask (matches the subset we forward).
enum InputModifiers : uint16_t {
    Mod_None    = 0,
    Mod_Shift   = 1 << 0,
    Mod_Control = 1 << 1,
    Mod_Option  = 1 << 2,
    Mod_Command = 1 << 3,
    Mod_CapsLock= 1 << 4,
    Mod_Fn      = 1 << 5,
};

// One input event. Position is normalized [0,1] relative to the remote screen
// so it is resolution-independent. wheelX/wheelY are scroll deltas (lines).
#pragma pack(push, 1)
struct NebulaInputEvent {
    uint8_t  type;       // InputType
    uint8_t  button;     // MouseButton (mouse events) or click count
    uint16_t modifiers;  // InputModifiers
    float    x;          // normalized [0,1]
    float    y;          // normalized [0,1]
    int32_t  wheelX;     // scroll delta
    int32_t  wheelY;     // scroll delta
    uint16_t keyCode;    // platform virtual key code (kVK_*)
    uint16_t reserved;
};
#pragma pack(pop)

static_assert(sizeof(NebulaInputEvent) == 24, "NebulaInputEvent must be 24 bytes");

inline std::vector<uint8_t> EncodeInput(const NebulaInputEvent& e) {
    const uint8_t* p = reinterpret_cast<const uint8_t*>(&e);
    return std::vector<uint8_t>(p, p + sizeof(e));
}

inline bool DecodeInput(const uint8_t* data, size_t len, NebulaInputEvent& out) {
    if (!data || len < sizeof(NebulaInputEvent)) return false;
    std::memcpy(&out, data, sizeof(NebulaInputEvent));
    return true;
}

} // namespace nebula
