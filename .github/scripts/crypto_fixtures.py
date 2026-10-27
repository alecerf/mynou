"""CI-only independent hash/KDF/AEAD reference data using Python's standard library.

No Mynou implementation is imported or executed; all keys/messages are synthetic.
"""

import argparse
import hashlib
import hmac
import ipaddress
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


def chacha_initial(key, nonce, counter):
    constants = [int.from_bytes(b"expand 32-byte k"[i:i+4], "little") for i in range(0, 16, 4)]
    return (constants + [int.from_bytes(key[i:i+4], "little") for i in range(0, 32, 4)]
            + [counter] + [int.from_bytes(nonce[i:i+4], "little") for i in range(0, 12, 4)])


def chacha_rounds(initial):
    working = initial.copy()
    for _ in range(10):
        for a, b, c, d in ((0, 4, 8, 12), (1, 5, 9, 13), (2, 6, 10, 14), (3, 7, 11, 15),
                           (0, 5, 10, 15), (1, 6, 11, 12), (2, 7, 8, 13), (3, 4, 9, 14)):
            for left, right, other, rotation in ((a, b, d, 16), (c, d, b, 12),
                                                  (a, b, d, 8), (c, d, b, 7)):
                working[left] = (working[left] + working[right]) & 0xffffffff
                value = working[other] ^ working[left]
                working[other] = ((value << rotation) | (value >> (32 - rotation))) & 0xffffffff
    return working


def chacha_block(key, nonce, counter):
    """Original scalar reference from RFC8439 equations, not product code."""
    initial = chacha_initial(key, nonce, counter)
    working = chacha_rounds(initial)
    return b"".join(((a + b) & 0xffffffff).to_bytes(4, "little")
                    for a, b in zip(initial, working))


def hchacha20(key, nonce):
    """draft-irtf-cfrg-xchacha-03 section 2.2: rounds without feed-forward."""
    # The 16-byte nonce fills state words 12..15, so reuse the layout with counter
    # taken from its first word.
    initial = chacha_initial(key, nonce[4:16], int.from_bytes(nonce[:4], "little"))
    working = chacha_rounds(initial)
    return b"".join(word.to_bytes(4, "little") for word in working[0:4] + working[12:16])


def reference_xaead(key, nonce, message, aad=b""):
    return reference_aead(hchacha20(key, nonce[:16]), bytes(4) + nonce[16:], message, aad)


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


def cookie_vectors():
    # draft-irtf-cfrg-xchacha-03 section 2.2.1 and appendix A.3.1 anchor the
    # independent HChaCha20/XChaCha20-Poly1305 reference (also used by Rust tests).
    assert hchacha20(bytes(range(32)), bytes.fromhex("000000090000004a0000000031415927")).hex() == (
        "82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc")
    assert reference_xaead(
        bytes(range(128, 160)), bytes(range(64, 88)),
        b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.",
        bytes.fromhex("50515253c0c1c2c3c4c5c6c7")).hex().endswith("c0875924c1c7987947deafd8780acf49")
    base = bytes((9,)) + bytes(31)
    digest = lambda data: hashlib.blake2s(data).digest()
    blake_mac = lambda key, data: hashlib.blake2s(data, key=key, digest_size=16).digest()
    rows = []
    cases = ((1, 148, "192.0.2.33", 51820), (2, 92, "198.51.100.7", 1), (1, 148, "2001:db8::beef", 65535),
             (2, 92, "2001:db8:1:2:3:4:5:6", 4242))
    for number, (kind, length, host, port) in enumerate(cases):
        responder = pattern(32, 61 + number)
        public = reference_dh(responder, base)
        secret = pattern(32, 67 + number)
        nonce = pattern(24, 71 + number)
        body = bytes((kind, 0, 0, 0)) + (1000 + number).to_bytes(4, "little")
        body += pattern(length - 32 - len(body), 73 + number)
        mac1 = blake_mac(digest(b"mac1----" + public), body)
        address = ipaddress.ip_address(host)
        cookie = blake_mac(secret, address.packed + port.to_bytes(2, "big"))
        reply = bytes((3, 0, 0, 0)) + (1000 + number).to_bytes(4, "little") + nonce
        reply += reference_xaead(digest(b"cookie--" + public), nonce, cookie, mac1)
        assert len(reply) == 64
        rows.append({"public": public.hex(), "secret": secret.hex(), "nonce": nonce.hex(),
                     "host": host, "port": port, "body": body.hex(), "mac1": mac1.hex(),
                     "cookie": cookie.hex(), "reply": reply.hex(),
                     "retry_mac2": blake_mac(cookie, body + mac1).hex()})
    return rows


