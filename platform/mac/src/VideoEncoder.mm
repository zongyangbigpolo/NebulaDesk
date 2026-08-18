//
// VideoEncoder.mm - VideoToolbox hardware encoder implementation
//
#include "VideoEncoder.h"
#include "AnnexB.h"
#include "NebulaLog.h"

#import <VideoToolbox/VideoToolbox.h>
#import <CoreMedia/CoreMedia.h>

#include <vector>
#include <atomic>

#define TAG "venc"

namespace nebula {
namespace {

// Serialize parameter sets as: count(1) then [len(4 LE) + bytes]...
std::vector<uint8_t> SerializeParameterSets(CMFormatDescriptionRef fmt, bool hevc) {
    std::vector<uint8_t> out;
    size_t count = 0;
    int nalHeaderLen = 0;
    if (hevc) {
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, 0, nullptr, nullptr, &count, &nalHeaderLen);
    } else {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, 0, nullptr, nullptr, &count, &nalHeaderLen);
    }
    out.push_back((uint8_t)count);
    for (size_t i = 0; i < count; ++i) {
        const uint8_t* ps = nullptr;
        size_t psSize = 0;
        if (hevc) {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, i, &ps, &psSize, nullptr, nullptr);
        } else {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, i, &ps, &psSize, nullptr, nullptr);
        }
        uint32_t len = (uint32_t)psSize;
        for (int b = 0; b < 4; ++b) out.push_back((len >> (b * 8)) & 0xff);
        out.insert(out.end(), ps, ps + psSize);
    }
    return out;
}

class VideoEncoder final : public IVideoEncoder {
public:
    ~VideoEncoder() override { stop(); }

    void setOutput(OutputCb cb) override { m_cb = std::move(cb); }

