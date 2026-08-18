//
// AppDelegate.mm - Cocoa window hosting a Metal layer, wired to CwaClient
//
#import <Cocoa/Cocoa.h>
#import <QuartzCore/CAMetalLayer.h>

#include "AppDelegate.h"
#include "CwaClient.h"
#include "MetalRenderer.h"
#include "AudioPlayer.h"
#include "InputView.h"
#include "SessionStatus.h"
#include "NebulaLog.h"

#include <fstream>
#include <sstream>
#include <string>
#include <atomic>
#include <cstdlib>
#include <utility>
#include <vector>

namespace {

// A tiny on-disk cache mapping relay device-id -> the most recent reconnect
// ticket the relay issued us, so a later launch can skip presenting the
// long-lived shared token. Format: one "deviceId<TAB>ticket" line per device.
std::string TicketCachePath() {
    const char* home = getenv("HOME");
    std::string dir = (home ? std::string(home) : ".") + "/Library/Application Support/Nebula";
    // Best-effort directory creation; ignore failure (falls back to no cache).
    std::string mkdirCmd = "mkdir -p '" + dir + "'";
    std::system(mkdirCmd.c_str());
    return dir + "/relay_tickets.txt";
}

std::string LoadCachedTicket(const std::string& deviceId) {
    std::ifstream in(TicketCachePath());
    std::string line;
    while (std::getline(in, line)) {
        auto tab = line.find('\t');
        if (tab == std::string::npos) continue;
        if (line.substr(0, tab) == deviceId) return line.substr(tab + 1);
    }
    return {};
}

void SaveCachedTicket(const std::string& deviceId, const std::string& ticket) {
    if (deviceId.empty() || ticket.empty()) return;
    std::string path = TicketCachePath();
    std::vector<std::pair<std::string, std::string>> entries;
    {
        std::ifstream in(path);
        std::string line;
        while (std::getline(in, line)) {
            auto tab = line.find('\t');
            if (tab == std::string::npos) continue;
            std::string id = line.substr(0, tab);
            if (id != deviceId) entries.emplace_back(id, line.substr(tab + 1));
        }
    }
    entries.emplace_back(deviceId, ticket);
    std::ofstream out(path, std::ios::trunc);
    for (auto& e : entries) out << e.first << '\t' << e.second << '\n';
}

} // namespace

@implementation NebulaAppDelegate {
    NSWindow*           _window;
    NebulaInputView*      _view;
    CAMetalLayer*       _metalLayer;
    NebulaMetalRenderer*  _renderer;
    NebulaAudioPlayer*    _audioPlayer;
    nebula::CwaClient     _client;
    std::atomic<int>    _framesInFlight;
    std::atomic<bool>   _streaming;
}

- (void)applicationDidFinishLaunching:(NSNotification*)note {
    nebula::VideoConfig vcfg;
    NSRect frame = NSMakeRect(0, 0, 1280, 720);
    _window = [[NSWindow alloc] initWithContentRect:frame
                                          styleMask:(NSWindowStyleMaskTitled |
                                                     NSWindowStyleMaskClosable |
                                                     NSWindowStyleMaskResizable)
                                            backing:NSBackingStoreBuffered
                                              defer:NO];
    NSString* t = _title.empty() ? @"Nebula session" : [NSString stringWithUTF8String:_title.c_str()];
    [_window setTitle:t];
    [_window center];

    // Input-capturing view hosts the Metal layer.
    _view = [[NebulaInputView alloc] initWithFrame:[[_window contentView] bounds]];
    _view.wantsLayer = YES;
    _metalLayer = [CAMetalLayer layer];
    _metalLayer.frame = _view.bounds;
    _metalLayer.autoresizingMask = kCALayerWidthSizable | kCALayerHeightSizable;
    // High-refresh / low-latency presentation: triple buffering + vsync so the
    // independent session process can drive ProMotion (120Hz) smoothly.
    _metalLayer.maximumDrawableCount = 3;
    _metalLayer.displaySyncEnabled = YES;
    _metalLayer.presentsWithTransaction = NO;
    [_view setLayer:_metalLayer];
    [_window setContentView:_view];

    nebula::ReportSessionStatus(nebula::SessionState::Connecting);

    __unsafe_unretained NebulaAppDelegate* weakForInput = self;
    _view.onInput = [weakForInput](const nebula::NebulaInputEvent& e) {
        NebulaAppDelegate* self = weakForInput;
        if (self) self->_client.sendInput(e);
    };

    _renderer = [[NebulaMetalRenderer alloc] initWithLayer:_metalLayer];
    [_renderer setDrawableSize:CGSizeMake(vcfg.width, vcfg.height)];

    [_window makeKeyAndOrderFront:nil];
    [_window makeFirstResponder:_view];
    [NSApp activateIgnoringOtherApps:YES];

    [self startClient];
}