def inet_checksum(data):
    # RFC 1071 section 4.1: sum 16-bit big-endian words, fold carries, complement.
    if len(data) % 2:
        data += b"\0"
    total = sum(int.from_bytes(data[index:index + 2], "big") for index in range(0, len(data), 2))
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return (~total) & 0xFFFF


def ip_packet(source, destination, protocol, payload, ttl=64, options=b"", flags=0x4000):
    words = 5 + len(options) // 4
    header = bytes((0x40 | words, 0)) + (words * 4 + len(payload)).to_bytes(2, "big")
    header += (0).to_bytes(2, "big") + flags.to_bytes(2, "big") + bytes((ttl, protocol, 0, 0))
    header += ipaddress.IPv4Address(source).packed + ipaddress.IPv4Address(destination).packed
    header += options
    return header[:10] + inet_checksum(header).to_bytes(2, "big") + header[12:] + payload


def udp_segment(source, destination, source_port, destination_port, payload, checksum=True):
    length = 8 + len(payload)
    segment = source_port.to_bytes(2, "big") + destination_port.to_bytes(2, "big")
    segment += length.to_bytes(2, "big") + b"\0\0" + payload
    if not checksum:
        return segment
    pseudo = ipaddress.IPv4Address(source).packed + ipaddress.IPv4Address(destination).packed
    pseudo += bytes((0, 17)) + length.to_bytes(2, "big")
    value = inet_checksum(pseudo + segment) or 0xFFFF  # RFC 768: a computed zero is sent as ones
    return segment[:6] + value.to_bytes(2, "big") + segment[8:]


def inner_ip_vectors():
    # RFC 1071 section 3 worked example and a widely published IPv4 header
    # anchor the independent checksum before any vector is derived from it.
    assert inet_checksum(bytes.fromhex("0001f203f4f5f6f7")) == 0x220D
    assert inet_checksum(bytes.fromhex("450000730000400040110000c0a80001c0a800c7")) == 0xB861
    checksums = [{"data": data.hex(), "checksum": f"{inet_checksum(data):04x}"}
                 for data in [pattern(size, 13) for size in (0, 1, 2, 3, 19, 20, 21, 64, 1399, 1400)]
                 + [bytes((255,)) * 8, bytes(8), bytes((255, 0, 0, 255, 1))]]
    packets = []
    cases = (("203.0.113.9", "10.64.0.2", 17, pattern(12, 5), 64, b""),
             ("198.51.100.20", "10.64.0.2", 17, pattern(1, 5), 1, b""),
             ("198.51.100.20", "10.64.0.2", 17, pattern(9, 7), 255, b""),
             ("203.0.113.9", "10.64.0.2", 17, pattern(30, 9), 64, bytes((1, 1, 1, 0))),
             ("203.0.113.9", "10.64.0.2", 17, pattern(30, 9), 64, bytes((1,) * 12)),
             ("10.64.0.1", "10.64.0.2", 17, b"", 64, b""))
    for source, destination, protocol, payload, ttl, options in cases:
        packet = ip_packet(source, destination, protocol, payload, ttl, options)
        packets.append({"source": source, "destination": destination, "protocol": protocol,
                        "ttl": ttl, "payload": payload.hex(), "packet": packet.hex(),
                        "padded": (packet + bytes(17)).hex(), "canonical": not options and ttl == 64})
    udp = []
    cases = (("203.0.113.9", "10.64.0.2", 53, 40000, b""),
             ("203.0.113.9", "10.64.0.2", 53, 40000, pattern(1, 3)),
             ("203.0.113.9", "10.64.0.2", 6881, 49152, pattern(2, 3)),
             ("203.0.113.9", "10.64.0.2", 6881, 49152, pattern(511, 3)),
             ("203.0.113.9", "10.64.0.2", 65535, 65535, pattern(1396, 3)))
    for source, destination, source_port, destination_port, payload in cases:
        segment = udp_segment(source, destination, source_port, destination_port, payload)
        udp.append({"source": source, "destination": destination, "source_port": source_port,
                    "destination_port": destination_port, "payload": payload.hex(),
                    "packet": ip_packet(source, destination, 17, segment).hex(), "canonical": True})
    # A payload whose checksum is computed as zero must be sent as 0xFFFF.
    for first in range(65536):
        payload = first.to_bytes(2, "big")
        pseudo = (ipaddress.IPv4Address("203.0.113.9").packed + ipaddress.IPv4Address("10.64.0.2").packed
                  + bytes((0, 17, 0, 10)))
        segment = (53).to_bytes(2, "big") + (40000).to_bytes(2, "big") + (10).to_bytes(2, "big") + b"\0\0" + payload
        if inet_checksum(pseudo + segment) == 0:
            break
    else:
        raise AssertionError("no zero-checksum payload")
    segment = udp_segment("203.0.113.9", "10.64.0.2", 53, 40000, payload)
    assert segment[6:8] == b"\xff\xff"
    udp.append({"source": "203.0.113.9", "destination": "10.64.0.2", "source_port": 53,
                "destination_port": 40000, "payload": payload.hex(),
                "packet": ip_packet("203.0.113.9", "10.64.0.2", 17, segment).hex(), "canonical": True})
    # RFC 768: an all-zero checksum field means the sender computed none.
    segment = udp_segment("203.0.113.9", "10.64.0.2", 53, 40000, pattern(7, 3), checksum=False)
    udp.append({"source": "203.0.113.9", "destination": "10.64.0.2", "source_port": 53,
                "destination_port": 40000, "payload": pattern(7, 3).hex(),
                "packet": ip_packet("203.0.113.9", "10.64.0.2", 17, segment).hex(), "canonical": False})
    return {"checksum": checksums, "ipv4": packets, "udp": udp}


