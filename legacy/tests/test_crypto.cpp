// Round-trip + tamper/replay tests for the session encryption layer.
#include "NebulaCrypto.h"
#include "NebulaProtocol.h"
#include <cassert>
#include <cstdio>
#include <cstring>

using namespace nebula;

int main() {
    // 1) Both sides derive the same key from the same PSK + salts.
    uint8_t saltA[kSessionSaltLen], saltB[kSessionSaltLen];
    SecureRandomBytes(saltA, sizeof(saltA));
    SecureRandomBytes(saltB, sizeof(saltB));

    SessionCrypto cwa, vda;
    cwa.deriveKey("shared-secret", saltA, saltB);
    vda.deriveKey("shared-secret", saltA, saltB);
    assert(cwa.ready() && vda.ready());

    // 2) A message built by the VDA decrypts cleanly on the CWA side.
    const char* text = "hello nebula";
    auto msg = BuildEncryptedMessage(vda, /*channel=*/1, /*senderIsVda=*/true,
                                     MsgType::Video, Flag_Keyframe, /*seq=*/42, /*ts=*/1000,
                                     (const uint8_t*)text, std::strlen(text));
    NebulaFrameHeader h{};
    assert(ParseHeader(msg.data(), msg.size(), h));
    assert(h.seq == 42 && h.flags == Flag_Keyframe);

    std::vector<uint8_t> plain;
    assert(OpenEncryptedMessage(cwa, /*channel=*/1, /*senderIsVda=*/true, h,
                                msg.data() + kHeaderSize, h.length, plain));
    assert(plain.size() == std::strlen(text));
    assert(std::memcmp(plain.data(), text, plain.size()) == 0);

    // 3) A mismatched PSK produces a different key -> decrypt fails.
    SessionCrypto wrongKey;
    wrongKey.deriveKey("different-secret", saltA, saltB);
    std::vector<uint8_t> shouldFail;
    assert(!OpenEncryptedMessage(wrongKey, 1, true, h, msg.data() + kHeaderSize, h.length, shouldFail));

    // 4) Tampering with a single ciphertext byte is caught (auth failure).
    auto tampered = msg;
    tampered[kHeaderSize] ^= 0x01;
    NebulaFrameHeader h2{};
    assert(ParseHeader(tampered.data(), tampered.size(), h2));
    std::vector<uint8_t> tamperedPlain;
    assert(!OpenEncryptedMessage(cwa, 1, true, h2, tampered.data() + kHeaderSize, h2.length, tamperedPlain));

    // 5) Replaying the exact same (channel, sender, seq) a second time is
    // rejected even though the ciphertext is byte-for-byte valid — decrypt()
    // enforces a strictly-increasing sequence per (channel, sender).
    std::vector<uint8_t> replay;
    assert(!OpenEncryptedMessage(cwa, 1, true, h, msg.data() + kHeaderSize, h.length, replay));

    // 6) A lower/equal seq after a higher one has already been accepted is
    // also rejected (out-of-order / rollback protection).
    auto msg2 = BuildEncryptedMessage(vda, 1, true, MsgType::Video, Flag_None, /*seq=*/41, 0,
                                      (const uint8_t*)text, std::strlen(text));
    NebulaFrameHeader h3{};
    assert(ParseHeader(msg2.data(), msg2.size(), h3));
    std::vector<uint8_t> stalePlain;
    assert(!OpenEncryptedMessage(cwa, 1, true, h3, msg2.data() + kHeaderSize, h3.length, stalePlain));

    // 7) A fresh higher seq after the accepted one still works normally.
    auto msg3 = BuildEncryptedMessage(vda, 1, true, MsgType::Video, Flag_None, /*seq=*/43, 0,
                                      (const uint8_t*)text, std::strlen(text));
    NebulaFrameHeader h4{};
    assert(ParseHeader(msg3.data(), msg3.size(), h4));
    std::vector<uint8_t> nextPlain;
    assert(OpenEncryptedMessage(cwa, 1, true, h4, msg3.data() + kHeaderSize, h4.length, nextPlain));
    assert(std::memcmp(nextPlain.data(), text, nextPlain.size()) == 0);

    printf("session crypto: ALL PASS\n");
    return 0;
}
