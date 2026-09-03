//
// OpusAudioEncoder.cpp - libopus encoder feeding fixed-size 20ms frames
//
#include "OpusAudioEncoder.h"
#include "NebulaLog.h"

#include <opus.h>

#include <vector>

#define TAG "opus-enc"

namespace nebula {
namespace {

// WebRTC/RTP convention: 20ms frames. Opus only accepts a fixed set of frame
// durations (2.5/5/10/20/40/60ms); 20ms is the standard choice.
constexpr uint32_t kFrameMs = 20;

class OpusAudioEncoder final : public IOpusAudioEncoder {
public:
    ~OpusAudioEncoder() override { stop(); }

    void setOutput(OutputCb cb) override { m_cb = std::move(cb); }

    bool start(const AudioConfig& cfg) override {
        stop();
        m_channels = cfg.channels ? cfg.channels : 2;
        m_sampleRate = cfg.sampleRate ? cfg.sampleRate : 48000;
        m_frameSamplesPerChannel = m_sampleRate * kFrameMs / 1000;

        int err = 0;
        m_enc = opus_encoder_create(m_sampleRate, m_channels, OPUS_APPLICATION_RESTRICTED_LOWDELAY, &err);
        if (err != OPUS_OK || !m_enc) {
            NEBULA_LOGE(TAG, "opus_encoder_create failed: %d", err);
            return false;
        }
        opus_encoder_ctl(m_enc, OPUS_SET_BITRATE(cfg.bitrate ? (int)cfg.bitrate : 64000));
        opus_encoder_ctl(m_enc, OPUS_SET_INBAND_FEC(1));
        opus_encoder_ctl(m_enc, OPUS_SET_PACKET_LOSS_PERC(10));
        opus_encoder_ctl(m_enc, OPUS_SET_SIGNAL(OPUS_SIGNAL_MUSIC));

        m_accum.clear();
        m_timestampUs = 0;
        NEBULA_LOGI(TAG, "opus encoder started %uHz %uch %ums frames", m_sampleRate, m_channels, kFrameMs);
        return true;
    }

    void encode(const RawAudioFrame& frame) override {
        if (!m_enc || !frame.samples || !frame.frames || !frame.channels) return;
        if (frame.channels != m_channels || frame.sampleRate != m_sampleRate) {
            // A mismatched source format would silently corrupt the encode;
            // refuse rather than produce garbage audio.
            NEBULA_LOGW(TAG, "dropping frame: format mismatch (got %uch/%uHz, expected %uch/%uHz)",
                        frame.channels, frame.sampleRate, m_channels, m_sampleRate);
            return;
        }
        if (m_accum.empty()) m_timestampUs = frame.timestampUs;
        m_accum.insert(m_accum.end(), frame.samples, frame.samples + (size_t)frame.frames * frame.channels);

        const size_t frameLen = (size_t)m_frameSamplesPerChannel * m_channels;
        std::vector<unsigned char> out(4000); // libopus recommends >= 4000 bytes for opus_encode
        while (m_accum.size() >= frameLen) {
            int n = opus_encode_float(m_enc, m_accum.data(), (int)m_frameSamplesPerChannel,
                                      out.data(), (opus_int32)out.size());
            if (n < 0) {
                NEBULA_LOGE(TAG, "opus_encode_float failed: %d", n);
            } else if (n > 0 && m_cb) {
                EncodedFrame ef;
                ef.data.assign(out.data(), out.data() + n);
                ef.timestampUs = m_timestampUs;
                ef.keyframe = true;  // every Opus frame decodes independently
                ef.config = false;   // no out-of-band config needed for Opus RTP
                m_cb(ef);
            }
            m_accum.erase(m_accum.begin(), m_accum.begin() + frameLen);
            m_timestampUs += (uint64_t)kFrameMs * 1000;
        }
    }

    void stop() override {
        if (m_enc) { opus_encoder_destroy(m_enc); m_enc = nullptr; }
        m_accum.clear();
    }

private:
    OpusEncoder* m_enc = nullptr;
    OutputCb     m_cb;
    uint32_t     m_channels = 2;
    uint32_t     m_sampleRate = 48000;
    uint32_t     m_frameSamplesPerChannel = 960;
    uint64_t     m_timestampUs = 0;
    std::vector<float> m_accum;
};

} // namespace

std::unique_ptr<IOpusAudioEncoder> CreateOpusAudioEncoder() {
    return std::make_unique<OpusAudioEncoder>();
}

} // namespace nebula
