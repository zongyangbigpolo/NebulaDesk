//
// AudioPlayer.h - AVAudioEngine PCM playback (CWA side)
//
#pragma once

#ifdef __OBJC__
#import <Foundation/Foundation.h>

@interface NebulaAudioPlayer : NSObject
- (instancetype)initWithSampleRate:(double)sampleRate channels:(uint32_t)channels;
- (void)start;
- (void)enqueueInterleaved:(const float*)pcm frames:(uint32_t)frames;
- (void)stop;
@end
#endif