    bool start(const VideoConfig& cfg) override {
        m_cfg = cfg;
        m_hevc = (cfg.codec == VideoCodec::HEVC);

        CMVideoCodecType codecType = m_hevc ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264;

        NSDictionary* encoderSpec = @{
            (id)kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder : @YES,
        };
        NSDictionary* srcAttrs = @{
            (id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
        };

        OSStatus st = VTCompressionSessionCreate(
            kCFAllocatorDefault, cfg.width, cfg.height, codecType,
            (__bridge CFDictionaryRef)encoderSpec,
            (__bridge CFDictionaryRef)srcAttrs,
            nullptr, &VideoEncoder::OutputCallback, this, &m_session);
        if (st != noErr || !m_session) {
            NEBULA_LOGE(TAG, "VTCompressionSessionCreate failed: %d", (int)st);
            return false;
        }

        // Real-time, low-latency configuration; no B-frames.
        VTSessionSetProperty(m_session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
        VTSessionSetProperty(m_session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
        if (@available(macOS 11.0, *)) {
            VTSessionSetProperty(m_session, kVTCompressionPropertyKey_MaximizePowerEfficiency, kCFBooleanFalse);
        }
        VTSessionSetProperty(m_session, kVTCompressionPropertyKey_ProfileLevel,
                             m_hevc ? kVTProfileLevel_HEVC_Main_AutoLevel
                                    : kVTProfileLevel_H264_High_AutoLevel);

        int32_t bitrate = (int32_t)m_cfg.bitrate;
        CFNumberRef br = CFNumberCreate(nullptr, kCFNumberSInt32Type, &bitrate);
        VTSessionSetProperty(m_session, kVTCompressionPropertyKey_AverageBitRate, br);
        CFRelease(br);

        int32_t kfInterval = (int32_t)m_cfg.fps * 2; // keyframe every ~2s
        CFNumberRef kf = CFNumberCreate(nullptr, kCFNumberSInt32Type, &kfInterval);
        VTSessionSetProperty(m_session, kVTCompressionPropertyKey_MaxKeyFrameInterval, kf);
        CFRelease(kf);

        int32_t fps = (int32_t)m_cfg.fps;
        CFNumberRef fr = CFNumberCreate(nullptr, kCFNumberSInt32Type, &fps);
        VTSessionSetProperty(m_session, kVTCompressionPropertyKey_ExpectedFrameRate, fr);
        CFRelease(fr);

        VTCompressionSessionPrepareToEncodeFrames(m_session);
        NEBULA_LOGI(TAG, "encoder started %ux%u @%ufps %s", cfg.width, cfg.height, cfg.fps,
                  m_hevc ? "HEVC" : "H264");
        return true;
    }

    void encode(const RawVideoFrame& frame) override {
        CVPixelBufferRef pb = (CVPixelBufferRef)frame.nativeHandle;
        if (!m_session || !pb) return;
        CMTime pts = CMTimeMake((int64_t)frame.timestampUs, 1'000'000);
        CFDictionaryRef frameProps = nullptr;
        if (m_forceKey.exchange(false)) {
            const void* keys[] = { kVTEncodeFrameOptionKey_ForceKeyFrame };
            const void* vals[] = { kCFBooleanTrue };
            frameProps = CFDictionaryCreate(nullptr, keys, vals, 1, nullptr, nullptr);
        }
        VTCompressionSessionEncodeFrame(m_session, pb, pts, kCMTimeInvalid, frameProps,
                                        nullptr, nullptr);
        if (frameProps) CFRelease(frameProps);
    }

    void forceKeyframe() override { m_forceKey = true; }

    void stop() override {
        if (m_session) {
            VTCompressionSessionCompleteFrames(m_session, kCMTimeInvalid);
            VTCompressionSessionInvalidate(m_session);
            CFRelease(m_session);
            m_session = nullptr;
        }
    }

private:
    static void OutputCallback(void* ctx, void*, OSStatus status,
                               VTEncodeInfoFlags, CMSampleBufferRef sample) {
        if (status != noErr || !sample) return;
        static_cast<VideoEncoder*>(ctx)->handleEncoded(sample);
    }

    void handleEncoded(CMSampleBufferRef sample) {
        bool keyframe = true;
        CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
        if (attachments && CFArrayGetCount(attachments)) {
            CFDictionaryRef d = (CFDictionaryRef)CFArrayGetValueAtIndex(attachments, 0);
            keyframe = !CFDictionaryContainsKey(d, kCMSampleAttachmentKey_NotSync);
        }

        CMTime pts = CMSampleBufferGetPresentationTimeStamp(sample);
        uint64_t ptsUs = (uint64_t)(CMTimeGetSeconds(pts) * 1'000'000.0);

        // On keyframes, emit parameter sets first as a config frame.
        if (keyframe) {
            CMFormatDescriptionRef fmt = CMSampleBufferGetFormatDescription(sample);
            EncodedFrame cfgFrame;
            cfgFrame.data = SerializeParameterSets(fmt, m_hevc);
            cfgFrame.timestampUs = ptsUs;
            cfgFrame.config = true;
            cfgFrame.keyframe = true;
            if (m_cb) m_cb(cfgFrame);
        }

        // Convert the AVCC (length-prefixed) elementary stream to Annex-B for
        // the platform-neutral wire format, then emit.
        CMBlockBufferRef bb = CMSampleBufferGetDataBuffer(sample);
        size_t totalLen = 0;
        char* dataPtr = nullptr;
        if (CMBlockBufferGetDataPointer(bb, 0, nullptr, &totalLen, &dataPtr) == kCMBlockBufferNoErr) {
            EncodedFrame f;
            f.data = AvccToAnnexB((uint8_t*)dataPtr, totalLen, 4);
            f.timestampUs = ptsUs;
            f.keyframe = keyframe;
            f.config = false;
            if (m_cb) m_cb(f);
        }
    }

    VTCompressionSessionRef m_session = nullptr;
    VideoConfig             m_cfg;
    bool                    m_hevc = true;
    std::atomic<bool>       m_forceKey{false};
    OutputCb                m_cb;
};

} // namespace

std::unique_ptr<IVideoEncoder> CreateVideoEncoder() {
    return std::make_unique<VideoEncoder>();
}

} // namespace nebula
