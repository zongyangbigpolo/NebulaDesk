//
// AppDelegate.h - CWA Cocoa application delegate
//
#pragma once
#ifdef __OBJC__
#import <Cocoa/Cocoa.h>
#include <string>

@interface NebulaAppDelegate : NSObject <NSApplicationDelegate>
@property (nonatomic) std::string host;
@property (nonatomic) uint16_t port;
@property (nonatomic) std::string psk;
@property (nonatomic) std::string title;
@property (nonatomic) std::string relayHost;
@property (nonatomic) uint16_t relayPort;
@property (nonatomic) std::string deviceId;
@property (nonatomic) std::string token;
@end
#endif
