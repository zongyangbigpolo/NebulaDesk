//
// NebulaCrypto.h - application-layer end-to-end encryption for Nebula
//
// Every byte that leaves a VdaServer/CwaClient (over EITHER the direct QUIC
// transport OR a nebula_relay bridge) is sealed with ChaCha20-Poly1305 (IETF)
// before it is handed to ITransport, and verified/opened on receipt. This is
// what makes the relay's "blind forward" claim actually true: the relay only
// ever sees ciphertext, because encryption happens above the transport
// abstraction, identically for both transport backends.
//
// Handshake: the two peers already share a long-lived secret (the PSK set via
// --psk / NEBULA_PSK). To avoid ever reusing a key+nonce pair across sessions,
// each connection derives a fresh 256-bit session key via HKDF-SHA256 over the
// PSK, salted with two 16-byte random values (one contributed by each side)
// exchanged in cleartext as the very first messages on the Control channel:
// KeyInit (CWA -> VDA, carries the CWA's salt) and KeyInitAck (VDA -> CWA,
// carries the VDA's salt). These two message types are the ONLY ones ever
// sent unencrypted; everything else (Hello/HelloAck/Video/Audio/Input/Bye) is
// sealed once both salts are known.
//
// Nonce: 12 bytes = channel(1) || senderIsVda(1) || reserved(2) || seq(8 BE).
// `seq` is the frame header's existing per-channel sequence number, already
// monotonic per sender — reusing it costs no wire format change and doubles
// as replay protection (a decrypt with a non-increasing seq is rejected).
//
#pragma once

#include "NebulaProtocol.h"
#include <cstdint>
#include <cstddef>
#include <string>
#include <vector>

namespace nebula {

constexpr size_t kSessionSaltLen = 16;
constexpr size_t kAeadTagLen     = 16; // crypto_aead_chacha20poly1305_ietf_ABYTES

// Fills `len` bytes with cryptographically secure random data.
void SecureRandomBytes(uint8_t* buf, size_t len);

class SessionCrypto {
public:
    // Derive the session key from the shared PSK plus both peers' salts.
    // Call exactly once, after both salts are known (KeyInit + KeyInitAck).
    void deriveKey(const std::string& psk, const uint8_t saltA[kSessionSaltLen],
                   const uint8_t saltB[kSessionSaltLen]);
    bool ready() const { return m_ready; }

    // Seal `plaintext` for `channel`, sent by the VDA (if senderIsVda) or the
    // CWA. `aad` should be the message's cleartext frame header bytes (binds
    // the ciphertext to that specific header so it can't be replayed under a
    // different header). Returns plaintext.size() + kAeadTagLen bytes.
    std::vector<uint8_t> encrypt(uint8_t channel, bool senderIsVda, uint32_t seq,
                                 const uint8_t* aad, size_t aadLen,
                                 const uint8_t* plaintext, size_t len);

    // Open + verify a message received on `channel` from `senderIsVda`.
    // Returns false on authentication failure OR a non-increasing `seq`
    // (replay/reorder guard) — callers must drop the message in that case.
    bool decrypt(uint8_t channel, bool senderIsVda, uint32_t seq,
                const uint8_t* aad, size_t aadLen,
                const uint8_t* ciphertext, size_t len,
                std::vector<uint8_t>& outPlaintext);

private:
    uint8_t m_key[32] = {};
    bool    m_ready = false;
    // Replay guard: highest accepted seq per (channel, senderIsVda). Channel
    // is 0..2 (Control/Video/Audio); index = channel*2 + (senderIsVda?0:1).
    int64_t m_lastSeq[6] = { -1, -1, -1, -1, -1, -1 };
};

// Convenience helpers used by VdaServer/CwaClient: build a fully-sealed wire
// message (header + ciphertext), and open a received (header, ciphertext)
// pair. These keep the AEAD nonce/AAD bookkeeping in one place.
std::vector<uint8_t> BuildEncryptedMessage(SessionCrypto& crypto, uint8_t channel, bool senderIsVda,
                                           MsgType type, uint16_t flags, uint32_t seq,
                                           uint64_t timestampUs,
                                           const uint8_t* plaintext, size_t plaintextLen);

bool OpenEncryptedMessage(SessionCrypto& crypto, uint8_t channel, bool senderIsVda,
                          const NebulaFrameHeader& header,
                          const uint8_t* ciphertext, size_t ciphertextLen,
                          std::vector<uint8_t>& outPlaintext);

} // namespace nebula
