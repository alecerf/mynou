"""CI-only independent hash/KDF reference data from Python's standard library.

No Mynou implementation is imported or executed; all keys/messages are synthetic.
"""

import argparse
import hashlib
import hmac
import json
from pathlib import Path


def pattern(length, multiplier):
    return bytes((index * multiplier + 7) & 255 for index in range(length))


def mac(key, message):
    return hmac.new(key, message, hashlib.blake2s).digest()


def expand(prk, info, length):
    result = bytearray()
    previous = b""
    for counter in range(1, (length + 31) // 32 + 1):
        previous = mac(prk, previous + info + bytes([counter]))
        result.extend(previous)
    return bytes(result[:length])


def chacha_block(key, nonce, counter):
    """Original scalar reference from RFC8439 equations, not product code."""
    initial = [int.from_bytes(part, "little") for part in
               (b"expand 32-byte k"[i:i+4] for i in range(0, 16, 4))]
    initial += [int.from_bytes(key[i:i+4], "little") for i in range(0, 32, 4)]
    initial += [counter] + [int.from_bytes(nonce[i:i+4], "little") for i in range(0, 12, 4)]
    working = initial.copy()
    for _ in range(10):
        for a, b, c, d in ((0, 4, 8, 12), (1, 5, 9, 13), (2, 6, 10, 14), (3, 7, 11, 15),
                           (0, 5, 10, 15), (1, 6, 11, 12), (2, 7, 8, 13), (3, 4, 9, 14)):
            for left, right, other, rotation in ((a, b, d, 16), (c, d, b, 12),
                                                  (a, b, d, 8), (c, d, b, 7)):
                working[left] = (working[left] + working[right]) & 0xffffffff
                value = working[other] ^ working[left]
                working[other] = ((value << rotation) | (value >> (32 - rotation))) & 0xffffffff
    return b"".join(((a + b) & 0xffffffff).to_bytes(4, "little")
                    for a, b in zip(initial, working))


def empty_aad_aead(key, nonce, message):
    ciphertext = bytearray()
    for offset in range(0, len(message), 64):
        stream = chacha_block(key, nonce, offset // 64 + 1)
        ciphertext.extend(a ^ b for a, b in zip(message[offset:offset+64], stream))
    one_time_key = chacha_block(key, nonce, 0)[:32]
    # Arbitrary-precision modular arithmetic is independent of Rust's limb code.
    r = int.from_bytes(one_time_key[:16], "little") & 0x0ffffffc0ffffffc0ffffffc0fffffff
    authenticated = bytes(ciphertext) + bytes((-len(ciphertext)) % 16)
    authenticated += bytes(8) + len(ciphertext).to_bytes(8, "little")
    accumulator = 0
    for offset in range(0, len(authenticated), 16):
        block = int.from_bytes(authenticated[offset:offset+16] + b"\x01", "little")
        accumulator = ((accumulator + block) * r) % ((1 << 130) - 5)
    tag = ((accumulator + int.from_bytes(one_time_key[16:], "little")) % (1 << 128))
    return bytes(ciphertext) + tag.to_bytes(16, "little")


def transport_vectors():
    # Public C2SP/Wycheproof chacha20_poly1305_test.json tcId 2 anchors the AEAD
    # reference (empty message and AAD, independent published expected tag).
    assert empty_aad_aead(
        bytes.fromhex("80ba3192c803ce965ea371d5ff073cf0f43b6a2ab576b208426e11409c09b9b0"),
        bytes.fromhex("4da5bf8dfd5852c1ea12379d"), b""
    ).hex() == "76acb342cf3166a5b63c0c0ea1383c8d"
    rows = []
    key = pattern(32, 29)
    for counter in (0, 0x0807060504030201):
        for size in (0, 1, 15, 16, 17, 63, 64, 1424):
            payload = pattern(size, 17)
            padded = payload + bytes((-size) % 16)
            nonce = bytes(4) + counter.to_bytes(8, "little")
            header = bytes((4, 0, 0, 0)) + (31).to_bytes(4, "little") + counter.to_bytes(8, "little")
            rows.append({"key": key.hex(), "counter": counter, "payload": payload.hex(),
                         "packet": (header + empty_aad_aead(key, nonce, padded)).hex()})
    return rows


def vectors():
    # Fixed public RFC 7693 Appendix B anchors the independent reference.
    assert hashlib.blake2s(b"abc").hexdigest() == (
        "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982"
    )
    hashes = []
    for key_length in (0, 1, 16, 32):
        for output in (1, 16, 20, 32):
            for size in (0, 1, 63, 64, 65, 127, 128, 129, 1024):
                key, data = pattern(key_length, 29), pattern(size, 17)
                hashes.append({"key": key.hex(), "data": data.hex(), "size": output,
                               "digest": hashlib.blake2s(data, key=key,
                                                         digest_size=output).hexdigest()})
    hmacs = []
    for key_length in (0, 1, 32, 63, 64, 65, 129):
        for size in (0, 1, 63, 64, 65, 1024):
            key, data = pattern(key_length, 29), pattern(size, 17)
            hmacs.append({"key": key.hex(), "data": data.hex(), "digest": mac(key, data).hex()})
    derivations = []
    for salt_length in (0, 13, 64, 65):
        for size in (0, 22, 64):
            salt, data = pattern(salt_length, 29), pattern(size, 17)
            prk = mac(salt, data)
            info = b"mynou synthetic key derivation"
            for length in (0, 1, 31, 32, 33, 64, 96, 8160):
                derivations.append({"salt": salt.hex(), "data": data.hex(),
                                    "info": info.hex(), "prk": prk.hex(), "size": length,
                                    "output": expand(prk, info, length).hex()})
    return {"schema": 1, "blake2s": hashes, "hmac": hmacs, "hkdf": derivations,
            "wireguard": transport_vectors()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    data = vectors()
    with args.output.open("x", encoding="utf-8") as stream:
        json.dump(data, stream, separators=(",", ":"))
    print("Generated independent std-only synthetic crypto vectors: "
          f"{len(data['blake2s'])} hashes, {len(data['hmac'])} HMACs, {len(data['hkdf'])} HKDFs")


if __name__ == "__main__":
    main()
