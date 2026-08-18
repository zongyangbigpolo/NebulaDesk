// Round-trip test for the phase-2 input protocol + framing.
#include "NebulaInput.h"
#include "NebulaProtocol.h"
#include <cassert>
#include <cstdio>

using namespace nebula;

int main() {
    // 1) Input event round-trips through encode/decode.
    NebulaInputEvent in{};
    in.type = (uint8_t)InputType::MouseDrag;
    in.button = Btn_Right;
    in.modifiers = Mod_Shift | Mod_Command;
    in.x = 0.4242f; in.y = 0.7777f;
    in.wheelX = -3; in.wheelY = 9;
    in.keyCode = 0x35; // Escape
    auto bytes = EncodeInput(in);
    assert(bytes.size() == 24);

    NebulaInputEvent out{};
    assert(DecodeInput(bytes.data(), bytes.size(), out));
    assert(out.type == in.type && out.button == in.button);
    assert(out.modifiers == in.modifiers);
    assert(out.x == in.x && out.y == in.y);
    assert(out.wheelX == in.wheelX && out.wheelY == in.wheelY);
    assert(out.keyCode == in.keyCode);

    // 2) Wrap as an INPUT message and parse it back (the over-the-wire path).
    auto msg = BuildMessage(MsgType::Input, Flag_None, 7, 0, bytes.data(), bytes.size());
    NebulaFrameHeader h{};
    assert(ParseHeader(msg.data(), msg.size(), h));
    assert(msg[0] == 'N' && msg[1] == 'E' && msg[2] == 'B' && msg[3] == 'U');
    assert(h.version == kNebulaVersion);
    assert(h.type == (uint8_t)MsgType::Input);
    assert(h.length == 24 && h.seq == 7);

    NebulaInputEvent out2{};
    assert(DecodeInput(msg.data() + kHeaderSize, h.length, out2));
    assert(out2.keyCode == 0x35 && out2.button == Btn_Right);

    // 3) Key event with no mouse fields.
    NebulaInputEvent key{};
    key.type = (uint8_t)InputType::KeyDown;
    key.keyCode = 0x00; // 'a'
    key.modifiers = Mod_Control;
    auto kb = EncodeInput(key);
    NebulaInputEvent kout{};
    assert(DecodeInput(kb.data(), kb.size(), kout));
    assert((InputType)kout.type == InputType::KeyDown && kout.keyCode == 0x00);

    printf("input protocol round-trip: ALL PASS\n");
    return 0;
}
