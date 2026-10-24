#!/bin/sh
# install.sh against a fake release on disk (file:// URLs): installs a good build, refuses a
# tampered one or one missing from SHA256SUMS without installing anything, checks the build's
# cosign signature when asked (CHRONOS_VERIFY=1), and refuses an OS it has no build for.
#   sh tests/install_test.sh
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT

# a release for every target: a stand-in `chronos` that prints its target
mkdir -p "$t/rel" "$t/pkg"
for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    printf '#!/bin/sh\necho %s\n' "$target" >"$t/pkg/chronos"
    tar -czf "$t/rel/chronos-$target.tar.gz" -C "$t/pkg" chronos
    echo '{}' >"$t/rel/chronos-$target.tar.gz.sigstore.json" # the fake cosign below only checks it's there
done
(cd "$t/rel" && for f in *.tar.gz; do
    if command -v sha256sum >/dev/null; then sha256sum "$f"; else shasum -a 256 "$f"; fi
done >SHA256SUMS)

run() { CHRONOS_BASE_URL="file://$t/rel" CHRONOS_INSTALL_DIR="$t/bin" sh "$root/install.sh"; }
fail() {
    echo "FAIL: $*" >&2
    exit 1
}

run >"$t/out" 2>&1 || fail "a good release installs: $(cat "$t/out")"
"$t/bin/chronos" | grep -q -- "-" || fail "the installed binary runs"

# CHRONOS_VERIFY=1: a fake cosign that vouches (only for the release builds' service account, $COSIGN_ACCOUNT, through
# Google's OIDC issuer; or the GitHub release workflow's identity, $COSIGN_WANT, through GitHub's; each for the
# downloaded bundle) or refuses; with no cosign at all, it fails
mkdir -p "$t/cosign"
cat >"$t/cosign/cosign" <<'COSIGN'
#!/bin/sh
[ "$1 $2" = "verify-blob --bundle" ] && [ -f "$3" ] || exit 1
bundle=$3
shift 3
[ -n "${COSIGN_ACCOUNT:-}" ] &&
    [ "$*" = "--certificate-identity $COSIGN_ACCOUNT --certificate-oidc-issuer https://accounts.google.com ${bundle%.sigstore.json}" ] &&
    exit 0
[ "$*" = "--certificate-oidc-issuer https://token.actions.githubusercontent.com ${COSIGN_WANT:-} ${bundle%.sigstore.json}" ] ||
    { echo "cosign got: $*" >&2; exit 1; }
COSIGN
chmod +x "$t/cosign/cosign"
rm -rf "$t/bin"
(COSIGN_ACCOUNT=__SIGNER_ACCOUNT__ PATH="$t/cosign:$PATH" CHRONOS_VERIFY=1 run) >"$t/out" 2>&1 ||
    fail "a build signed by the release builds' account installs: $(cat "$t/out")"
rm -rf "$t/bin"
# (and one signed by the GitHub release workflow, as releases up to v0.4.1 are)
(COSIGN_WANT='--certificate-identity-regexp=^https://github\.com/__SIGNER__/\.github/workflows/release\.yml@refs/tags/v' \
    PATH="$t/cosign:$PATH" CHRONOS_VERIFY=1 run) >"$t/out" 2>&1 || fail "a signed build installs: $(cat "$t/out")"
if (COSIGN_WANT=other PATH="$t/cosign:$PATH" CHRONOS_VERIFY=1 run) >"$t/out" 2>&1; then fail "cosign refusing other arguments must fail"; fi
# a tagged copy names the account that signs, and pins an older signature to its tag's workflow; it leaves only
# chronos in the install dir
sed -e "s|__REPO__|o/r|g" -e "s|__SIGNER_ACCOUNT__|rel@p.iam.gserviceaccount.com|g" -e "s|__SIGNER__|s/e|g" \
    -e "s|__VERSION__|v9.9.9|g" "$root/install.sh" >"$t/tagged.sh"
rm -rf "$t/bin"
COSIGN_ACCOUNT=rel@p.iam.gserviceaccount.com PATH="$t/cosign:$PATH" CHRONOS_VERIFY=1 CHRONOS_BASE_URL="file://$t/rel" \
    CHRONOS_INSTALL_DIR="$t/bin" sh "$t/tagged.sh" >"$t/out" 2>&1 || fail "a tagged build signed by the account installs: $(cat "$t/out")"
if (COSIGN_ACCOUNT=other@p.iam.gserviceaccount.com PATH="$t/cosign:$PATH" CHRONOS_VERIFY=1 CHRONOS_BASE_URL="file://$t/rel" \
    CHRONOS_INSTALL_DIR="$t/bin2" sh "$t/tagged.sh") >"$t/out" 2>&1; then fail "another account's signature must fail"; fi
rm -rf "$t/bin"
COSIGN_WANT='--certificate-identity=https://github.com/s/e/.github/workflows/release.yml@refs/tags/v9.9.9' \
    PATH="$t/cosign:$PATH" CHRONOS_VERIFY=1 CHRONOS_BASE_URL="file://$t/rel" CHRONOS_INSTALL_DIR="$t/bin" sh "$t/tagged.sh" >"$t/out" 2>&1 ||
    fail "a tagged signed build installs: $(cat "$t/out")"
[ "$(ls -A "$t/bin")" = chronos ] || fail "only chronos is left in the install dir: $(ls -A "$t/bin")"
printf '#!/bin/sh\nexit 1\n' >"$t/cosign/cosign"
rm -rf "$t/bin"
if (PATH="$t/cosign:$PATH" CHRONOS_VERIFY=1 run) >"$t/out" 2>&1; then fail "an unsigned build must fail"; fi
grep -q "no valid signature" "$t/out" || fail "the error says no valid signature: $(cat "$t/out")"
[ ! -e "$t/bin/chronos" ] || fail "an unsigned build installs nothing"
# without cosign: a PATH of links to every other command
mkdir -p "$t/nocosign"
(IFS=:; for d in $PATH; do for f in "$d"/*; do
    n=${f##*/}
    [ "$n" = cosign ] || [ ! -x "$f" ] || [ -e "$t/nocosign/$n" ] || ln -s "$f" "$t/nocosign/$n"
