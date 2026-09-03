//
// ScreenCapture.mm - ScreenCaptureKit capture implementation
//
#include "ScreenCapture.h"
#include "VirtualDisplay.h"
#include "NebulaLog.h"

#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreGraphics/CoreGraphics.h>
#include <cstring>
#include <unistd.h>
#include <vector>

#define TAG "scap"

// ---------------------------------------------------------------------------
// Objective-C output sink bridging SCStream callbacks to C++ std::functions.
// Emits platform-neutral RawVideoFrame / RawAudioFrame that carry the native
// buffer in nativeHandle for zero-copy hand-off to the mac encoder.
// ---------------------------------------------------------------------------
@interface NebulaCaptureSink : NSObject <SCStreamOutput, SCStreamDelegate>
@property (nonatomic, assign) std::function<void(const nebula::RawVideoFrame&)> videoCb;
@property (nonatomic, assign) std::function<void(const nebula::RawAudioFrame&)> audioCb;
@end

@implementation NebulaCaptureSink

- (void)stream:(SCStream *)stream
        didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
        ofType:(SCStreamOutputType)type {
    if (!CMSampleBufferIsValid(sampleBuffer)) return;

    if (type == SCStreamOutputTypeScreen) {
        // Drop frames whose status is not "complete".
        CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
        if (attachments && CFArrayGetCount(attachments)) {
            CFDictionaryRef d = (CFDictionaryRef)CFArrayGetValueAtIndex(attachments, 0);
            CFNumberRef statusRef = (CFNumberRef)CFDictionaryGetValue(d, (const void*)SCStreamFrameInfoStatus);
            int status = 0;
            if (statusRef) CFNumberGetValue(statusRef, kCFNumberIntType, &status);
            if (status != SCFrameStatusComplete) return;
        }
        CVPixelBufferRef pb = CMSampleBufferGetImageBuffer(sampleBuffer);
        if (pb && _videoCb) {
            CMTime pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
            nebula::RawVideoFrame f;
            f.format = nebula::PixelFormat::NV12;
            f.width  = (uint32_t)CVPixelBufferGetWidth(pb);
            f.height = (uint32_t)CVPixelBufferGetHeight(pb);
            f.timestampUs = (uint64_t)(CMTimeGetSeconds(pts) * 1000000.0);
            f.nativeHandle = pb; // zero-copy: encoder reads the CVPixelBuffer directly
            _videoCb(f);
        }
    } else if (type == SCStreamOutputTypeAudio) {
        if (_audioCb) {
            nebula::RawAudioFrame f;
            f.nativeHandle = sampleBuffer; // AAC path: encoder reads the CMSampleBuffer directly

            // Also populate the portable interleaved-float view so platform-
            // neutral consumers (e.g. the WebRTC/Opus path in core/) don't
            // need any Apple types. This is a real copy+interleave, done once
            // per audio callback; negligible cost relative to video capture.
            std::vector<float> interleaved;
            AudioBufferList abl;
            CMBlockBufferRef blockBuffer = nullptr;
            OSStatus st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
                sampleBuffer, nullptr, &abl, sizeof(abl), nullptr, nullptr,
                kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, &blockBuffer);
            if (st == noErr && abl.mNumberBuffers > 0) {
                const CMAudioFormatDescriptionRef fmtDesc =
                    CMSampleBufferGetFormatDescription(sampleBuffer);
                const AudioStreamBasicDescription* asbd =
                    fmtDesc ? CMAudioFormatDescriptionGetStreamBasicDescription(fmtDesc) : nullptr;
                uint32_t channels = asbd ? asbd->mChannelsPerFrame : abl.mNumberBuffers;
                uint32_t sampleRate = asbd ? (uint32_t)asbd->mSampleRate : 48000;
                uint32_t frames = channels ? (uint32_t)(abl.mBuffers[0].mDataByteSize / sizeof(float)) : 0;
                bool nonInterleaved = channels > 0 && abl.mNumberBuffers == channels;
                if (frames > 0 && channels > 0) {
                    interleaved.resize((size_t)frames * channels);
                    if (nonInterleaved) {
                        for (uint32_t ch = 0; ch < channels; ++ch) {
                            const float* src = (const float*)abl.mBuffers[ch].mData;
                            if (!src) continue;
                            for (uint32_t i = 0; i < frames; ++i) interleaved[i * channels + ch] = src[i];
                        }
                    } else {
                        const float* src = (const float*)abl.mBuffers[0].mData;
                        if (src) std::memcpy(interleaved.data(), src, interleaved.size() * sizeof(float));
                    }
                    f.samples = interleaved.data();
                    f.frames = frames;
                    f.channels = channels;
                    f.sampleRate = sampleRate;
                }
            }
            if (blockBuffer) CFRelease(blockBuffer);

            CMTime pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
            f.timestampUs = (uint64_t)(CMTimeGetSeconds(pts) * 1000000.0);
            _audioCb(f);
        }
    }
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    nebula::LogWrite(nebula::LogLevel::Error, TAG, "stream stopped: %s",
                   error.localizedDescription.UTF8String);
}

@end

namespace nebula {
namespace {

class ScreenCapture final : public IScreenCapture {
public:
    ~ScreenCapture() override { stop(); }

    void setVideoOutput(VideoCb cb) override { m_videoCb = std::move(cb); }
    void setAudioOutput(AudioCb cb) override { m_audioCb = std::move(cb); }
    uint32_t displayId() const override {
        return m_virtual ? m_virtual->displayId() : 0;
    }