def tcp_segment(source, destination, source_port, destination_port, sequence, acknowledgment,
                flags, window, payload=b"", options=b""):
    # RFC 9293 section 3.1: the checksum covers the pseudo-header, header and payload and
    # is mandatory; unlike UDP a computed zero is sent as zero.
    assert len(options) % 4 == 0
    offset = (20 + len(options)) // 4
    segment = source_port.to_bytes(2, "big") + destination_port.to_bytes(2, "big")
    segment += sequence.to_bytes(4, "big") + acknowledgment.to_bytes(4, "big")
    segment += bytes((offset << 4, flags)) + window.to_bytes(2, "big") + b"\0\0\0\0"
    segment += options + payload
    pseudo = ipaddress.IPv4Address(source).packed + ipaddress.IPv4Address(destination).packed
    pseudo += bytes((0, 6)) + len(segment).to_bytes(2, "big")
    return segment[:16] + inet_checksum(pseudo + segment).to_bytes(2, "big") + segment[18:]


def tcp_vectors():
    FIN, SYN, RST, PSH, ACK = 1, 2, 4, 8, 16
    mss = lambda value: bytes((2, 4)) + value.to_bytes(2, "big")
    local, remote = "10.64.0.2", "203.0.113.9"
    rows = []

    def add(source_port, destination_port, sequence, acknowledgment, flags, window, payload=b"",
            options=b"", mss_value=None, canonical=True):
        segment = tcp_segment(remote, local, source_port, destination_port, sequence,
                              acknowledgment, flags, window, payload, options)
        rows.append({"source": remote, "destination": local, "source_port": source_port,
                     "destination_port": destination_port, "sequence": sequence,
                     "acknowledgment": acknowledgment, "flags": flags, "window": window,
                     "mss": mss_value, "payload": payload.hex(),
                     "packet": ip_packet(remote, local, 6, segment).hex(), "canonical": canonical})

    add(443, 49152, 1000, 0, SYN, 65535, options=mss(1384), mss_value=1384)
    add(443, 49152, 4294967295, 7, SYN | ACK, 8192, options=mss(536), mss_value=536)
    add(443, 49152, 1, 2, ACK, 1000, pattern(100, 5))
    add(443, 49152, 4294967290, 4294967295, PSH | ACK, 65535, pattern(1384, 7))
    add(6881, 49152, 5, 6, ACK, 65535, pattern(1, 3))
    add(6881, 49152, 5, 6, ACK, 65535, pattern(3, 3))
    add(6881, 49152, 9, 10, FIN | ACK, 0)
    add(6881, 49152, 11, 12, RST, 0)
    add(6881, 49152, 11, 12, RST | ACK, 0)
    # Options that are valid but not produced by the builder: NOP padding, window scale,
    # SACK-permitted and timestamps are skipped by length; a stray MSS outside a SYN is ignored.
    add(443, 49152, 20, 21, ACK, 100, pattern(4, 9), options=bytes((1, 1, 8, 10)) + bytes(8),
        canonical=False)
    add(443, 49152, 22, 0, SYN, 100, options=bytes((1, 3, 3, 7)) + bytes((4, 2, 1, 0)),
        canonical=False)
    add(443, 49152, 24, 25, ACK, 100, options=mss(1000), canonical=False)
    add(443, 49152, 26, 27, ACK, 100, options=bytes((0, 0, 0, 0)), canonical=False)
    # A payload whose checksum is computed as zero is sent as zero.
    for first in range(65536):
        payload = first.to_bytes(2, "big")
        segment = tcp_segment(remote, local, 443, 49152, 30, 31, ACK, 100, payload)
        if segment[16:18] == b"\0\0":
            break
    else:
        raise AssertionError("no zero-checksum payload")
    add(443, 49152, 30, 31, ACK, 100, payload)
    return rows