done; done)
if (PATH="$t/nocosign" CHRONOS_VERIFY=1 run) >"$t/out" 2>&1; then fail "CHRONOS_VERIFY=1 without cosign must fail"; fi
grep -q "needs cosign" "$t/out" || fail "the error says cosign is needed: $(cat "$t/out")"
# (a call of `run` with assignments before it is in a subshell: bash in POSIX mode, macOS's sh, keeps a function
# call's assignments, so CHRONOS_VERIFY=1, and the fake uname and curl below on PATH, would linger)
(PATH="$t/cosign:$PATH" CHRONOS_VERIFY=0 run) >"$t/out" 2>&1 || fail "without CHRONOS_VERIFY, cosign isn't asked: $(cat "$t/out")"

rm -rf "$t/bin"
sed 's/^\(.\)/0\1/; s/^\(.\{64\}\)./\1/' "$t/rel/SHA256SUMS" >"$t/sums" && mv "$t/sums" "$t/rel/SHA256SUMS"
if run >"$t/out" 2>&1; then fail "a tampered checksum must fail"; fi
grep -q "checksum mismatch" "$t/out" || fail "the error says checksum mismatch: $(cat "$t/out")"
[ ! -e "$t/bin/chronos" ] || fail "a tampered release installs nothing"

# no checksum line for this build: fail closed
grep -v x86_64-unknown-linux-musl "$t/rel/SHA256SUMS" | grep -v aarch64-apple-darwin >"$t/sums" && mv "$t/sums" "$t/rel/SHA256SUMS"
rm -rf "$t/bin"
if run >"$t/out" 2>&1; then fail "a release with no checksum line must fail"; fi
grep -q "has no entry" "$t/out" || fail "the error says no entry: $(cat "$t/out")"
[ ! -e "$t/bin/chronos" ] || fail "a release with no checksum installs nothing"

# an OS with no build: a fake uname first on PATH
mkdir -p "$t/fake"
printf '#!/bin/sh\ncase "$1" in -s) echo FreeBSD ;; -m) echo amd64 ;; esac\n' >"$t/fake/uname"
chmod +x "$t/fake/uname"
if (PATH="$t/fake:$PATH" run) >"$t/out" 2>&1; then fail "an unsupported OS must fail"; fi
grep -q "/issues" "$t/out" || fail "the error points to the issues: $(cat "$t/out")"

# a copy published with a release installs that release, unless CHRONOS_VERSION says otherwise:
# the release workflow's substitution, and a fake curl that fails, naming the URL it was given
printf '#!/bin/sh\nexit 22\n' >"$t/fake/curl"
chmod +x "$t/fake/curl"
rm "$t/fake/uname"
fetches() { env PATH="$t/fake:$PATH" CHRONOS_INSTALL_DIR="$t/bin" "$@" >"$t/out" 2>&1 || true; }
fetches sh "$t/tagged.sh"
grep -q "releases/download/v9.9.9/" "$t/out" || fail "a tagged install.sh installs its own tag: $(cat "$t/out")"
fetches CHRONOS_VERSION=v1.0.0 sh "$t/tagged.sh"
grep -q "releases/download/v1.0.0/" "$t/out" || fail "CHRONOS_VERSION overrides the tag: $(cat "$t/out")"
fetches sh "$root/install.sh"
grep -q "releases/latest/download/" "$t/out" || fail "the repo's install.sh installs latest: $(cat "$t/out")"

# a release on disk whose stand-in `chronos` exits with $1
release() {
    rm -rf "$t/rel2" "$t/pkg2"
    mkdir -p "$t/rel2" "$t/pkg2"
    printf '#!/bin/sh\necho chronos-stand-in\nexit %s\n' "$1" >"$t/pkg2/chronos"
    for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
        tar -czf "$t/rel2/chronos-$target.tar.gz" -C "$t/pkg2" chronos
    done
    (cd "$t/rel2" && for f in *.tar.gz; do
        if command -v sha256sum >/dev/null; then sha256sum "$f"; else shasum -a 256 "$f"; fi
    done >SHA256SUMS)
}
# a build that doesn't run here (another CPU's, or one this CPU lacks instructions for) installs nothing
release 132
rm -rf "$t/bin"
if CHRONOS_BASE_URL="file://$t/rel2" CHRONOS_INSTALL_DIR="$t/bin" sh "$root/install.sh" >"$t/out" 2>&1; then
    fail "a build that doesn't run must fail"
fi
grep -q "doesn't run on this machine" "$t/out" || fail "the error says it doesn't run: $(cat "$t/out")"
[ ! -e "$t/bin/chronos" ] || fail "a build that doesn't run installs nothing"
# a folder the user can't write is refused, with what to do (install.sh never runs anything as root)
if [ "$(id -u)" != 0 ]; then
    release 0
    mkdir -p "$t/ro"
    chmod 0555 "$t/ro"
    if CHRONOS_BASE_URL="file://$t/rel2" CHRONOS_INSTALL_DIR="$t/ro/bin" sh "$root/install.sh" >"$t/out" 2>&1; then
        fail "an unwritable folder must fail"
    fi
    grep -q "can't write to $t/ro/bin" "$t/out" || fail "the error names the folder: $(cat "$t/out")"
    chmod 0755 "$t/ro"
fi

echo "install.sh: ok"
