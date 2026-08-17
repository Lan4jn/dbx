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
  docs/dbx-gateway/local-database-targets.md
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

require_heading() {
  local document="$1"
  local heading="$2"
  if ! grep -qF "$heading" "$runtime_dir/$document"; then
    echo "missing documentation heading '$heading' in $document" >&2
    exit 1
  fi
}
require_heading docs/dbx-gateway.md "## 信任边界"
require_heading docs/dbx-gateway.md "## 网络拓扑"
require_heading docs/dbx-gateway.md "## 文档入口"
require_heading docs/dbx-gateway/main-gateway.md "## 安装"
require_heading docs/dbx-gateway/main-gateway.md "## HTTPS 回退"
require_heading docs/dbx-gateway/main-gateway.md "## ACL"
require_heading docs/dbx-gateway/main-gateway.md "## systemd"
require_heading docs/dbx-gateway/main-gateway.md "## 升级与回滚"
require_heading docs/dbx-gateway/edge-gateway.md "## 令牌领证"
require_heading docs/dbx-gateway/edge-gateway.md "## 本地目标"
require_heading docs/dbx-gateway/edge-gateway.md "## 重连与迁移"
require_heading docs/dbx-gateway/local-database-targets.md "## 关系型数据库"
require_heading docs/dbx-gateway/local-database-targets.md "## 多节点与多端口限制"
require_heading docs/dbx-gateway/pki.md "## 离线 Root CA"
require_heading docs/dbx-gateway/pki.md "## 在线 Edge CA"
require_heading docs/dbx-gateway/pki.md "## 续期与吊销"
require_heading docs/dbx-gateway/pki.md "## 备份与恢复"
require_heading docs/dbx-gateway/configuration.md "## Main 字段"
require_heading docs/dbx-gateway/configuration.md "## Edge 字段"
require_heading docs/dbx-gateway/configuration.md "## PKI 字段"
require_heading docs/dbx-gateway/operations.md "## 抓包验收"
require_heading docs/dbx-gateway/operations.md "## 到期监控"
require_heading docs/dbx-gateway/operations.md "## 故障排查"
require_heading docs/dbx-gateway/operations.md "## 卸载"

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
