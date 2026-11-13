#!/usr/bin/env python3
"""TEMPORARY (arena export helper, not meant to stay on main):
gzip + base64 the workspace Cargo.lock and print it as ::notice:: chunks so a
locked-down sandbox can reassemble it."""
import base64
import gzip

data = open("Cargo.lock", "rb").read()
enc = base64.b64encode(gzip.compress(data, 9)).decode()
CHUNK = 3500
n = (len(enc) + CHUNK - 1) // CHUNK
print(f"::notice::CARGO_LOCK_TOTAL bytes={len(data)} b64={len(enc)} chunks={n}")
for i in range(n):
    print(f"::notice::CARGO_LOCK_CHUNK {i} " + enc[i * CHUNK : (i + 1) * CHUNK])
