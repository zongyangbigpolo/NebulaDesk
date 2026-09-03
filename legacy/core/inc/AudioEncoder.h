//
// AudioEncoder.h - AudioToolbox AAC encoder (VDA side)
//
#pragma once

#include "NebulaTypes.h"
#include "NebulaFrame.h"
#include <functional>
#include <memory>

namespace nebula {

class IAudioEncoder {
public:
    using OutputCb = std::function<void(const EncodedFrame&)>;

    virtual ~IAudioEncoder() = default;
    virtual bool start(const AudioConfig& cfg) = 0;
    virtual void setOutput(OutputCb cb) = 0;
    // Feed a captured PCM frame. The backend may consume frame.nativeHandle
    // (e.g. CMSampleBufferRef on macOS) for a zero-copy path.
    virtual void encode(const RawAudioFrame& frame) = 0;
    virtual void stop() = 0;
};

std::unique_ptr<IAudioEncoder> CreateAudioEncoder();

} // namespace nebula
