//
// InputView.mm - mouse/keyboard capture view (CWA side)
//
#include "InputView.h"

using namespace nebula;

static uint16_t ModsFromEvent(NSEvent* e) {
    NSEventModifierFlags f = e.modifierFlags;
    uint16_t m = Mod_None;
    if (f & NSEventModifierFlagShift)    m |= Mod_Shift;
    if (f & NSEventModifierFlagControl)  m |= Mod_Control;
    if (f & NSEventModifierFlagOption)   m |= Mod_Option;
    if (f & NSEventModifierFlagCommand)  m |= Mod_Command;
    if (f & NSEventModifierFlagCapsLock) m |= Mod_CapsLock;
    if (f & NSEventModifierFlagFunction) m |= Mod_Fn;
    return m;
}

@implementation NebulaInputView {
    BOOL _cursorHidden;
}

- (BOOL)acceptsFirstResponder { return YES; }
- (BOOL)acceptsFirstMouse:(NSEvent*)event { return YES; }
- (BOOL)isFlipped { return YES; } // top-left origin to match remote screen

// Convert an event location to normalized [0,1], y=0 at top.
- (void)fill:(NebulaInputEvent*)ev from:(NSEvent*)e {
    NSPoint p = [self convertPoint:e.locationInWindow fromView:nil];
    CGFloat w = self.bounds.size.width  > 0 ? self.bounds.size.width  : 1;
    CGFloat h = self.bounds.size.height > 0 ? self.bounds.size.height : 1;
    float nx = (float)(p.x / w);
    float ny = (float)(p.y / h); // already flipped (isFlipped == YES)
    ev->x = nx < 0 ? 0 : (nx > 1 ? 1 : nx);
    ev->y = ny < 0 ? 0 : (ny > 1 ? 1 : ny);
    ev->modifiers = ModsFromEvent(e);
}

- (void)emit:(InputType)type button:(uint8_t)btn event:(NSEvent*)e {
    if (!_onInput) return;
    NebulaInputEvent ev{};
    ev.type = (uint8_t)type;
    ev.button = btn;
    [self fill:&ev from:e];
    _onInput(ev);
}

// --- Mouse move / drag ---
- (void)mouseMoved:(NSEvent*)e        { [self emit:InputType::MouseMove button:Btn_Left  event:e]; }
- (void)mouseDragged:(NSEvent*)e      { [self emit:InputType::MouseDrag button:Btn_Left  event:e]; }
- (void)rightMouseDragged:(NSEvent*)e { [self emit:InputType::MouseDrag button:Btn_Right event:e]; }
- (void)otherMouseDragged:(NSEvent*)e { [self emit:InputType::MouseDrag button:Btn_Middle event:e]; }

// --- Mouse buttons ---
- (void)mouseDown:(NSEvent*)e         { [self emit:InputType::MouseDown button:Btn_Left   event:e]; }
- (void)mouseUp:(NSEvent*)e           { [self emit:InputType::MouseUp   button:Btn_Left   event:e]; }
- (void)rightMouseDown:(NSEvent*)e    { [self emit:InputType::MouseDown button:Btn_Right  event:e]; }
- (void)rightMouseUp:(NSEvent*)e      { [self emit:InputType::MouseUp   button:Btn_Right  event:e]; }
- (void)otherMouseDown:(NSEvent*)e    { [self emit:InputType::MouseDown button:Btn_Middle event:e]; }
- (void)otherMouseUp:(NSEvent*)e      { [self emit:InputType::MouseUp   button:Btn_Middle event:e]; }

// --- Scroll ---
- (void)scrollWheel:(NSEvent*)e {
    if (!_onInput) return;
    NebulaInputEvent ev{};
    ev.type = (uint8_t)InputType::Wheel;
    [self fill:&ev from:e];
    ev.wheelX = (int32_t)e.scrollingDeltaX;
    ev.wheelY = (int32_t)e.scrollingDeltaY;
    if (ev.wheelX == 0 && ev.wheelY == 0) {
        ev.wheelX = (int32_t)e.deltaX;
        ev.wheelY = (int32_t)e.deltaY;
    }
    _onInput(ev);
}

// --- Keyboard ---
- (void)keyDown:(NSEvent*)e   { [self emitKey:e down:YES]; }
- (void)keyUp:(NSEvent*)e     { [self emitKey:e down:NO];  }

- (void)emitKey:(NSEvent*)e down:(BOOL)down {
    if (!_onInput) return;
    NebulaInputEvent ev{};
    ev.type = (uint8_t)(down ? InputType::KeyDown : InputType::KeyUp);
    ev.keyCode = e.keyCode;
    ev.modifiers = ModsFromEvent(e);
    _onInput(ev);
}

// Track mouse-moved + enter/exit so we can hide the local cursor inside the view.
- (void)updateTrackingAreas {
    for (NSTrackingArea* ta in self.trackingAreas) [self removeTrackingArea:ta];
    NSTrackingArea* area = [[NSTrackingArea alloc]
        initWithRect:self.bounds
             options:(NSTrackingMouseMoved | NSTrackingMouseEnteredAndExited |
                      NSTrackingActiveInKeyWindow | NSTrackingInVisibleRect)
               owner:self
            userInfo:nil];
    [self addTrackingArea:area];
    [super updateTrackingAreas];
}

// --- Local cursor hiding: inside the session view only the remote (captured)
// cursor should be visible, so it feels like operating the remote Mac directly.
- (void)hideLocalCursor { if (!_cursorHidden) { [NSCursor hide]; _cursorHidden = YES; } }
- (void)showLocalCursor { if (_cursorHidden) { [NSCursor unhide]; _cursorHidden = NO; } }

- (void)mouseEntered:(NSEvent*)e { [self hideLocalCursor]; }
- (void)mouseExited:(NSEvent*)e  { [self showLocalCursor]; }

- (void)viewDidMoveToWindow {
    [super viewDidMoveToWindow];
    [[NSNotificationCenter defaultCenter] removeObserver:self];
    if (self.window) {
        // Restore the cursor if the window loses focus (e.g. Cmd-Tab away).
        [[NSNotificationCenter defaultCenter] addObserver:self
            selector:@selector(showLocalCursor)
                name:NSWindowDidResignKeyNotification object:self.window];
    }
}

- (void)dealloc {
    [self showLocalCursor];
    [[NSNotificationCenter defaultCenter] removeObserver:self];
    [super dealloc];
}

@end
