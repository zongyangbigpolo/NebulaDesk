//
// AudioDecoder.mm - AAC -> PCM via AudioConverter
//
#include "AudioDecoder.h"
#include "NebulaLog.h"

#import <AudioToolbox/AudioToolbox.h>

#include <vector>

#define TAG "adec"

namespace nebula {
namespace {

class AudioDecoder final : public IAudioDecoder {
public:
    ~AudioDecoder() override { stop(); }

    void setOutput(PcmCb cb) override { m_cb = std::move(cb); }

    bool start(const AudioConfig& cfg) override {
        m_cfg = cfg;

        m_src = {};
        m_src.mSampleRate       = cfg.sampleRate;
        m_src.mFormatID         = kAudioFormatMPEG4AAC;
        m_src.mChannelsPerFrame = cfg.channels;
        m_src.mFramesPerPacket  = 1024;

        m_dst = {};
        m_dst.mSampleRate       = cfg.sampleRate;
        m_dst.mFormatID         = kAudioFormatLinearPCM;
        m_dst.mFormatFlags      = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
        m_dst.mBitsPerChannel   = 32;
        m_dst.mChannelsPerFrame = cfg.channels;
        m_dst.mFramesPerPacket  = 1;
        m_dst.mBytesPerFrame    = 4 * cfg.channels;
        m_dst.mBytesPerPacket   = 4 * cfg.channels;
        return true;
    }

    void setMagicCookie(const uint8_t* data, size_t len) override {
        m_cookie.assign(data, data + len);
        ensureConverter();
        if (m_conv && !m_cookie.empty()) {
            AudioConverterSetProperty(m_conv, kAudioConverterDecompressionMagicCookie,
                                      (UInt32)m_cookie.size(), m_cookie.data());
        }
    }

    void decode(const uint8_t* aac, size_t len) override {
        ensureConverter();
        if (!m_conv || !aac || !len) return;

        m_inData = aac;
        m_inSize = (UInt32)len;
        m_inConsumed = false;
        m_pktDesc = {};
        m_pktDesc.mStartOffset = 0;
        m_pktDesc.mVariableFramesInPacket = 0;
        m_pktDesc.mDataByteSize = (UInt32)len;

        const UInt32 framesPerPacket = 1024;
        std::vector<float> pcm((size_t)framesPerPacket * m_cfg.channels);

        AudioBufferList outAbl;
        outAbl.mNumberBuffers = 1;
        outAbl.mBuffers[0].mNumberChannels = m_cfg.channels;
        outAbl.mBuffers[0].mDataByteSize   = (UInt32)(pcm.size() * sizeof(float));
        outAbl.mBuffers[0].mData           = pcm.data();

        UInt32 outFrames = framesPerPacket;
        OSStatus st = AudioConverterFillComplexBuffer(m_conv, &AudioDecoder::PullInput, this,
                                                      &outFrames, &outAbl, nullptr);
        if (st == noErr && outFrames > 0 && m_cb) {
            RawAudioFrame f;
            f.samples    = pcm.data();
            f.frames     = outFrames;
            f.channels   = m_cfg.channels;
            f.sampleRate = m_cfg.sampleRate;
            m_cb(f);
        }
    }

    void stop() override {
        if (m_conv) { AudioConverterDispose(m_conv); m_conv = nullptr; }
    }

private:
    void ensureConverter() {
        if (m_conv) return;
        OSStatus st = AudioConverterNew(&m_src, &m_dst, &m_conv);
        if (st != noErr) { NEBULA_LOGE(TAG, "AudioConverterNew failed: %d", (int)st); m_conv = nullptr; return; }
        if (!m_cookie.empty()) {
            AudioConverterSetProperty(m_conv, kAudioConverterDecompressionMagicCookie,
                                      (UInt32)m_cookie.size(), m_cookie.data());
        }
    }

    static OSStatus PullInput(AudioConverterRef, UInt32* ioNumberPackets,
                              AudioBufferList* ioData,
                              AudioStreamPacketDescription** outPktDesc, void* ctx) {
        auto* self = static_cast<AudioDecoder*>(ctx);
        if (self->m_inConsumed) { *ioNumberPackets = 0; return noErr; }
        ioData->mNumberBuffers = 1;
        ioData->mBuffers[0].mNumberChannels = self->m_cfg.channels;
        ioData->mBuffers[0].mDataByteSize   = self->m_inSize;
        ioData->mBuffers[0].mData           = (void*)self->m_inData;
        if (outPktDesc) *outPktDesc = &self->m_pktDesc;
        *ioNumberPackets = 1;
        self->m_inConsumed = true;
        return noErr;
    }

    AudioConverterRef           m_conv = nullptr;
    AudioStreamBasicDescription m_src{};
    AudioStreamBasicDescription m_dst{};
    AudioConfig                 m_cfg;
    std::vector<uint8_t>        m_cookie;
    PcmCb                       m_cb;

    const uint8_t* m_inData = nullptr;
    UInt32         m_inSize = 0;
    bool           m_inConsumed = true;
    AudioStreamPacketDescription m_pktDesc{};
};

} // namespace

std::unique_ptr<IAudioDecoder> CreateAudioDecoder() {
    return std::make_unique<AudioDecoder>();
}

} // namespace nebula
