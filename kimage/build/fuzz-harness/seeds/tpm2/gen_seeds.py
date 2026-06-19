#!/usr/bin/env python3
"""Generate TPM 2.0 fuzz seeds. Each seed is a raw TPM command stream
(header tag/size/cc + params).

- getcap.seed   : a clean TPM2_GetCapability, the benchmark seed (read-mostly,
                  never crashes; the fuzzer mutates it to explore command paths).
- nv_write.seed : near-boundary NV_Write that drives the planted-bug gate. The
                  planted OOB in target_tpm2.c triggers on commandCode 0x00000137
                  with the size field at byte offsets 12..13; here it is 16 (<=32,
                  safe) with 40 trailing bytes, so one mutation of that field past
                  32 overflows the 32-byte scratch.
"""
import os, struct

ST_NO_SESSIONS = 0x8001
CC_GETCAP = 0x17A
CC_NV_WRITE = 0x137
here = os.path.dirname(os.path.abspath(__file__))


def cmd(cc, body):
    return struct.pack(">HI", ST_NO_SESSIONS, 10 + len(body)) + struct.pack(">I", cc) + body


def write(name, data):
    with open(os.path.join(here, name), "wb") as f:
        f.write(data)
    print(f"wrote {name} ({len(data)} bytes)")


# GetCapability(TPM_CAP_TPM_PROPERTIES=6, property=0x100, propertyCount=1)
write("getcap.seed", cmd(CC_GETCAP, struct.pack(">III", 0x6, 0x100, 1)))

# NV_Write gate seed: bytes after the cc are [authHi:2][sizeField:2][payload:40].
# sizeField = 16 keeps the seed itself safe; mutation past 32 triggers the plant.
nv_body = b"\x00\x00" + struct.pack(">H", 16) + bytes(range(40))
write("nv_write.seed", cmd(CC_NV_WRITE, nv_body))

if __name__ == "__main__":
    pass
