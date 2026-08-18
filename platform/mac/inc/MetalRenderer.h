//
// MetalRenderer.h - Metal NV12 -> RGB renderer (CWA side)
//
#pragma once

#ifdef __OBJC__
#import <QuartzCore/CAMetalLayer.h>
#import <CoreVideo/CoreVideo.h>

// Renders decoded NV12 CVPixelBuffers to a CAMetalLayer with zero CPU copy.
@interface NebulaMetalRenderer : NSObject
- (instancetype)initWithLayer:(CAMetalLayer*)layer;
- (void)renderPixelBuffer:(CVImageBufferRef)pixelBuffer;
- (void)setDrawableSize:(CGSize)size;
@end
#endif
