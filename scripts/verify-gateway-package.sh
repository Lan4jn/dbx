#!/usr/bin/env bash
set -euo pipefail

tarball="${1:-}"
if [ -z "$tarball" ]; then
  echo "usage: $0 <gateway-package.tar.gz>" >&2
  exit 2
fi
if [ ! -f "$tarball" ]; then
  echo "missing gateway package: $tarball" >&2
  exit 1
fi
if [ ! -f "${tarball}.sha256" ]; then
  echo "missing gateway package checksum: ${tarball}.sha256" >&2
  exit 1
fi

(cd "$(dirname "$tarball")" && sha256sum -c "$(basename "$tarball").sha256")

runtime_dir="$(mktemp -d)"
trap 'rm -rf "$runtime_dir"' EXIT
tar -xzf "$tarball" -C "$runtime_dir" --strip-components=1

required_paths=(
  bin/dbx-gateway
  bin/dbx-gateway-pki
  examples/main.toml
  examples/edge.toml
  examples/pki.toml
  systemd/dbx-gateway-main.service
  systemd/dbx-gateway-edge.service
  systemd/dbx-gateway-pki.service
  docs/dbx-gateway.md
  docs/dbx-gateway/main-gateway.md
  docs/dbx-gateway/edge-gateway.md
  docs/dbx-gateway/pki.md
  docs/dbx-gateway/configuration.md
  docs/dbx-gateway/operations.md
  SHA256SUMS
)
for path in "${required_paths[@]}"; do
  if [ ! -f "$runtime_dir/$path" ]; then
    echo "missing packaged gateway path: $path" >&2
    exit 1
  fi
done

(cd "$runtime_dir" && sha256sum -c SHA256SUMS)
"$runtime_dir/bin/dbx-gateway" --help >/dev/null
"$runtime_dir/bin/dbx-gateway" --version >/dev/null
"$runtime_dir/bin/dbx-gateway-pki" --help >/dev/null
"$runtime_dir/bin/dbx-gateway-pki" --version >/dev/null
if "$runtime_dir/bin/dbx-gateway" --config /dev/null check-config >/dev/null 2>&1; then
  echo "check-config accepted an empty configuration" >&2
  exit 1
fi

for unit in "$runtime_dir"/systemd/*.service; do
  grep -Eq '^User=dbx-gateway(-pki)?$' "$unit"
  grep -q '^NoNewPrivileges=true$' "$unit"
  grep -q '^PrivateTmp=true$' "$unit"
  grep -q '^ProtectSystem=strict$' "$unit"
  grep -q '^LimitCORE=0$' "$unit"
  grep -q '^ReadWritePaths=' "$unit"
done

echo "gateway package verification passed: $tarball"
