#!/usr/bin/env bash
# Download the CC BY-SA bird fixtures into tests/e2e/fixtures/birds and verify
# every SHA-256. Re-run to repair a missing or truncated file.
set -euo pipefail
cd "$(dirname "$0")"
python3 - <<'EOF'
import hashlib
import json
import os
import subprocess

manifest = json.load(open("manifest.json"))
out = "../fixtures/birds"
os.makedirs(out, exist_ok=True)
for photo in manifest["photos"]:
    path = os.path.join(out, photo["file"])
    good = (
        os.path.exists(path)
        and hashlib.sha256(open(path, "rb").read()).hexdigest() == photo["sha256"]
    )
    if not good:
        subprocess.run(
            [
                "curl", "-fsSL",
                "-A", "photo-edit-simplified-fixtures/1.0 (+https://github.com/agyeiagyeiagyei/photo-edit-simplified)",
                "-o", path,
                photo["source_url"],
            ],
            check=True,
        )
        actual = hashlib.sha256(open(path, "rb").read()).hexdigest()
        if actual != photo["sha256"]:
            raise SystemExit(f"sha256 mismatch for {photo['file']}: {actual}")
        print("fetch", photo["file"])
    else:
        print("ok   ", photo["file"])

print("\nAttribution (CC BY-SA, see manifest.json for links):")
for photo in manifest["photos"]:
    print(f"  {photo['file']} — {photo['artist']} — {photo['license']}")
EOF
