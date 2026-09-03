//
// AudioEncoder.mm - AAC encode via AudioConverter (AudioToolbox)
//
#include "AudioEncoder.h"
#include "NebulaLog.h"

#import <AudioToolbox/AudioToolbox.h>
#import <CoreMedia/CoreMedia.h>

#include <vector>

#define TAG "aenc"

namespace nebula {
namespace {

class AudioEncoder final : public IAudioEncoder {
public:
    ~AudioEncoder() override { stop(); }

    void setOutput(OutputCb cb) override { m_cb = std::move(cb); }

    bool start(const AudioConfig& cfg) override {
        m_cfg = cfg;

        // Source: 32-bit float, deinterleaved PCM (ScreenCaptureKit default).
        m_src = {};
        m_src.mSampleRate       = cfg.sampleRate;
        m_src.mFormatID         = kAudioFormatLinearPCM;
        m_src.mFormatFlags      = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonInterleaved;
        m_src.mBitsPerChannel   = 32;
        m_src.mChannelsPerFrame = cfg.channels;
        m_src.mFramesPerPacket  = 1;
        m_src.mBytesPerFrame    = 4; // per channel, non-interleaved
        m_src.mBytesPerPacket   = 4;

        // Destination: AAC LC.
        m_dst = {};
        m_dst.mSampleRate       = cfg.sampleRate;
        m_dst.mFormatID         = kAudioFormatMPEG4AAC;
        m_dst.mChannelsPerFrame = cfg.channels;
        m_dst.mFramesPerPacket  = 1024;

        OSStatus st = AudioConverterNew(&m_src, &m_dst, &m_conv);
        if (st != noErr || !m_conv) {
            NEBULA_LOGE(TAG, "AudioConverterNew failed: %d", (int)st);
            return false;
        }
        UInt32 br = cfg.bitrate;
        AudioConverterSetProperty(m_conv, kAudioConverterEncodeBitRate, sizeof(br), &br);

        emitMagicCookie();
        NEBULA_LOGI(TAG, "audio encoder started %uHz %uch AAC", cfg.sampleRate, cfg.channels);
        return true;
    }

    void encode(const RawAudioFrame& frame) override {
        CMSampleBufferRef pcm = (CMSampleBufferRef)frame.nativeHandle;
        if (!m_conv || !pcm) return;

        CMItemCount numSamples = CMSampleBufferGetNumSamples(pcm);
        if (numSamples <= 0) return;

        // Non-interleaved source needs one AudioBuffer per channel; a bare
        // AudioBufferList only has room for one, so size it for all channels.
        const UInt32 nbuf = m_src.mChannelsPerFrame ? m_src.mChannelsPerFrame : 1;
        const size_t ablSize = sizeof(AudioBufferList) + (nbuf - 1) * sizeof(AudioBuffer);
        std::vector<uint8_t> ablStorage(ablSize);
        AudioBufferList* abl = reinterpret_cast<AudioBufferList*>(ablStorage.data());

        CMBlockBufferRef blockBuf = nullptr;
        OSStatus st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            pcm, nullptr, abl, ablSize, nullptr, nullptr,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, &blockBuf);
        if (st != noErr) { if (blockBuf) CFRelease(blockBuf); return; }

        // Stash the input for the converter pull callback.
        m_inAbl = abl;
        m_inFrames = (UInt32)numSamples;
        m_inConsumed = false;

        CMTime pts = CMSampleBufferGetPresentationTimeStamp(pcm);
        uint64_t ptsUs = (uint64_t)(CMTimeGetSeconds(pts) * 1'000'000.0);

        // Pull AAC packets until the input is drained.
        for (;;) {
            std::vector<uint8_t> outBuf(4096);
            AudioBufferList outAbl;
            outAbl.mNumberBuffers = 1;
            outAbl.mBuffers[0].mNumberChannels = m_cfg.channels;
            outAbl.mBuffers[0].mDataByteSize   = (UInt32)outBuf.size();
            outAbl.mBuffers[0].mData           = outBuf.data();

            UInt32 outPackets = 1;
            AudioStreamPacketDescription pktDesc = {};
            st = AudioConverterFillComplexBuffer(m_conv, &AudioEncoder::PullInput, this,
                                                 &outPackets, &outAbl, &pktDesc);
            if (outPackets > 0) {
                EncodedFrame f;
                f.data.assign(outBuf.data(), outBuf.data() + outAbl.mBuffers[0].mDataByteSize);
                f.timestampUs = ptsUs;
                if (m_cb) m_cb(f);
            }
            if (st != noErr || outPackets == 0 || m_inConsumed) break;
        }

        if (blockBuf) CFRelease(blockBuf);
        m_inAbl = nullptr;
    }

    void stop() override {
        if (m_conv) { AudioConverterDispose(m_conv); m_conv = nullptr; }
    }

private:
    void emitMagicCookie() {
        UInt32 size = 0;
        if (AudioConverterGetPropertyInfo(m_conv, kAudioConverterCompressionMagicCookie, &size, nullptr) == noErr && size) {
            std::vector<uint8_t> cookie(size);
            if (AudioConverterGetProperty(m_conv, kAudioConverterCompressionMagicCookie, &size, cookie.data()) == noErr) {
                EncodedFrame cfg;
                cfg.data = std::move(cookie);
                cfg.config = true;
                if (m_cb) m_cb(cfg);
            }
        }
    }

    static OSStatus PullInput(AudioConverterRef, UInt32* ioNumberPackets,
                              AudioBufferList* ioData,
                              AudioStreamPacketDescription**, void* ctx) {
        auto* self = static_cast<AudioEncoder*>(ctx);
        if (self->m_inConsumed || !self->m_inAbl) {
            *ioNumberPackets = 0;
            return noErr;
        }
        ioData->mNumberBuffers = self->m_inAbl->mNumberBuffers;
        for (UInt32 i = 0; i < self->m_inAbl->mNumberBuffers; ++i) {
            ioData->mBuffers[i] = self->m_inAbl->mBuffers[i];
        }
        *ioNumberPackets = self->m_inFrames;
        self->m_inConsumed = true;
        return noErr;
    }

    AudioConverterRef           m_conv = nullptr;
    AudioStreamBasicDescription m_src{};
    AudioStreamBasicDescription m_dst{};
    AudioConfig                 m_cfg;
    OutputCb                    m_cb;

    AudioBufferList* m_inAbl = nullptr;
    UInt32           m_inFrames = 0;
    bool             m_inConsumed = true;
};

} // namespace

std::unique_ptr<IAudioEncoder> CreateAudioEncoder() {
    return std::make_unique<AudioEncoder>();
}

} // namespace nebula
