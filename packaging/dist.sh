#!/bin/sh
# Builds the release files for one target into dist/: a tarball, a .deb and an .rpm. The
# names carry no version, so github.com/.../releases/latest/download/<name> stays valid.
# The release workflow runs it for x86_64 and aarch64. Locally it needs the rustup target,
# musl-gcc and nfpm.
#
#   packaging/dist.sh x86_64-unknown-linux-musl
set -eu

target=${1:-x86_64-unknown-linux-musl}
case "$target" in
  x86_64-*) deb_arch=amd64 rpm_arch=x86_64 ;;
  aarch64-*) deb_arch=arm64 rpm_arch=aarch64 ;;
  *) echo "dist.sh: no package architecture for $target" >&2; exit 1 ;;
esac

cd "$(dirname "$0")/.."
# Static linking is the default for musl targets. musl-gcc compiles the C parts of ring.
TARGET_CC=${TARGET_CC:-musl-gcc} cargo build --release --locked --target "$target" --bin meersh --bin meer-box
bin=${CARGO_TARGET_DIR:-target}/$target/release

# The binaries must start on any distribution, so nothing may be linked dynamically.
for exe in "$bin/meersh" "$bin/meer-box"; do
  headers=$(readelf -l "$exe")
  case "$headers" in
    *"program interpreter"*) echo "dist.sh: $exe is linked dynamically" >&2; exit 1 ;;
  esac
done

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
name=meersh-$target
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p dist "$stage/$name"
cp "$bin/meersh" "$bin/meer-box" config.example.toml README.md LICENSE "$stage/$name/"
tar -C "$stage" -czf "dist/$name.tar.gz" "$name"

export ARCH=$deb_arch VERSION=$version BIN_DIR=$bin
${NFPM:-nfpm} package -f packaging/nfpm.yaml -p deb -t "dist/meersh-$deb_arch.deb"
${NFPM:-nfpm} package -f packaging/nfpm.yaml -p rpm -t "dist/meersh-$rpm_arch.rpm"
