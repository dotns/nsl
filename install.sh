#!/bin/sh
# nsl installer — downloads the right prebuilt binary for your OS/arch from the
# latest (or a pinned) GitHub release and installs it.
#
# Quick install:
#   curl -fsSL https://raw.githubusercontent.com/dotns/nsl/main/install.sh | sh
#   wget -qO-  https://raw.githubusercontent.com/dotns/nsl/main/install.sh | sh
#
# Options (when piping, pass them after `sh -s --`):
#   curl -fsSL .../install.sh | sh -s -- --dir "$HOME/.local/bin"
#
#   -d, --dir DIR       install directory (default: /usr/local/bin)
#   -v, --version VER   version to install, e.g. v0.1.9 (default: latest)
#   -h, --help          show this help
#
# Environment equivalents: NSL_INSTALL_DIR, NSL_VERSION.
#
# Windows is not covered by this script — use `npm i -g @dotns/nsl` instead.

set -eu

REPO="dotns/nsl"
BIN="nsl"
INSTALL_DIR="${NSL_INSTALL_DIR:-/usr/local/bin}"
VERSION="${NSL_VERSION:-}"

err() { printf 'nsl-install: error: %s\n' "$*" >&2; exit 1; }
info() { printf '%s\n' "$*" >&2; }
have() { command -v "$1" >/dev/null 2>&1; }

usage() {
	cat <<'EOF'
nsl installer — download and install the nsl binary for your OS/arch.

Usage:
  curl -fsSL https://raw.githubusercontent.com/dotns/nsl/main/install.sh | sh
  wget -qO-  https://raw.githubusercontent.com/dotns/nsl/main/install.sh | sh

Options (when piping, pass them after `sh -s --`):
  -d, --dir DIR       install directory (default: /usr/local/bin)
  -v, --version VER   version to install, e.g. v0.1.9 (default: latest)
  -h, --help          show this help

Environment equivalents: NSL_INSTALL_DIR, NSL_VERSION.
Windows: use `npm i -g @dotns/nsl` instead.
EOF
}

# --- parse arguments -------------------------------------------------------
while [ $# -gt 0 ]; do
	case "$1" in
	-d | --dir)
		[ $# -ge 2 ] || err "$1 needs a value"
		INSTALL_DIR="$2"
		shift 2
		;;
	-v | --version)
		[ $# -ge 2 ] || err "$1 needs a value"
		VERSION="$2"
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*) err "unknown option: $1 (try --help)" ;;
	esac
done

# --- pick a downloader -----------------------------------------------------
if have curl; then
	DL=curl
elif have wget; then
	DL=wget
else
	err "need either curl or wget installed"
fi
have tar || err "need tar installed"

fetch() { # fetch URL -> stdout
	if [ "$DL" = curl ]; then curl -fsSL "$1"; else wget -qO- "$1"; fi
}
download() { # download URL FILE
	if [ "$DL" = curl ]; then curl -fsSL -o "$2" "$1"; else wget -qO "$2" "$1"; fi
}

# --- detect OS / arch ------------------------------------------------------
os=$(uname -s)
arch=$(uname -m)
case "$os" in
Linux) os=linux ;;
Darwin) os=darwin ;;
*) err "unsupported OS '$os' (Windows: use 'npm i -g @dotns/nsl')" ;;
esac
case "$arch" in
x86_64 | amd64) arch=x64 ;;
aarch64 | arm64) arch=arm64 ;;
*) err "unsupported architecture '$arch'" ;;
esac
subpkg="${os}-${arch}"

# --- resolve version -------------------------------------------------------
if [ -z "$VERSION" ]; then
	info "Resolving latest release..."
	# Read the whole response first: piping curl straight into `grep -m1`
	# makes grep close the pipe early, which surfaces as a curl write error.
	latest_json=$(fetch "https://api.github.com/repos/${REPO}/releases/latest") ||
		err "could not query the latest release (set --version, or check your network)"
	VERSION=$(printf '%s\n' "$latest_json" |
		grep -m1 '"tag_name"' |
		sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')
	[ -n "$VERSION" ] || err "could not parse the latest version (set --version)"
fi
# Normalize to a leading 'v'.
case "$VERSION" in
v*) ;;
*) VERSION="v$VERSION" ;;
esac

asset="${BIN}-${subpkg}.tar.gz"
url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"

# --- download + extract ----------------------------------------------------
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t nsl-install)
trap 'rm -rf "$tmp"' EXIT INT TERM

info "Downloading ${BIN} ${VERSION} (${subpkg})..."
download "$url" "$tmp/$asset" || err "download failed: $url"
tar -xzf "$tmp/$asset" -C "$tmp" || err "failed to extract $asset"
[ -f "$tmp/$BIN" ] || err "archive did not contain '$BIN'"
chmod +x "$tmp/$BIN"

# --- install ---------------------------------------------------------------
dest="${INSTALL_DIR%/}/$BIN"
if mkdir -p "$INSTALL_DIR" 2>/dev/null && [ -w "$INSTALL_DIR" ]; then
	mv -f "$tmp/$BIN" "$dest"
elif have sudo; then
	info "Writing to $INSTALL_DIR requires elevated permissions; using sudo..."
	sudo mkdir -p "$INSTALL_DIR"
	sudo mv -f "$tmp/$BIN" "$dest"
else
	err "cannot write to $INSTALL_DIR and sudo is unavailable; re-run with --dir \"\$HOME/.local/bin\""
fi

# --- verify + hints --------------------------------------------------------
[ -x "$dest" ] || err "installation failed"
installed=$("$dest" --version 2>/dev/null || echo "$BIN $VERSION")
info ""
info "Installed $installed -> $dest"

case ":$PATH:" in
*":${INSTALL_DIR%/}:"*) ;;
*)
	info ""
	info "note: ${INSTALL_DIR%/} is not on your PATH. Add it, e.g.:"
	info "  export PATH=\"${INSTALL_DIR%/}:\$PATH\""
	;;
esac
info ""
info "Run '${BIN} --help' to get started."