def ipv6_packet(source, destination, next_header, payload, hop_limit=64):
    # RFC 8200 section 3: the fixed header only, traffic class and flow label zero.
    header = bytes((0x60, 0, 0, 0)) + len(payload).to_bytes(2, "big") + bytes((next_header, hop_limit))
    header += ipaddress.IPv6Address(source).packed + ipaddress.IPv6Address(destination).packed
    return header + payload


def ipv6_pseudo(source, destination, next_header, length):
    # RFC 8200 section 8.1: addresses, 32-bit upper-layer length, zeros and next header.
    return (ipaddress.IPv6Address(source).packed + ipaddress.IPv6Address(destination).packed
            + length.to_bytes(4, "big") + bytes((0, 0, 0, next_header)))


def ipv6_vectors():
    local, remote = "fd00:64::2", "2001:db8::9"
    udp = []
    for source_port, destination_port, payload in ((53, 40000, b""), (53, 40000, pattern(1, 3)),
                                                    (6881, 49152, pattern(511, 3)),
                                                    (65535, 65535, pattern(1376, 3))):
        length = 8 + len(payload)
        segment = source_port.to_bytes(2, "big") + destination_port.to_bytes(2, "big")
        segment += length.to_bytes(2, "big") + b"\0\0" + payload
        # RFC 8200 section 8.1: the checksum is mandatory; a computed zero is sent as ones.
        value = inet_checksum(ipv6_pseudo(remote, local, 17, length) + segment) or 0xFFFF
        segment = segment[:6] + value.to_bytes(2, "big") + segment[8:]
        udp.append({"source": remote, "destination": local, "source_port": source_port,
                    "destination_port": destination_port, "payload": payload.hex(),
                    "packet": ipv6_packet(remote, local, 17, segment).hex()})
    tcp = []
    for sequence, acknowledgment, flags, window, payload, options in (
            (1000, 0, 2, 65535, b"", bytes((2, 4)) + (1364).to_bytes(2, "big")),
            (1, 2, 16, 1000, pattern(100, 5), b""),
            (4294967290, 4294967295, 24, 65535, pattern(1364, 7), b"")):
        offset = (20 + len(options)) // 4
        segment = (443).to_bytes(2, "big") + (49152).to_bytes(2, "big")
        segment += sequence.to_bytes(4, "big") + acknowledgment.to_bytes(4, "big")
        segment += bytes((offset << 4, flags)) + window.to_bytes(2, "big") + bytes(4) + options + payload
        value = inet_checksum(ipv6_pseudo(remote, local, 6, len(segment)) + segment)
        segment = segment[:16] + value.to_bytes(2, "big") + segment[18:]
        tcp.append({"source": remote, "destination": local, "sequence": sequence,
                    "acknowledgment": acknowledgment, "flags": flags, "window": window,
                    "payload": payload.hex(), "packet": ipv6_packet(remote, local, 6, segment).hex()})

    def icmpv6(source, destination, message):
        # RFC 4443 section 2.3: the checksum covers the IPv6 pseudo-header.
        value = inet_checksum(ipv6_pseudo(source, destination, 58, len(message)) + message)
        return ipv6_packet(source, destination, 58, message[:2] + value.to_bytes(2, "big") + message[4:])

    body = (0x1234).to_bytes(2, "big") + (7).to_bytes(2, "big") + pattern(56, 11)
    echo = {"request": icmpv6(remote, local, bytes((128, 0, 0, 0)) + body).hex(),
            "reply": icmpv6(local, remote, bytes((129, 0, 0, 0)) + body).hex()}
    name, txid = "tracker.example.net", 0x2B3C
    aaaa = lambda address: ipaddress.IPv6Address(address).packed
    pointer = bytes((0xC0, 12))
    answers = dns_record(pointer, 28, 300, aaaa("2001:db8::10")) + dns_record(pointer, 28, 120, aaaa("2001:db8::11"))
    dns = {"id": txid, "name": name,
           "query": dns_message(txid, name, flags=0x0100, qtype=28).hex(),
           "response": dns_message(txid, name, answers, 2, qtype=28).hex(),
           "addresses": ["2001:db8::10", "2001:db8::11"], "ttl": 120,
           "a_in_aaaa": dns_message(txid, name, dns_record(pointer, 1, 300, bytes((192, 0, 2, 10))), 1,
                                    qtype=28).hex()}
    return {"local": local, "remote": remote, "udp": udp, "tcp": tcp, "echo": echo, "dns": dns}


