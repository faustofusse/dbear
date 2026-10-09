#!/usr/bin/env bash
# Renders assets/icon.svg into packaging/windows/dbear.ico (16–256 px, PNG-compressed entries).
# The macOS icon leaves a wide margin (Apple's icon grid); Windows icons fill more of the square,
# so the render crops some of it. Run in `nix develop .#windows` (needs resvg and python3).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [[ -z "${IN_NIX_SHELL:-}" ]] && command -v nix >/dev/null; then
    exec nix develop .#windows -c "$0" "$@"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
sed 's|viewBox="0 0 1024 1024"|viewBox="64 64 896 896"|' assets/icon.svg >"$WORK/icon.svg"
for size in 16 20 24 32 40 48 64 128 256; do
    resvg -w "$size" -h "$size" "$WORK/icon.svg" "$WORK/$size.png"
done

python3 - "$WORK" packaging/windows/dbear.ico <<'EOF'
import struct, sys, pathlib
work, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
images = [(s, (work / f"{s}.png").read_bytes()) for s in sizes]
header = struct.pack("<HHH", 0, 1, len(images))
offset = len(header) + 16 * len(images)
entries, data = b"", b""
for size, png in images:
    dim = 0 if size >= 256 else size  # 0 means 256
    entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png), offset + len(data))
    data += png
out.parent.mkdir(parents=True, exist_ok=True)
out.write_bytes(header + entries + data)
print(f"wrote {out} ({out.stat().st_size} bytes)")
EOF
