#!/usr/bin/env bash
set -euo pipefail

target="${DBX_GATEWAY_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"
output_dir="${DBX_GATEWAY_OUTPUT_DIR:-dist-gateway}"
case "$target" in
  x86_64-*) arch="x64" ;;
  aarch64-*) arch="arm64" ;;
  *) echo "unsupported gateway target: $target" >&2; exit 2 ;;
esac

version="${DBX_GATEWAY_VERSION:-$(cargo pkgid -p dbx-gateway | sed -E 's/.*[@#]([^@#]+)$/\1/')}"
client_version="$(node -p "require('./package.json').version")"
if [ "$version" != "$client_version" ]; then
  echo "gateway version $version does not match client version $client_version" >&2
  exit 1
fi
target_dir="${CARGO_TARGET_DIR:-target}"
binary_dir="${target_dir}/${target}/release"
if [ ! -x "$binary_dir/dbx-gateway" ] && [ "$target" = "$(rustc -vV | sed -n 's/^host: //p')" ]; then
  binary_dir="${target_dir}/release"
fi
for binary in dbx-gateway dbx-gateway-pki; do
  if [ ! -x "$binary_dir/$binary" ]; then
    echo "missing gateway binary: $binary_dir/$binary" >&2
    exit 1
  fi
done

package_name="DBX_Gateway_${version}_${arch}"
package_dir="$output_dir/$package_name"
tarball="$output_dir/$package_name.tar.gz"
rm -rf "$package_dir"
mkdir -p "$package_dir/bin" "$package_dir/examples" "$package_dir/systemd" "$package_dir/docs/dbx-gateway"
install -m 0755 "$binary_dir/dbx-gateway" "$binary_dir/dbx-gateway-pki" "$package_dir/bin/"
install -m 0644 examples/dbx-gateway/main.toml examples/dbx-gateway/edge.toml examples/dbx-gateway/pki.toml "$package_dir/examples/"
install -m 0644 examples/dbx-gateway/systemd/*.service "$package_dir/systemd/"
install -m 0644 docs/dbx-gateway.md "$package_dir/docs/"
install -m 0644 docs/dbx-gateway/*.md "$package_dir/docs/dbx-gateway/"
install -m 0644 LICENSE "$package_dir/"

(cd "$package_dir" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
COPYFILE_DISABLE=1 tar --no-xattrs -C "$output_dir" -czf "$tarball" "$package_name"
(cd "$output_dir" && sha256sum "$(basename "$tarball")" > "$(basename "$tarball").sha256")
echo "$tarball"
