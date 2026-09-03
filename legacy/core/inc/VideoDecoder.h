//
// VideoDecoder.h - VideoToolbox hardware decoder (CWA side)
//
#pragma once

#include "NebulaTypes.h"
#include "NebulaFrame.h"
#include <functional>
#include <memory>

namespace nebula {

class IVideoDecoder {
public:
    // Delivers a decoded frame. frame.nativeHandle carries the platform image
    // buffer (e.g. CVImageBufferRef) for zero-copy hand-off to the renderer.
    using FrameCb = std::function<void(const RawVideoFrame& frame)>;

    virtual ~IVideoDecoder() = default;
    virtual bool start(const VideoConfig& cfg) = 0;
    virtual void setOutput(FrameCb cb) = 0;
    // Rebuilds the format description from serialized parameter sets.
    virtual void setConfig(const uint8_t* data, size_t len) = 0;
    virtual void decode(const uint8_t* data, size_t len, bool keyframe, uint64_t ptsUs) = 0;
    virtual void stop() = 0;
};

std::unique_ptr<IVideoDecoder> CreateVideoDecoder();

} // namespace nebula
