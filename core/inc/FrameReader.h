//
// FrameReader.h - reassembles NebulaFrameHeader-delimited messages from a byte stream
//
#pragma once

#include "NebulaProtocol.h"
#include <functional>
#include <vector>
#include <cstdint>

namespace nebula {

// Feed raw QUIC stream bytes; emits one callback per complete framed message.
class FrameReader {
public:
    using MsgCb = std::function<void(const NebulaFrameHeader&, const uint8_t* payload, size_t len)>;

    explicit FrameReader(MsgCb cb) : m_cb(std::move(cb)) {}

    void feed(const uint8_t* data, size_t len) {
        m_buf.insert(m_buf.end(), data, data + len);
        for (;;) {
            if (m_buf.size() < kHeaderSize) return;
            NebulaFrameHeader h;
            if (!ParseHeader(m_buf.data(), m_buf.size(), h)) {
                // Resync: drop one byte and retry (robustness against corruption).
                m_buf.erase(m_buf.begin());
                continue;
            }
            const size_t total = kHeaderSize + h.length;
            if (m_buf.size() < total) return; // wait for more bytes
            if (m_cb) m_cb(h, m_buf.data() + kHeaderSize, h.length);
            m_buf.erase(m_buf.begin(), m_buf.begin() + total);
        }
    }

private:
    MsgCb                m_cb;
    std::vector<uint8_t> m_buf;
};

} // namespace nebula
