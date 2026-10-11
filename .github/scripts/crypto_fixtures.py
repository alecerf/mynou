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
    return {"schema": 1, "blake2s": hashes, "hmac": hmacs, "hkdf": derivations}


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
