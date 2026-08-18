//
// AudioPlayer.mm - AVAudioEngine playback implementation
//
#include "AudioPlayer.h"
#include "NebulaLog.h"

#import <AVFoundation/AVFoundation.h>

#define TAG "aplay"

@implementation NebulaAudioPlayer {
    AVAudioEngine*      _engine;
    AVAudioPlayerNode*  _player;
    AVAudioFormat*      _format;
    uint32_t            _channels;
}

- (instancetype)initWithSampleRate:(double)sampleRate channels:(uint32_t)channels {
    if ((self = [super init])) {
        _channels = channels;
        _engine = [[AVAudioEngine alloc] init];
        _player = [[AVAudioPlayerNode alloc] init];
        _format = [[AVAudioFormat alloc] initStandardFormatWithSampleRate:sampleRate
                                                                 channels:channels];
        [_engine attachNode:_player];
        [_engine connect:_player to:_engine.mainMixerNode format:_format];
    }
    return self;
}

- (void)start {
    NSError* err = nil;
    [_engine startAndReturnError:&err];
    if (err) { nebula::LogWrite(nebula::LogLevel::Error, TAG, "engine start failed: %s", err.localizedDescription.UTF8String); return; }
    [_player play];
    nebula::LogWrite(nebula::LogLevel::Info, TAG, "audio playback started");
}

// Convert interleaved Float32 -> AVAudioPCMBuffer (non-interleaved) and schedule.
- (void)enqueueInterleaved:(const float*)pcm frames:(uint32_t)frames {
    if (!pcm || frames == 0) return;
    AVAudioPCMBuffer* buf = [[AVAudioPCMBuffer alloc] initWithPCMFormat:_format
                                                         frameCapacity:frames];
    buf.frameLength = frames;
    float* const* dst = buf.floatChannelData;
    for (uint32_t c = 0; c < _channels; ++c) {
        for (uint32_t i = 0; i < frames; ++i) {
            dst[c][i] = pcm[i * _channels + c];
        }
    }
    [_player scheduleBuffer:buf completionHandler:nil];
}

- (void)stop {
    [_player stop];
    [_engine stop];
}

@end
