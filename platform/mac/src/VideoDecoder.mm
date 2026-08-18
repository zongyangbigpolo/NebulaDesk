//
// VideoDecoder.mm - VideoToolbox hardware decode implementation
//
#include "VideoDecoder.h"
#include "AnnexB.h"
#include "NebulaLog.h"

#import <VideoToolbox/VideoToolbox.h>
#import <CoreMedia/CoreMedia.h>

#include <vector>

#define TAG "vdec"

namespace nebula {
namespace {

class VideoDecoder final : public IVideoDecoder {
public:
    ~VideoDecoder() override { stop(); }

    void setOutput(FrameCb cb) override { m_cb = std::move(cb); }

    bool start(const VideoConfig& cfg) override {
        m_hevc = (cfg.codec == VideoCodec::HEVC);
        NEBULA_LOGI(TAG, "decoder ready (%s)", m_hevc ? "HEVC" : "H264");
        return true;
    }

    void setConfig(const uint8_t* data, size_t len) override {
        // Parse: count(1) + [len(4 LE)+bytes]...
        if (!data || len < 1) return;
        size_t p = 0;
        uint8_t count = data[p++];
        std::vector<const uint8_t*> ptrs;
        std::vector<size_t> sizes;
        m_paramStore.clear();
        for (uint8_t i = 0; i < count; ++i) {
            if (p + 4 > len) return;
            uint32_t psLen = 0;
            for (int b = 0; b < 4; ++b) psLen |= (uint32_t)data[p++] << (b * 8);
            if (p + psLen > len) return;
            m_paramStore.emplace_back(data + p, data + p + psLen);
            p += psLen;
        }
        for (auto& v : m_paramStore) { ptrs.push_back(v.data()); sizes.push_back(v.size()); }

        if (m_format) { CFRelease(m_format); m_format = nullptr; }
        OSStatus st;
        if (m_hevc) {
            st = CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                kCFAllocatorDefault, ptrs.size(), ptrs.data(), sizes.data(), 4, nullptr, &m_format);
        } else {
            st = CMVideoFormatDescriptionCreateFromH264ParameterSets(
                kCFAllocatorDefault, ptrs.size(), ptrs.data(), sizes.data(), 4, &m_format);
        }
        if (st != noErr || !m_format) {
            NEBULA_LOGE(TAG, "format description creation failed: %d", (int)st);
            return;
        }
        recreateSession();
    }

    void decode(const uint8_t* data, size_t len, bool, uint64_t ptsUs) override {
        if (!m_session || !m_format || !data || !len) return;

        // Wire format is Annex-B; VideoToolbox needs AVCC (4-byte length prefix).
        std::vector<uint8_t> avcc = AnnexBToAvcc(data, len);
        if (avcc.empty()) return;

        CMBlockBufferRef bb = nullptr;
        OSStatus st = CMBlockBufferCreateWithMemoryBlock(
            kCFAllocatorDefault, nullptr, avcc.size(), kCFAllocatorDefault, nullptr, 0, avcc.size(), 0, &bb);
        if (st != kCMBlockBufferNoErr) return;
        CMBlockBufferReplaceDataBytes(avcc.data(), bb, 0, avcc.size());

        CMSampleBufferRef sample = nullptr;
        const size_t sizeArr[1] = { avcc.size() };
        CMSampleTimingInfo timing;
        timing.duration = kCMTimeInvalid;
        timing.presentationTimeStamp = CMTimeMake((int64_t)ptsUs, 1'000'000);
        timing.decodeTimeStamp = kCMTimeInvalid;
        st = CMSampleBufferCreateReady(kCFAllocatorDefault, bb, m_format, 1, 1, &timing,
                                       1, sizeArr, &sample);
        CFRelease(bb);
        if (st != noErr || !sample) return;

        VTDecodeFrameFlags flags = kVTDecodeFrame_EnableAsynchronousDecompression;
        VTDecompressionSessionDecodeFrame(m_session, sample, flags, nullptr, nullptr);
        CFRelease(sample);
    }

    void stop() override {
        if (m_session) {
            VTDecompressionSessionInvalidate(m_session);
            CFRelease(m_session);
            m_session = nullptr;
        }
        if (m_format) { CFRelease(m_format); m_format = nullptr; }
    }

private:
    void recreateSession() {
        if (m_session) {
            VTDecompressionSessionInvalidate(m_session);
            CFRelease(m_session);
            m_session = nullptr;
        }
        NSDictionary* destAttrs = @{
            (id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
            (id)kCVPixelBufferMetalCompatibilityKey : @YES,
        };
        VTDecompressionOutputCallbackRecord cb;
        cb.decompressionOutputCallback = &VideoDecoder::OutputCallback;
        cb.decompressionOutputRefCon = this;

        OSStatus st = VTDecompressionSessionCreate(
            kCFAllocatorDefault, m_format, nullptr,
            (__bridge CFDictionaryRef)destAttrs, &cb, &m_session);
        if (st != noErr) NEBULA_LOGE(TAG, "VTDecompressionSessionCreate failed: %d", (int)st);
    }

    static void OutputCallback(void* ctx, void*, OSStatus status, VTDecodeInfoFlags,
                               CVImageBufferRef image, CMTime pts, CMTime) {
        if (status != noErr || !image) return;
        auto* self = static_cast<VideoDecoder*>(ctx);
        RawVideoFrame f;
        f.format = PixelFormat::NV12;
        f.width  = (uint32_t)CVPixelBufferGetWidth(image);
        f.height = (uint32_t)CVPixelBufferGetHeight(image);
        f.timestampUs = (uint64_t)(CMTimeGetSeconds(pts) * 1'000'000.0);
        f.nativeHandle = image; // zero-copy: renderer wraps this CVImageBuffer as a Metal texture
        if (self->m_cb) self->m_cb(f);
    }

    VTDecompressionSessionRef m_session = nullptr;
    CMVideoFormatDescriptionRef m_format = nullptr;
    std::vector<std::vector<uint8_t>> m_paramStore;
    bool m_hevc = true;
    FrameCb m_cb;
};

} // namespace

std::unique_ptr<IVideoDecoder> CreateVideoDecoder() {
    return std::make_unique<VideoDecoder>();
}

} // namespace nebula
