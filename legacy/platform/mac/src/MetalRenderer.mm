//
// MetalRenderer.mm - Metal NV12 -> RGB rendering implementation
//
#include "MetalRenderer.h"
#include "NebulaLog.h"

#import <Metal/Metal.h>
#import <CoreVideo/CoreVideo.h>

#define TAG "metal"

// Fullscreen-triangle vertex shader + BT.709 video-range NV12 -> RGB fragment shader.
static NSString* const kShaderSource = @R"(
#include <metal_stdlib>
using namespace metal;

struct VSOut {
    float4 pos [[position]];
    float2 uv;
};

vertex VSOut vs_main(uint vid [[vertex_id]]) {
    float2 p[3] = { float2(-1.0, -3.0), float2(-1.0, 1.0), float2(3.0, 1.0) };
    float2 t[3] = { float2(0.0, 2.0),   float2(0.0, 0.0),  float2(2.0, 0.0) };
    VSOut o;
    o.pos = float4(p[vid], 0.0, 1.0);
    o.uv = t[vid];
    return o;
}

fragment float4 fs_main(VSOut in [[stage_in]],
                        texture2d<float> yTex [[texture(0)]],
                        texture2d<float> cbcrTex [[texture(1)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float y  = yTex.sample(s, in.uv).r;
    float2 cbcr = cbcrTex.sample(s, in.uv).rg;
    // BT.709, video range
    float Y = (y - 16.0/255.0) * 1.16438;
    float Cb = cbcr.r - 128.0/255.0;
    float Cr = cbcr.g - 128.0/255.0;
    float r = Y + 1.79274 * Cr;
    float g = Y - 0.21325 * Cb - 0.53291 * Cr;
    float b = Y + 2.11240 * Cb;
    return float4(r, g, b, 1.0);
}
)";

@implementation NebulaMetalRenderer {
    CAMetalLayer*               _layer;
    id<MTLDevice>               _device;
    id<MTLCommandQueue>         _queue;
    id<MTLRenderPipelineState>  _pipeline;
    CVMetalTextureCacheRef      _texCache;
}

- (instancetype)initWithLayer:(CAMetalLayer*)layer {
    if ((self = [super init])) {
        _layer = layer;
        _device = MTLCreateSystemDefaultDevice();
        _queue = [_device newCommandQueue];
        _layer.device = _device;
        _layer.pixelFormat = MTLPixelFormatBGRA8Unorm;
        _layer.framebufferOnly = YES;

        CVMetalTextureCacheCreate(kCFAllocatorDefault, nullptr, _device, nullptr, &_texCache);

        NSError* err = nil;
        id<MTLLibrary> lib = [_device newLibraryWithSource:kShaderSource options:nil error:&err];
        if (!lib) { nebula::LogWrite(nebula::LogLevel::Error, TAG, "shader compile failed: %s", err.localizedDescription.UTF8String); return self; }

        MTLRenderPipelineDescriptor* desc = [[MTLRenderPipelineDescriptor alloc] init];
        desc.vertexFunction = [lib newFunctionWithName:@"vs_main"];
        desc.fragmentFunction = [lib newFunctionWithName:@"fs_main"];
        desc.colorAttachments[0].pixelFormat = MTLPixelFormatBGRA8Unorm;
        _pipeline = [_device newRenderPipelineStateWithDescriptor:desc error:&err];
        if (!_pipeline) nebula::LogWrite(nebula::LogLevel::Error, TAG, "pipeline failed: %s", err.localizedDescription.UTF8String);
    }
    return self;
}

- (void)setDrawableSize:(CGSize)size {
    _layer.drawableSize = size;
}

- (id<MTLTexture>)textureFromPixelBuffer:(CVPixelBufferRef)pb plane:(size_t)plane format:(MTLPixelFormat)fmt {
    size_t w = CVPixelBufferGetWidthOfPlane(pb, plane);
    size_t h = CVPixelBufferGetHeightOfPlane(pb, plane);
    CVMetalTextureRef cvTex = nullptr;
    CVReturn r = CVMetalTextureCacheCreateTextureFromImage(
        kCFAllocatorDefault, _texCache, pb, nullptr, fmt, w, h, plane, &cvTex);
    if (r != kCVReturnSuccess || !cvTex) return nil;
    id<MTLTexture> tex = CVMetalTextureGetTexture(cvTex);
    CFRelease(cvTex);
    return tex;
}

- (void)renderPixelBuffer:(CVImageBufferRef)pixelBuffer {
    if (!_pipeline || !pixelBuffer) return;
    CVPixelBufferRef pb = (CVPixelBufferRef)pixelBuffer;

    id<MTLTexture> yTex    = [self textureFromPixelBuffer:pb plane:0 format:MTLPixelFormatR8Unorm];
    id<MTLTexture> cbcrTex = [self textureFromPixelBuffer:pb plane:1 format:MTLPixelFormatRG8Unorm];
    if (!yTex || !cbcrTex) return;

    id<CAMetalDrawable> drawable = [_layer nextDrawable];
    if (!drawable) return;

    MTLRenderPassDescriptor* rp = [MTLRenderPassDescriptor renderPassDescriptor];
    rp.colorAttachments[0].texture = drawable.texture;
    rp.colorAttachments[0].loadAction = MTLLoadActionClear;
    rp.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 1);
    rp.colorAttachments[0].storeAction = MTLStoreActionStore;

    id<MTLCommandBuffer> cmd = [_queue commandBuffer];
    id<MTLRenderCommandEncoder> enc = [cmd renderCommandEncoderWithDescriptor:rp];
    [enc setRenderPipelineState:_pipeline];
    [enc setFragmentTexture:yTex atIndex:0];
    [enc setFragmentTexture:cbcrTex atIndex:1];
    [enc drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
    [enc endEncoding];
    [cmd presentDrawable:drawable];
    [cmd commit];

    CVMetalTextureCacheFlush(_texCache, 0);
}

- (void)dealloc {
    if (_texCache) CFRelease(_texCache);
    [super dealloc];
}

@end
