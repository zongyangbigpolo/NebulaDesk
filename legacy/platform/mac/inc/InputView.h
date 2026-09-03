//
// InputView.h - NSView that hosts the Metal layer and captures mouse/keyboard
//
#pragma once
#ifdef __OBJC__
#import <Cocoa/Cocoa.h>
#include <functional>
#include "NebulaInput.h"

// Captures local mouse/keyboard and forwards normalized NebulaInputEvents.
@interface NebulaInputView : NSView
@property (nonatomic, assign) std::function<void(const nebula::NebulaInputEvent&)> onInput;
@end
#endif
