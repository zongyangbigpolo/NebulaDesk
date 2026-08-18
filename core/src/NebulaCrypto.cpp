//
// NebulaCrypto.cpp - ChaCha20-Poly1305 session encryption (libsodium)
//
#include "NebulaCrypto.h"

#include <sodium.h>

#include <cstring>
#include <mutex>

namespace nebula {
namespace {

std::once_flag g_sodiumInitFlag;

void EnsureSodiumInit() {
    std::call_once(g_sodiumInitFlag, [] {
        if (sodium_init() < 0) {
            // sodium_init() failing means the CSPRNG couldn't be initialized;
            // there is no safe way to continue, so abort loudly rather than
            // silently running with a broken/absent crypto layer.
            std::abort();
        }
    });
}

// nonce = channel(1) || senderIsVda(1) || reserved(2) || seq(8, big-endian)
void BuildNonce(uint8_t channel, bool senderIsVda, uint32_t seq,
                uint8_t out[crypto_aead_chacha20poly1305_ietf_NPUBBYTES]) {
    static_assert(crypto_aead_chacha20poly1305_ietf_NPUBBYTES == 12, "nonce size assumption");
    out[0] = channel;
    out[1] = senderIsVda ? 0 : 1;
    out[2] = 0;
    out[3] = 0;
    uint64_t s = seq;
    for (int i = 0; i < 8; ++i) out[4 + i] = (uint8_t)(s >> (8 * (7 - i)));
}

} // namespace

void SecureRandomBytes(uint8_t* buf, size_t len) {
    EnsureSodiumInit();
    randombytes_buf(buf, len);
}

void SessionCrypto::deriveKey(const std::string& psk, const uint8_t saltA[kSessionSaltLen],
                              const uint8_t saltB[kSessionSaltLen]) {
    EnsureSodiumInit();
    uint8_t salt[kSessionSaltLen * 2];
    std::memcpy(salt, saltA, kSessionSaltLen);
    std::memcpy(salt + kSessionSaltLen, saltB, kSessionSaltLen);

    uint8_t prk[crypto_kdf_hkdf_sha256_KEYBYTES];
    crypto_kdf_hkdf_sha256_extract(prk, salt, sizeof(salt),
                                   reinterpret_cast<const uint8_t*>(psk.data()), psk.size());
    static const char kInfo[] = "nebula-session-v1";
    crypto_kdf_hkdf_sha256_expand(m_key, sizeof(m_key), kInfo, sizeof(kInfo) - 1, prk);
    sodium_memzero(prk, sizeof(prk));

    for (auto& v : m_lastSeq) v = -1;
    m_ready = true;
}

std::vector<uint8_t> SessionCrypto::encrypt(uint8_t channel, bool senderIsVda, uint32_t seq,
                                            const uint8_t* aad, size_t aadLen,
                                            const uint8_t* plaintext, size_t len) {
    uint8_t nonce[crypto_aead_chacha20poly1305_ietf_NPUBBYTES];
    BuildNonce(channel, senderIsVda, seq, nonce);

    std::vector<uint8_t> out(len + kAeadTagLen);
    unsigned long long outLen = 0;
    crypto_aead_chacha20poly1305_ietf_encrypt(
        out.data(), &outLen, plaintext, len, aad, aadLen, nullptr, nonce, m_key);
    out.resize((size_t)outLen);
    return out;
}

bool SessionCrypto::decrypt(uint8_t channel, bool senderIsVda, uint32_t seq,
                            const uint8_t* aad, size_t aadLen,
                            const uint8_t* ciphertext, size_t len,
                            std::vector<uint8_t>& outPlaintext) {
    if (len < kAeadTagLen) return false;

    const int idx = channel * 2 + (senderIsVda ? 0 : 1);
    if (idx < 0 || idx >= 6) return false;
    // Strictly-increasing sequence per (channel, sender) rejects replays and
    // out-of-order delivery; safe because each channel is one ordered QUIC
    // stream (direct) or one ordered multiplexed sub-stream (relay).
    if ((int64_t)seq <= m_lastSeq[idx]) return false;

    uint8_t nonce[crypto_aead_chacha20poly1305_ietf_NPUBBYTES];
    BuildNonce(channel, senderIsVda, seq, nonce);

    outPlaintext.resize(len - kAeadTagLen);
    unsigned long long outLen = 0;
    if (crypto_aead_chacha20poly1305_ietf_decrypt(
            outPlaintext.data(), &outLen, nullptr,
            ciphertext, len, aad, aadLen, nonce, m_key) != 0) {
        outPlaintext.clear();
        return false;
    }
    outPlaintext.resize((size_t)outLen);
    m_lastSeq[idx] = (int64_t)seq;
    return true;
}

std::vector<uint8_t> BuildEncryptedMessage(SessionCrypto& crypto, uint8_t channel, bool senderIsVda,
                                           MsgType type, uint16_t flags, uint32_t seq,
                                           uint64_t timestampUs,
                                           const uint8_t* plaintext, size_t plaintextLen) {
    NebulaFrameHeader h{};
    h.magic       = kNebulaMagic;
    h.version     = kNebulaVersion;
    h.type        = static_cast<uint8_t>(type);
    h.flags       = flags;
    h.length      = static_cast<uint32_t>(plaintextLen + kAeadTagLen);
    h.seq         = seq;
    h.timestampUs = timestampUs;

    auto ciphertext = crypto.encrypt(channel, senderIsVda, seq,
                                     reinterpret_cast<const uint8_t*>(&h), kHeaderSize,
                                     plaintext, plaintextLen);
    std::vector<uint8_t> out(kHeaderSize + ciphertext.size());
    std::memcpy(out.data(), &h, kHeaderSize);
    std::memcpy(out.data() + kHeaderSize, ciphertext.data(), ciphertext.size());
    return out;
}

bool OpenEncryptedMessage(SessionCrypto& crypto, uint8_t channel, bool senderIsVda,
                          const NebulaFrameHeader& header,
                          const uint8_t* ciphertext, size_t ciphertextLen,
                          std::vector<uint8_t>& outPlaintext) {
    return crypto.decrypt(channel, senderIsVda, header.seq,
                          reinterpret_cast<const uint8_t*>(&header), kHeaderSize,
                          ciphertext, ciphertextLen, outPlaintext);
}

} // namespace nebula
