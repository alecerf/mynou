"""CI-only independent hash/KDF/AEAD reference data using Python's standard library.

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


def reference_aead(key, nonce, message, aad=b""):
    ciphertext = bytearray()
    for offset in range(0, len(message), 64):
        stream = chacha_block(key, nonce, offset // 64 + 1)
        ciphertext.extend(a ^ b for a, b in zip(message[offset:offset+64], stream))
    one_time_key = chacha_block(key, nonce, 0)[:32]
    # Arbitrary-precision modular arithmetic is independent of Rust's limb code.
    r = int.from_bytes(one_time_key[:16], "little") & 0x0ffffffc0ffffffc0ffffffc0fffffff
    authenticated = aad + bytes((-len(aad)) % 16)
    authenticated += bytes(ciphertext) + bytes((-len(ciphertext)) % 16)
    authenticated += len(aad).to_bytes(8, "little") + len(ciphertext).to_bytes(8, "little")
    accumulator = 0
    for offset in range(0, len(authenticated), 16):
        block = int.from_bytes(authenticated[offset:offset+16] + b"\x01", "little")
        accumulator = ((accumulator + block) * r) % ((1 << 130) - 5)
    tag = ((accumulator + int.from_bytes(one_time_key[16:], "little")) % (1 << 128))
    return bytes(ciphertext) + tag.to_bytes(16, "little")


def transport_vectors():
    # RFC8439 section 2.3.2 anchors the counter-1 encryption stream separately.
    assert chacha_block(bytes(range(32)), bytes.fromhex("000000090000004a00000000"), 1).hex() == (
        "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e"
        "d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
    )
    # Public C2SP/Wycheproof chacha20_poly1305_test.json tcId 2 anchors the AEAD
    # reference (empty message and AAD, independent published expected tag).
    assert reference_aead(
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
            rows.append({"key": key.hex(), "counter": str(counter), "payload": payload.hex(),
                         "packet": (header + reference_aead(key, nonce, padded)).hex()})
    return rows


def reference_dh(secret, peer):
    """Independently authored RFC7748 Montgomery equations with Python integers."""
    scalar = bytearray(secret)
    scalar[0] &= 248
    scalar[31] = (scalar[31] & 127) | 64
    scalar = int.from_bytes(scalar, "little")
    prime = (1 << 255) - 19
    coordinate = int.from_bytes(peer, "little") & ((1 << 255) - 1)
    x2, z2, x3, z3 = 1, 0, coordinate, 1
    for bit in range(254, -1, -1):
        selected = (scalar >> bit) & 1
        if selected:
            x2, z2, x3, z3 = x3, z3, x2, z2
        a, b, c, d = x2 + z2, x2 - z2, x3 + z3, x3 - z3
        aa, bb = a * a % prime, b * b % prime
        e = (aa - bb) % prime
        da, cb = d * a % prime, c * b % prime
        x3, z3 = (da + cb)**2 % prime, coordinate * (da - cb)**2 % prime
        x2, z2 = aa * bb % prime, e * (aa + 121665 * e) % prime
        if selected:
            x2, z2, x3, z3 = x3, z3, x2, z2
    return (x2 * pow(z2, prime - 2, prime) % prime).to_bytes(32, "little")


def handshake_vectors():
    base = bytes((9,)) + bytes(31)
    assert reference_dh(base, base).hex() == (
        "422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079"
    )  # RFC7748 section 5.2, iteration one.
    # RFC8439 section 2.8.2 anchors nonempty associated data, not just empty tags.
    assert reference_aead(bytes(range(128, 160)), bytes.fromhex("070000004041424344454647"),
        b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.",
        bytes.fromhex("50515253c0c1c2c3c4c5c6c7")).hex() == (
        "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63"
        "dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b3692"
        "ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc3ff4"
        "def08e4b7a9de576d26586cec64b61161ae10b594f09e26a7e902ecbd0600691"
    )
    rows = []
    digest = lambda data: hashlib.blake2s(data).digest()
    for present in (False, True):
        initiator, responder = pattern(32, 17), pattern(32, 29)
        ephemeral_i, ephemeral_r = pattern(32, 37), pattern(32, 43)
        psk = pattern(32, 53) if present else bytes(32)
        public_i, public_r = reference_dh(initiator, base), reference_dh(responder, base)
        public_ei, public_er = reference_dh(ephemeral_i, base), reference_dh(ephemeral_r, base)
        chain = digest(b"Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s")
        transcript = digest(digest(chain + b"WireGuard v1 zx2c4 Jason@zx2c4.com") + public_r)
        def mix(value, count):
            nonlocal chain
            output = expand(mac(chain, value), b"", count * 32)
            chain = output[:32]
            return [output[i:i+32] for i in range(0, len(output), 32)]
        transcript = digest(transcript + public_ei)
        mix(public_ei, 1)
        key = mix(reference_dh(ephemeral_i, public_r), 2)[1]
        identity = reference_aead(key, bytes(12), public_i, transcript)
        transcript = digest(transcript + identity)
        key = mix(reference_dh(initiator, public_r), 2)[1]
        timestamp = (0x400000000000000a + 123).to_bytes(8, "big") + (456).to_bytes(4, "big")
        encrypted_time = reference_aead(key, bytes(12), timestamp, transcript)
        transcript = digest(transcript + encrypted_time)
        initiation = bytes((1, 0, 0, 0)) + (17).to_bytes(4, "little") + public_ei + identity + encrypted_time
        initiation += hashlib.blake2s(initiation, key=digest(b"mac1----" + public_r), digest_size=16).digest() + bytes(16)
        initial_chain, initial_hash = chain, transcript
        transcript = digest(transcript + public_er)
        mix(public_er, 1)
        mix(reference_dh(ephemeral_r, public_ei), 1)
        mix(reference_dh(ephemeral_r, public_i), 1)
        _, temp, key = mix(psk, 3)
        transcript = digest(transcript + temp)
        empty = reference_aead(key, bytes(12), b"", transcript)
        response = bytes((2, 0, 0, 0)) + (31).to_bytes(4, "little") + (17).to_bytes(4, "little") + public_er + empty
        response += hashlib.blake2s(response, key=digest(b"mac1----" + public_i), digest_size=16).digest() + bytes(16)
        directional = expand(mac(chain, b""), b"", 64)
        payload = b"reference proof!"  # exactly 16 bytes including exclamation.
        payload += bytes((-len(payload)) % 16)
        transport = bytes((4, 0, 0, 0)) + (31).to_bytes(4, "little") + bytes(8)
        transport += reference_aead(directional[:32], bytes(12), payload)
        rows.append({"initiator": initiator.hex(), "responder": responder.hex(),
                     "ephemeral_i": ephemeral_i.hex(), "ephemeral_r": ephemeral_r.hex(),
                     "psk": psk.hex(), "initiation": initiation.hex(), "response": response.hex(),
                     "initial_chain": initial_chain.hex(), "initial_hash": initial_hash.hex(),
                     "final_chain": chain.hex(), "transport": transport.hex()})
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
            "wireguard": transport_vectors(), "handshake": handshake_vectors()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    data = vectors()
    with args.output.open("x", encoding="utf-8") as stream:
        json.dump(data, stream, separators=(",", ":"))
    print("Generated independent std-only synthetic crypto vectors: "
          f"{len(data['blake2s'])} hashes, {len(data['hmac'])} HMACs, "
          f"{len(data['hkdf'])} HKDFs, {len(data['wireguard'])} transport packets, "
          f"{len(data['handshake'])} Noise handshakes")


if __name__ == "__main__":
    main()