- (void)startClient {
    nebula::NebulaCaps requested; // defaults: HEVC, 60fps, AAC 48k stereo

    // HELLO always carries this CWA's main-screen logical dimensions. The VDA
    // must create and capture a virtual display with this exact geometry.
    NSScreen* scr = [NSScreen mainScreen];
    NSRect fr = scr.frame;
    requested.video.useVirtualDisplay = true;
    requested.video.width  = (uint32_t)fr.size.width;
    requested.video.height = (uint32_t)fr.size.height;
    nebula::LogWrite(nebula::LogLevel::Info, "session",
                     "requesting mandatory virtual display %ux%u",
                     requested.video.width, requested.video.height);

    _client.setPreSharedKey(_psk);
    if (!_relayHost.empty() && !_deviceId.empty()) {
        std::string cachedTicket = LoadCachedTicket(_deviceId);
        _client.useRelay(_relayHost, _relayPort, _deviceId, _token, cachedTicket);
    }

    __unsafe_unretained NebulaAppDelegate* weakSelf = self;

    _client.setReadyCallback([weakSelf](const nebula::NebulaCaps& caps) {
        nebula::ReportSessionStatus(nebula::SessionState::Connected);
        dispatch_async(dispatch_get_main_queue(), ^{
            NebulaAppDelegate* self = weakSelf;
            if (!self) return;
            if (!self->_deviceId.empty()) {
                SaveCachedTicket(self->_deviceId, self->_client.relayTicket());
            }
            self->_audioPlayer = [[NebulaAudioPlayer alloc]
                initWithSampleRate:caps.audio.sampleRate channels:caps.audio.channels];
            [self->_audioPlayer start];

            // Render at the VDA's full source resolution (sharp), and size the
            // window to match it 1:1 so text looks native — shrinking only if the
            // remote screen is larger than this Mac's usable screen area.
            CGFloat vw = caps.video.width, vh = caps.video.height;
            if (vw > 0 && vh > 0) {
                [self->_renderer setDrawableSize:CGSizeMake(vw, vh)];
                NSRect vis = [[NSScreen mainScreen] visibleFrame];
                CGFloat fit = fmin(1.0, fmin(vis.size.width / vw, vis.size.height / vh));
                NSSize content = NSMakeSize(floor(vw * fit), floor(vh * fit));
                [self->_window setContentSize:content];
                [self->_window setContentAspectRatio:NSMakeSize(vw, vh)];
                [self->_window center];
            }
        });
    });

    _client.setVideoFrameCallback([weakSelf](const nebula::RawVideoFrame& frame) {
        NebulaAppDelegate* self = weakSelf;
        if (!self) return;
        CVImageBufferRef img = (CVImageBufferRef)frame.nativeHandle;
        if (!img) return;
        if (!self->_streaming.exchange(true)) nebula::ReportSessionStatus(nebula::SessionState::Streaming);
        // Latest-wins backpressure: if the renderer is behind, drop this frame
        // instead of queueing it, keeping end-to-end latency low.
        if (self->_framesInFlight.load() >= 2) return;
        self->_framesInFlight.fetch_add(1);
        CVPixelBufferRetain(img);
        dispatch_async(dispatch_get_main_queue(), ^{
            NebulaAppDelegate* s = weakSelf;
            if (s) [s->_renderer renderPixelBuffer:img];
            CVPixelBufferRelease(img);
            self->_framesInFlight.fetch_sub(1);
        });
    });

    _client.setAudioPcmCallback([weakSelf](const nebula::RawAudioFrame& f) {
        NebulaAppDelegate* self = weakSelf;
        if (self && self->_audioPlayer && f.samples)
            [self->_audioPlayer enqueueInterleaved:f.samples frames:f.frames];
    });

    if (!_client.connect(_host, _port, requested)) {
        NEBULA_LOGE("session", "failed to connect to %s:%u", _host.c_str(), _port);
        nebula::ReportSessionStatus(nebula::SessionState::Error);
    }
}

- (void)applicationWillTerminate:(NSNotification*)note {
    nebula::ReportSessionStatus(nebula::SessionState::Disconnected);
    _client.stop();
    [_audioPlayer stop];
}

- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication*)sender { return YES; }

@end
