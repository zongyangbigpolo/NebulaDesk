//
// AudioPlayer.h - platform-neutral PCM playback interface (CWA/session side)
//
// macOS backend = AVAudioEngine; Windows = WASAPI; Linux = PipeWire.
//
#pragma once

#include "NebulaFrame.h"
#include <memory>

namespace nebula {

class IAudioPlayer {
public:
    virtual ~IAudioPlayer() = default;
    virtual bool start(uint32_t sampleRate, uint32_t channels) = 0;
    virtual void enqueue(const RawAudioFrame& pcm) = 0;
    virtual void stop() = 0;
};

std::unique_ptr<IAudioPlayer> CreateAudioPlayer();

} // namespace nebula
