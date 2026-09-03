//
// VirtualDisplay.mm - private CGVirtualDisplay wrapper (VDA side).
//
// Signatures verified by runtime introspection on macOS 26.x. Declared locally
// because CoreGraphics ships no public headers for these classes.
//
#include "VirtualDisplay.h"
#include "NebulaLog.h"

#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>

#define TAG "vdisp"

// --- Private CoreGraphics interface declarations ---------------------------
@interface CGVirtualDisplayDescriptor : NSObject
@property(nonatomic, strong) dispatch_queue_t queue;
@property(nonatomic, assign) uint32_t vendorID;
@property(nonatomic, assign) uint32_t productID;
@property(nonatomic, assign) uint32_t serialNum;
@property(nonatomic, copy)   NSString* name;
@property(nonatomic, assign) CGSize sizeInMillimeters;
@property(nonatomic, assign) uint32_t maxPixelsWide;
@property(nonatomic, assign) uint32_t maxPixelsHigh;
@property(nonatomic, assign) CGPoint redPrimary;
@property(nonatomic, assign) CGPoint greenPrimary;
@property(nonatomic, assign) CGPoint bluePrimary;
@property(nonatomic, assign) CGPoint whitePoint;
@property(nonatomic, copy)   void (^terminationHandler)(id, id);
@end

@interface CGVirtualDisplayMode : NSObject
- (instancetype)initWithWidth:(uint32_t)w height:(uint32_t)h refreshRate:(double)r;
@property(nonatomic, readonly) uint32_t width;
@property(nonatomic, readonly) uint32_t height;
@property(nonatomic, readonly) double refreshRate;
@end

@interface CGVirtualDisplaySettings : NSObject
@property(nonatomic, assign) uint32_t hiDPI;
@property(nonatomic, copy)   NSArray<CGVirtualDisplayMode*>* modes;
@property(nonatomic, assign) uint32_t rotation;
@end

@interface CGVirtualDisplay : NSObject
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor*)descriptor;
- (BOOL)applySettings:(CGVirtualDisplaySettings*)settings;
@property(nonatomic, readonly) uint32_t displayID;
@end

namespace nebula {
namespace {

class VirtualDisplay final : public IVirtualDisplay {
public:
    VirtualDisplay(uint32_t w, uint32_t h, uint32_t fps, bool hidpi) {
        @autoreleasepool {
            Class descCls = NSClassFromString(@"CGVirtualDisplayDescriptor");
            Class setCls  = NSClassFromString(@"CGVirtualDisplaySettings");
            Class modeCls = NSClassFromString(@"CGVirtualDisplayMode");
            Class dispCls = NSClassFromString(@"CGVirtualDisplay");
            if (!descCls || !setCls || !modeCls || !dispCls) {
                NSOperatingSystemVersion v = [[NSProcessInfo processInfo] operatingSystemVersion];
                NEBULA_LOGE(TAG,
                    "CGVirtualDisplay private API unavailable on this system (macOS %ld.%ld.%ld). "
                    "This API is undocumented and Apple can change or remove it in any release; "
                    "Nebula requires macOS 13+ and has been validated against macOS 26. If you are "
                    "on a much older or unusually customized macOS build, virtual-display capture "
                    "cannot proceed — there is no physical-display fallback (see ARCHITECTURE.md).",
                    (long)v.majorVersion, (long)v.minorVersion, (long)v.patchVersion);
                return;
            }

            CGVirtualDisplayDescriptor* desc = [[descCls alloc] init];
            desc.queue = dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0);
            desc.name = @"Nebula Virtual Display";
            desc.vendorID = 0x1234;
            desc.productID = 0x5678;
            desc.serialNum = 0x0001;
            desc.maxPixelsWide = w * (hidpi ? 2 : 1);
            desc.maxPixelsHigh = h * (hidpi ? 2 : 1);
            // ~109 ppi sizing so macOS picks a sensible default scale.
            desc.sizeInMillimeters = CGSizeMake(w / 109.0 * 25.4, h / 109.0 * 25.4);
            desc.redPrimary   = CGPointMake(0.640, 0.330);
            desc.greenPrimary = CGPointMake(0.300, 0.600);
            desc.bluePrimary  = CGPointMake(0.150, 0.060);
            desc.whitePoint   = CGPointMake(0.3127, 0.3290);
            desc.terminationHandler = ^(id, id) {};

            m_display = [[dispCls alloc] initWithDescriptor:desc];
            if (!m_display) {
                NEBULA_LOGE(TAG,
                    "CGVirtualDisplay initWithDescriptor failed for %ux%u — the OS refused to "
                    "create the virtual display (common causes: another virtual-display tool "
                    "already holds the maximum number of virtual displays, or this account/session "
                    "lacks the entitlement Screen Recording grants). Check System Settings > "
                    "Privacy & Security > Screen Recording, then relaunch.", w, h);
                return;
            }

            CGVirtualDisplaySettings* settings = [[setCls alloc] init];
            settings.hiDPI = hidpi ? 1 : 0;
            CGVirtualDisplayMode* mode =
                [[modeCls alloc] initWithWidth:w height:h refreshRate:(double)fps];
            settings.modes = @[ mode ];

            if (![m_display applySettings:settings]) {
                NEBULA_LOGE(TAG,
                    "CGVirtualDisplay applySettings failed for mode %ux%u@%ufps — the requested "
                    "geometry may be invalid or unsupported on this macOS version. Capture cannot "
                    "proceed for this CWA; ask the viewer to reconnect (a different logical screen "
                    "size may succeed).", w, h, fps);
                m_display = nil;
                return;
            }
            m_id = [m_display displayID];
            NEBULA_LOGI(TAG, "virtual display created id=%u %ux%u%s",
                      m_id, w, h, hidpi ? " (HiDPI)" : "");
        }
    }
    ~VirtualDisplay() override {
        // Releasing the CGVirtualDisplay removes the display from the system.
        m_display = nil;
        if (m_id) NEBULA_LOGI(TAG, "virtual display %u removed", m_id);
    }

    uint32_t displayId() const override { return m_id; }
    bool valid() const override { return m_id != 0; }

private:
    CGVirtualDisplay* m_display = nil;
    uint32_t m_id = 0;
};

} // namespace

std::unique_ptr<IVirtualDisplay> CreateVirtualDisplay(uint32_t w, uint32_t h,
                                                      uint32_t fps, bool hidpi) {
    auto vd = std::make_unique<VirtualDisplay>(w, h, fps, hidpi);
    if (!vd->valid()) return nullptr;
    return vd;
}

} // namespace nebula
