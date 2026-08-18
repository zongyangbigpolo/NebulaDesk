//
// VideoEncoder.h - VideoToolbox hardware H.264/HEVC encoder (VDA side)
//
#pragma once

#include "NebulaTypes.h"
#include "NebulaFrame.h"
#include <functional>
#include <memory>

namespace nebula {

class IVideoEncoder {
public:
    // Emits encoded frames. `config==true` carries codec parameter sets.
    using OutputCb = std::function<void(const EncodedFrame&)>;

    virtual ~IVideoEncoder() = default;
    virtual bool start(const VideoConfig& cfg) = 0;
    virtual void setOutput(OutputCb cb) = 0;
    // Feed a captured frame. The backend may consume frame.nativeHandle for a
    // zero-copy path (e.g. CVPixelBufferRef on macOS).
    virtual void encode(const RawVideoFrame& frame) = 0;
    virtual void forceKeyframe() = 0;
    virtual void stop() = 0;
};

std::unique_ptr<IVideoEncoder> CreateVideoEncoder();

} // namespace nebula
