#!/bin/sh
# Installs the `chronos` binary: picks the build for this OS and CPU, checks its SHA-256 against
# the release's SHA256SUMS, and puts it in ~/.local/bin (or $CHRONOS_INSTALL_DIR). It never runs anything
# as root: for /usr/local/bin, run it as `sudo env CHRONOS_INSTALL_DIR=/usr/local/bin sh install.sh` yourself.
#
#   curl -fsSL https://github.com/__REPO__/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/__REPO__/releases/latest/download/install.sh | CHRONOS_VERSION=v0.1.0 sh
#
# CHRONOS_VERSION picks a release tag (default: the release this copy came from). CHRONOS_BASE_URL overrides where the files
# come from (a release, a bucket, or file:// for tests). CHRONOS_VERIFY=1 also checks the build's signature
# with cosign (keyless, Sigstore): proof it was built and signed by Chronos DB's release workflow.
set -eu

# everything runs from main at the end, so a download cut off halfway runs nothing
main() {
REPO="__REPO__" # filled in by the release workflow: where the releases are
SIGNER="__SIGNER__" # and the repository whose release workflow signs them
VERSION="${CHRONOS_VERSION:-__VERSION__}" # so is its tag; a copy from the repo installs latest
case "$VERSION" in
latest | __*) rel=latest/download ref= ;;
*[!A-Za-z0-9._-]*) fail "CHRONOS_VERSION must be a release tag like v0.1.0, not '$VERSION'" ;;
*) rel="download/$VERSION" ref="refs/tags/$VERSION" ;;
esac
BASE="${CHRONOS_BASE_URL:-https://github.com/$REPO/releases/$rel}"
DIR="${CHRONOS_INSTALL_DIR:-$HOME/.local/bin}"

os=$(uname -s)
cpu=$(uname -m)
case "$os/$cpu" in
Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
Darwin/x86_64) target=x86_64-apple-darwin ;;
Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-musl ;;
Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
*) fail "no prebuilt binary for $os $cpu yet: say which you need at https://github.com/$REPO/issues" ;;
esac

command -v curl >/dev/null || fail "needs curl"
if command -v sha256sum >/dev/null; then
    sha() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null; then
    sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    fail "needs sha256sum or shasum to check the download"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp" "$DIR/.chronos.tmp.$$"' EXIT
file="chronos-$target.tar.gz"
curl -fsSL "$BASE/$file" -o "$tmp/$file" || fail "could not download $BASE/$file"
curl -fsSL "$BASE/SHA256SUMS" -o "$tmp/SHA256SUMS" || fail "could not download $BASE/SHA256SUMS"
want=$(awk -v f="$file" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$want" ] || fail "SHA256SUMS has no entry for $file"
got=$(sha "$tmp/$file")
[ "$got" = "$want" ] || fail "checksum mismatch for $file (expected $want, got $got); nothing was installed"
if [ "${CHRONOS_VERIFY:-}" = 1 ]; then
    command -v cosign >/dev/null || fail "CHRONOS_VERIFY=1 needs cosign: https://docs.sigstore.dev/cosign/system_config/installation/"
    curl -fsSL "$BASE/$file.sigstore.json" -o "$tmp/$file.sigstore.json" || fail "could not download $BASE/$file.sigstore.json"
    # signed only by the release workflow, through GitHub's OIDC issuer, for this tag (latest: a v tag)
    wf="https://github.com/$SIGNER/.github/workflows/release.yml"
    if [ -n "$ref" ]; then
        id="--certificate-identity=$wf@$ref"
    else
        id="--certificate-identity-regexp=^$(printf %s "$wf" | sed 's/\./\\./g')@refs/tags/v"
    fi
    cosign verify-blob --bundle "$tmp/$file.sigstore.json" \
        --certificate-oidc-issuer https://token.actions.githubusercontent.com "$id" "$tmp/$file" >/dev/null 2>&1 ||
        fail "$file has no valid signature from $SIGNER's release workflow; nothing was installed"
fi

tar -xzf "$tmp/$file" -C "$tmp"
[ -f "$tmp/chronos" ] || fail "$file has no chronos binary in it"
chmod +x "$tmp/chronos"
# it runs here (x86-64 builds need an x86-64-v2 CPU: POPCNT, SSE4.2) before anything is put in place
version=$("$tmp/chronos" --version 2>/dev/null) || fail "the $target build doesn't run on this machine; nothing was installed"
# nothing here runs as root: a folder you can't write is yours to choose (sudo, or another folder)
mkdir -p "$DIR" 2>/dev/null && [ -w "$DIR" ] ||
    fail "can't write to $DIR; set CHRONOS_INSTALL_DIR to a folder you can write (to use $DIR, run this script with sudo yourself)"
# copy next to the target, then rename: a same-directory mv is atomic, so an interrupted
# install never leaves a half-written chronos (mv from $tmp may cross filesystems and copy)
cp "$tmp/chronos" "$DIR/.chronos.tmp.$$"
chmod +x "$DIR/.chronos.tmp.$$"
mv -f "$DIR/.chronos.tmp.$$" "$DIR/chronos"
echo "installed $version ($target) to $DIR/chronos"
case ":$PATH:" in
*":$DIR:"*) ;;
*) echo "add it to your PATH: export PATH=\"$DIR:\$PATH\"" ;;
esac
}

fail() {
    echo "chronos install: $*" >&2
    exit 1
}

main "$@"
