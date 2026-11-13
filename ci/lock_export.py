#!/usr/bin/env python3
"""TEMPORARY (arena export helper, not meant to stay on main):
gzip + base64 the workspace Cargo.lock and print it as ::notice:: chunks so a
locked-down sandbox can reassemble it."""
import base64
import gzip

import argparse

parser = argparse.ArgumentParser()
parser.add_argument("--skip", type=int, default=0, help="skip this many chunks")
parser.add_argument("--take", type=int, default=None, help="emit at most this many chunks")
args = parser.parse_args()

data = open("Cargo.lock", "rb").read()
enc = base64.b64encode(gzip.compress(data, 9)).decode()
CHUNK = 3500
n = (len(enc) + CHUNK - 1) // CHUNK
if args.skip == 0:
    print(f"::notice::CARGO_LOCK_TOTAL bytes={len(data)} b64={len(enc)} chunks={n}")
end = n if args.take is None else min(n, args.skip + args.take)
for i in range(args.skip, end):
    print(f"::notice::CARGO_LOCK_CHUNK {i} " + enc[i * CHUNK : (i + 1) * CHUNK])
