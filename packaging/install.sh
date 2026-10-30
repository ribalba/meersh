#!/bin/sh
# Installs the static meersh build for this machine from the GitHub releases.
#
#   curl -fsSL https://github.com/ribalba/meersh/releases/latest/download/install.sh | sh
#
# The binaries go to ~/.local/bin, or /usr/local/bin when run as root. Running it again
# updates meersh, also while a meersh session is open. Environment variables change the
# defaults: MEERSH_VERSION=v0.1.0 picks a release, MEERSH_INSTALL_DIR the directory for the
# binaries and MEERSH_DOWNLOAD_URL the place the release files are fetched from.
set -eu

# Everything runs from main, so a download cut short cannot run half a script.
main() {
  repo=https://github.com/ribalba/meersh
  version=${MEERSH_VERSION:-latest}
  if [ "$version" = latest ]; then
    base=${MEERSH_DOWNLOAD_URL:-$repo/releases/latest/download}
  else
    base=${MEERSH_DOWNLOAD_URL:-$repo/releases/download/$version}
  fi

  if [ "$(uname -s)" != Linux ]; then
    fail "meersh runs on Linux, this is $(uname -s)"
  fi
  case "$(uname -m)" in
    x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
    aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
    *) fail "there is no build for $(uname -m). See $repo for building from source" ;;
  esac

  if [ "$(id -u)" = 0 ]; then
    bindir=${MEERSH_INSTALL_DIR:-/usr/local/bin}
    docdir=/usr/local/share/doc/meersh
  else
    bindir=${MEERSH_INSTALL_DIR:-$HOME/.local/bin}
    docdir=${XDG_DATA_HOME:-$HOME/.local/share}/doc/meersh
  fi

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  name=meersh-$target
  echo "Downloading $base/$name.tar.gz"
  fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz"
  fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && grep " $name.tar.gz\$" SHA256SUMS | sha256sum -c - >/dev/null) ||
      fail "the checksum of $name.tar.gz does not match SHA256SUMS"
  else
    echo "meersh: sha256sum is missing, so the download is not verified" >&2
  fi
  tar -xzf "$tmp/$name.tar.gz" -C "$tmp"

  # A copy next to the target and a rename, because a running binary cannot be overwritten.
  mkdir -p "$bindir" "$docdir"
  for bin in meersh meer-box; do
    cp "$tmp/$name/$bin" "$bindir/.$bin.new"
    chmod 755 "$bindir/.$bin.new"
    mv -f "$bindir/.$bin.new" "$bindir/$bin"
  done
  cp "$tmp/$name/config.example.toml" "$docdir/config.example.toml"

  echo "Installed $("$bindir/meersh" --version) to $bindir"
  case ":$PATH:" in
    *":$bindir:"*) ;;
    *) echo "$bindir is not in your PATH. Add it in your shell profile: export PATH=\"$bindir:\$PATH\"" ;;
  esac
  echo "An example configuration is in $docdir/config.example.toml"
}

fail() {
  echo "meersh: $1" >&2
  exit 1
}

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$2" "$1" || fail "cannot download $1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1" || fail "cannot download $1"
  else
    fail "the installer needs curl or wget"
  fi
}

main "$@"