def dns_name(name):
    wire = b""
    for label in name.rstrip(".").split("."):
        wire += bytes((len(label),)) + label.encode("ascii")
    return wire + b"\0"


def dns_message(txid, name, answers=b"", count=0, flags=0x8180, question=True, qtype=1):
    message = txid.to_bytes(2, "big") + flags.to_bytes(2, "big") + (1 if question else 0).to_bytes(2, "big")
    message += count.to_bytes(2, "big") + bytes(4)
    if question:
        message += dns_name(name) + qtype.to_bytes(2, "big") + (1).to_bytes(2, "big")
    return message + answers


def dns_record(owner, rtype, ttl, data, rclass=1):
    return owner + rtype.to_bytes(2, "big") + rclass.to_bytes(2, "big") + ttl.to_bytes(4, "big") + len(data).to_bytes(2, "big") + data


def dns_vectors():
    pointer = bytes((0xC0, 12))
    a = lambda address: ipaddress.IPv4Address(address).packed
    name, txid = "tracker.example.net", 0x1A2B
    queries = [{"id": txid, "name": name, "message": dns_message(txid, name, flags=0x0100).hex()},
               {"id": 0, "name": "A.b-c.d_e.", "message": dns_message(0, "A.b-c.d_e", flags=0x0100).hex()}]
    cname_owner = dns_name("edge.cdn.example.net")
    responses = []

    def case(label, message, expect, **extra):
        responses.append({"case": label, "id": txid, "name": name, "message": message.hex(),
                          "expect": expect, **extra})

    case("single", dns_message(txid, name, dns_record(pointer, 1, 300, a("192.0.2.10")), 1),
         "answers", addresses=["192.0.2.10"], ttl=300)
    case("minimum ttl", dns_message(txid, name, dns_record(pointer, 1, 900, a("192.0.2.10"))
                                    + dns_record(pointer, 1, 60, a("192.0.2.11")), 2),
         "answers", addresses=["192.0.2.10", "192.0.2.11"], ttl=60)
    chain = dns_record(pointer, 5, 120, cname_owner)
    target = 12 + len(dns_name(name)) + 4 + 12  # the CNAME data starts after its 12 byte record header
    chain_answer = dns_record(bytes((0xC0 | target >> 8, target & 255)), 1, 500, a("198.51.100.77"))
    case("cname chain", dns_message(txid, name, chain + chain_answer, 2),
         "answers", addresses=["198.51.100.77"], ttl=120)
    case("zero ttl", dns_message(txid, name, dns_record(pointer, 1, 0x80000000, a("192.0.2.10")), 1),
         "answers", addresses=["192.0.2.10"], ttl=0)
    case("special addresses dropped", dns_message(txid, name, dns_record(pointer, 1, 30, a("127.0.0.1"))
                                                  + dns_record(pointer, 1, 30, a("192.0.2.12")), 2),
         "answers", addresses=["192.0.2.12"], ttl=30)
    case("no address", dns_message(txid, name, b"", 0), "no_address")
    case("unrelated owner", dns_message(txid, name, dns_record(dns_name("other.example.net"), 1, 30,
                                                               a("192.0.2.13")), 1), "no_address")
    case("nxdomain", dns_message(txid, name, flags=0x8183), "name_error")
    case("servfail", dns_message(txid, name, flags=0x8182), "failure", code=2)
    case("truncated", dns_message(txid, name, flags=0x8380), "truncated")
    case("aaaa answer", dns_message(txid, name, dns_record(pointer, 28, 30, bytes(15) + b"\1"), 1), "reject")
    case("other record type", dns_message(txid, name, dns_record(pointer, 16, 30, b"\3abc"), 1), "reject")
    case("wrong class", dns_message(txid, name, dns_record(pointer, 1, 30, a("192.0.2.10"), rclass=3), 1), "reject")
    own = 12 + len(dns_name(name)) + 4
    case("self pointer", dns_message(txid, name, dns_record(bytes((0xC0 | own >> 8, own & 255)), 1, 30,
                                                            a("192.0.2.10")), 1), "reject")
    case("forward pointer", dns_message(txid, name, dns_record(bytes((0xC0, 0xF0)), 1, 30,
                                                               a("192.0.2.10")), 1), "reject")
    case("pointer loop", dns_message(txid, name, bytes((1, 97, 0xC0, own + 1)) + bytes(10), 1), "reject")
    case("reserved label", dns_message(txid, name, dns_record(bytes((0x40, 0)), 1, 30, a("192.0.2.10")), 1),
         "reject")
    case("rdlength five", dns_message(txid, name, dns_record(pointer, 1, 30, a("192.0.2.10") + b"\0"), 1),
         "reject")
    case("rdata overrun", dns_message(txid, name, dns_record(pointer, 1, 30, a("192.0.2.10"))[:-2], 1), "reject")
    case("missing answers", dns_message(txid, name, b"", 3), "reject")
    case("query echoed", dns_message(txid, name, flags=0x0100), "reject")
    case("other question", dns_message(txid, "other.example.net", dns_record(pointer, 1, 30, a("192.0.2.10")), 1),
         "reject")
    case("wrong qtype", dns_message(txid, name, dns_record(pointer, 1, 30, a("192.0.2.10")), 1, qtype=28), "reject")
    case("no question", dns_message(txid, name, b"", 0, question=False), "reject")
    case("other id", dns_message(txid + 1, name, dns_record(pointer, 1, 30, a("192.0.2.10")), 1), "reject")
    case("short header", dns_message(txid, name)[:11], "reject")
    long_label = "x" * 64
    case("overlong label", dns_message(txid, name, bytes((64,)) + long_label.encode() + b"\0", 1), "reject")
    return {"txid": txid, "name": name, "queries": queries, "responses": responses}


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
            "wireguard": transport_vectors(), "handshake": handshake_vectors(),
            "cookie": cookie_vectors(), "inner": inner_ip_vectors(), "tcp": tcp_vectors(), "dns": dns_vectors(),
            "ipv6": ipv6_vectors()}


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
          f"{len(data['handshake'])} Noise handshakes, {len(data['cookie'])} cookie exchanges, "
          f"{len(data['inner']['ipv4']) + len(data['inner']['udp'])} inner packets, "
          f"{len(data['dns']['responses'])} DNS replies, "
          f"{len(data['ipv6']['udp']) + len(data['ipv6']['tcp']) + 2} IPv6 packets")


if __name__ == "__main__":
    main()
