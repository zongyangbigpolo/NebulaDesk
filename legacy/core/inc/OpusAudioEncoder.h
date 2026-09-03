//
// OpusAudioEncoder.h - portable Opus encoder (for the WebRTC path)
//
// Real browsers' WebRTC audio only interoperates with Opus (RFC 6716), not
// AAC — so this is a second, independent encoder alongside the existing
// AudioToolbox AAC path (AudioEncoder.h), used only when a WebRTC viewer is
// active. Pure C++ over libopus: no Apple frameworks, portable to any future
// platform backend.
//
#pragma once

#include "NebulaTypes.h"
#include "NebulaFrame.h"
#include <functional>
#include <memory>

namespace nebula {

class IOpusAudioEncoder {
public:
    using OutputCb = std::function<void(const EncodedFrame&)>;

    virtual ~IOpusAudioEncoder() = default;
    virtual bool start(const AudioConfig& cfg) = 0;
    virtual void setOutput(OutputCb cb) = 0;
    // Feed a captured PCM frame. Reads frame.samples (interleaved float32) —
    // unlike the AAC encoder, this path does NOT use frame.nativeHandle, so
    // it works from any backend that populates the portable fields.
    virtual void encode(const RawAudioFrame& frame) = 0;
    virtual void stop() = 0;
};

std::unique_ptr<IOpusAudioEncoder> CreateOpusAudioEncoder();

} // namespace nebula
