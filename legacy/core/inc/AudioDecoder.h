//
// AudioDecoder.h - AAC -> PCM decoder (CWA side)
//
#pragma once

#include "NebulaTypes.h"
#include "NebulaFrame.h"
#include <cstdint>
#include <functional>
#include <memory>

namespace nebula {

class IAudioDecoder {
public:
    // Interleaved Float32 PCM output as a platform-neutral frame.
    using PcmCb = std::function<void(const RawAudioFrame& pcm)>;

    virtual ~IAudioDecoder() = default;
    virtual bool start(const AudioConfig& cfg) = 0;
    virtual void setOutput(PcmCb cb) = 0;
    virtual void setMagicCookie(const uint8_t* data, size_t len) = 0;
    virtual void decode(const uint8_t* aac, size_t len) = 0;
    virtual void stop() = 0;
};

std::unique_ptr<IAudioDecoder> CreateAudioDecoder();

} // namespace nebula
