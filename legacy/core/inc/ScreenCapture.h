//
// ScreenCapture.h - ScreenCaptureKit screen + system audio capture (VDA side)
//
#pragma once

#include "NebulaTypes.h"
#include "NebulaFrame.h"
#include <cstdint>
#include <functional>
#include <memory>

namespace nebula {

class IScreenCapture {
public:
    using VideoCb = std::function<void(const RawVideoFrame& frame)>;
    using AudioCb = std::function<void(const RawAudioFrame& frame)>;

    virtual ~IScreenCapture() = default;
    virtual bool start(const VideoConfig& video, const AudioConfig& audio) = 0;
    virtual uint32_t displayId() const = 0;
    virtual void setVideoOutput(VideoCb cb) = 0;
    virtual void setAudioOutput(AudioCb cb) = 0;
    virtual void stop() = 0;
};

std::unique_ptr<IScreenCapture> CreateScreenCapture();

} // namespace nebula