    bool start(const VideoConfig& video, const AudioConfig& audio) override {
        if (!video.useVirtualDisplay || !video.width || !video.height) {
            NEBULA_LOGE(TAG, "virtual display capture requires valid dimensions");
            return false;
        }
        m_virtual = CreateVirtualDisplay(video.width, video.height, video.fps, /*hidpi=*/false);
        if (!m_virtual) {
            NEBULA_LOGE(TAG,
                "failed to create mandatory virtual display %ux%u — see the preceding "
                "CGVirtualDisplay error above for the specific reason. Capture cannot fall back "
                "to the physical display by design (see ARCHITECTURE.md); fix the underlying "
                "issue and have the viewer reconnect.",
                video.width, video.height);
            return false;
        }
        uint32_t targetDisplayId = m_virtual->displayId();
        NEBULA_LOGI(TAG, "using virtual display id=%u", targetDisplayId);
        usleep(300 * 1000);

        __block SCDisplay* display = nil;
        __block bool sawPermissionError = false;
        // The virtual display may take a moment to register; retry a few times.
        int attempts = 10;
        for (int attempt = 0; attempt < attempts && !display; ++attempt) {
            dispatch_semaphore_t sem = dispatch_semaphore_create(0);
            [SCShareableContent getShareableContentWithCompletionHandler:
                ^(SCShareableContent* content, NSError* error) {
                if (error) {
                    NEBULA_LOGE(TAG, "getShareableContent failed: %s", error.localizedDescription.UTF8String);
                    // SCStreamErrorUserDeclined / permission-denied surfaces here — this is the
                    // most common real-world cause of "no displays visible to ScreenCaptureKit".
                    if ([error.localizedDescription rangeOfString:@"declined" options:NSCaseInsensitiveSearch].location != NSNotFound ||
                        [error.localizedDescription rangeOfString:@"permission" options:NSCaseInsensitiveSearch].location != NSNotFound) {
                        sawPermissionError = true;
                    }
                } else {
                    for (SCDisplay* d in content.displays) {
                        if (d.displayID == targetDisplayId) { display = d; break; }
                    }
                }
                dispatch_semaphore_signal(sem);
            }];
            dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));
            if (!display) usleep(200 * 1000);
        }
        if (!display) {
            if (sawPermissionError) {
                NEBULA_LOGE(TAG,
                    "mandatory virtual display %u unavailable — Screen Recording permission looks "
                    "missing or revoked. Grant it in System Settings > Privacy & Security > Screen "
                    "Recording for this app, then relaunch nebula_vda (macOS does not let an app "
                    "re-request this mid-session).", targetDisplayId);
            } else {
                NEBULA_LOGE(TAG,
                    "mandatory virtual display %u unavailable in ScreenCaptureKit after %d attempts "
                    "— it may not have finished registering with the OS in time, or was rejected. "
                    "No physical-display fallback is used by design.", targetDisplayId, attempts);
            }
            return false;
        }

        SCContentFilter* filter =
            [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];

        SCStreamConfiguration* cfg = [[SCStreamConfiguration alloc] init];
        cfg.width  = video.width;
        cfg.height = video.height;
        cfg.minimumFrameInterval = CMTimeMake(1, (int32_t)video.fps);
        cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        cfg.queueDepth = 6;
        cfg.showsCursor = YES;
        if (@available(macOS 13.0, *)) {
            cfg.capturesAudio = YES;
            cfg.sampleRate    = audio.sampleRate;
            cfg.channelCount  = audio.channels;
        }

        m_sink = [[NebulaCaptureSink alloc] init];
        m_sink.videoCb = m_videoCb;
        m_sink.audioCb = m_audioCb;

        m_stream = [[SCStream alloc] initWithFilter:filter configuration:cfg delegate:m_sink];

        NSError* err = nil;
        m_videoQueue = dispatch_queue_create("com.nebula.capture.video", DISPATCH_QUEUE_SERIAL);
        [m_stream addStreamOutput:m_sink type:SCStreamOutputTypeScreen
                sampleHandlerQueue:m_videoQueue error:&err];
        if (err) { NEBULA_LOGE(TAG, "addStreamOutput(screen) failed"); return false; }

        if (@available(macOS 13.0, *)) {
            m_audioQueue = dispatch_queue_create("com.nebula.capture.audio", DISPATCH_QUEUE_SERIAL);
            [m_stream addStreamOutput:m_sink type:SCStreamOutputTypeAudio
                    sampleHandlerQueue:m_audioQueue error:&err];
            if (err) { NEBULA_LOGW(TAG, "addStreamOutput(audio) failed (continuing video-only)"); err = nil; }
        }

        __block bool ok = false;
        dispatch_semaphore_t startSem = dispatch_semaphore_create(0);
        [m_stream startCaptureWithCompletionHandler:^(NSError* e) {
            if (e) NEBULA_LOGE(TAG, "startCapture failed: %s", e.localizedDescription.UTF8String);
            else   ok = true;
            dispatch_semaphore_signal(startSem);
        }];
        dispatch_semaphore_wait(startSem, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));

        if (ok) NEBULA_LOGI(TAG, "screen capture started %ux%u", (unsigned)video.width, (unsigned)video.height);
        return ok;
    }

    void stop() override {
        if (m_stream) {
            [m_stream stopCaptureWithCompletionHandler:^(NSError*){}];
            m_stream = nil;
        }
        m_sink = nil;
        m_virtual.reset(); // removes the virtual display, if any
    }

private:
    SCStream*        m_stream = nil;
    NebulaCaptureSink* m_sink   = nil;
    dispatch_queue_t m_videoQueue = nullptr;
    dispatch_queue_t m_audioQueue = nullptr;
    std::unique_ptr<IVirtualDisplay> m_virtual;
    VideoCb          m_videoCb;
    AudioCb          m_audioCb;
};

} // namespace

std::unique_ptr<IScreenCapture> CreateScreenCapture() {
    return std::make_unique<ScreenCapture>();
}

} // namespace nebula
