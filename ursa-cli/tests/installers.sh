#!/bin/sh
# Tests the one-line installers, docs/install.sh and docs/install.ps1, against
# a local copy of a release: no network, nothing published.
#
#   sh ursa-cli/tests/installers.sh target/release/ursa
#
# Packages the given `ursa` binary the way .github/workflows/cli-build.yml
# does (ursa-cli-v<version>-<target>.tar.gz plus a .sha256 file), serves it
# with `python3 -m http.server` in the layout of GitHub's release downloads,
# and runs the installer against it with URSA_DOWNLOAD_BASE into temporary
# directories, on the stable channel and the nightly one (the same binary, in
# nightly/ under the nightly archive names). Needs python3, tar, and sha256sum
# or shasum. Runs shellcheck on the scripts when it is installed.
#
# When `pwsh` is installed (it is on GitHub's Ubuntu runners), also runs
# install.ps1 as `irm <url> | iex`. On Linux that installs the Linux binary,
# packaged as the Windows .zip with the name ursa.exe: it checks the script's
# download, checksum and install steps, not Windows itself.
#
# Exits non-zero on the first failure.

set -eu

[ $# -eq 1 ] || {
    echo "usage: $0 path/to/ursa" >&2
    exit 2
}

here=$(cd "$(dirname "$0")" && pwd)
installer="$here/../../docs/install.sh"
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
[ -x "$bin" ] || {
    echo "not an executable: $1" >&2
    exit 2
}

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

pass() {
    printf 'ok: %s\n' "$*"
}

full=$("$bin" --version)
version=$(printf '%s\n' "$full" | sed 's/^ursa //; s/ .*//')
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Darwin-arm64) target=aarch64-apple-darwin ;;
    *) fail "no release target for $(uname -s) $(uname -m)" ;;
esac

work=$(mktemp -d)
server_pid=
cleanup() {
    [ -z "$server_pid" ] || kill "$server_pid" 2> /dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

sha256() {
    if command -v sha256sum > /dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

# A release, packaged as cli-build.yml's Package step does.
name="ursa-cli-v$version-$target"
release="$work/www/good/ursa-cli-v$version"
mkdir -p "$release" "$work/pkg/$name"
cp "$bin" "$here/../README.md" "$here/../../LICENSE-MIT" "$here/../../LICENSE-APACHE" "$work/pkg/$name/"
(cd "$work/pkg" && tar czf "$release/$name.tar.gz" "$name")
(cd "$release" && sha256 "$name.tar.gz" > "$name.tar.gz.sha256")

# The rolling nightly release: the same binary, in archives of fixed names
# under nightly/, as nightly-cli.yml publishes them.
nname="ursa-cli-nightly-$target"
nightly="$work/www/good/nightly"
mkdir -p "$nightly" "$work/npkg/$nname"
cp "$work/pkg/$name/"* "$work/npkg/$nname/"
(cd "$work/npkg" && tar czf "$nightly/$nname.tar.gz" "$nname")
(cd "$nightly" && sha256 "$nname.tar.gz" > "$nname.tar.gz.sha256")

# The same archive with a checksum that does not match it.
bad="$work/www/bad/ursa-cli-v$version"
mkdir -p "$bad"
cp "$release/$name.tar.gz" "$bad/"
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$name.tar.gz" > "$bad/$name.tar.gz.sha256"

# A releases listing, as GitHub's API returns it: the Python package's release
# first, then an ursa-cli draft and prerelease, which must be skipped. Compact
# JSON, with a release body that mentions the keys the installer looks for.
cat > "$work/www/releases.json" << EOF
[{"tag_name":"v9.0.0","name":"Python","draft":false,"prerelease":false,"body":"not ursa-cli"},
{"tag_name":"ursa-cli-v9.9.9","draft":true,"prerelease":false,"body":"a draft"},
{"tag_name":"ursa-cli-v9.9.8","draft":false,"prerelease":true,"body":"\\"tag_name\\": \\"ursa-cli-v1.0.0\\", \\"draft\\": false"},
{"tag_name":"ursa-cli-v$version","author":{"login":"x","id":1},"draft":false,"immutable":false,"prerelease":false,"body":"the one"},
{"tag_name":"ursa-cli-v0.0.1","draft":false,"prerelease":false}]
EOF
# The same, pretty-printed, as GitHub serves it to curl.
python3 -c 'import json,sys; json.dump(json.load(open(sys.argv[1])), open(sys.argv[2], "w"), indent=2)' \
    "$work/www/releases.json" "$work/www/releases-pretty.json"
printf '[]\n' > "$work/www/no-releases.json"
cp "$installer" "$here/../../docs/install.ps1" "$work/www/"

python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$work/www" > "$work/server.log" 2>&1 &
server_pid=$!
port=
i=0
while [ -z "$port" ] && [ $i -lt 50 ]; do
    port=$(sed -n 's/.* port \([0-9][0-9]*\).*/\1/p' "$work/server.log" | head -n 1)
    [ -n "$port" ] || sleep 0.1
    i=$((i + 1))
done
[ -n "$port" ] || fail "the HTTP server did not start: $(cat "$work/server.log")"
www="http://127.0.0.1:$port"

# A PATH without the install directories, so the installer prints its PATH hint.
base_path="/usr/local/bin:/usr/bin:/bin"

# install <dir> <shell> [VAR=value...]: run the installer with `<shell> -s`,
# reading it from stdin as `curl ... | sh` does. Output goes to $work/out.
install() {
    dir=$1
    shell=$2
    shift 2
    mkdir -p "$work/tmp"
    env -i PATH="$base_path" HOME="$work/home" SHELL=/bin/bash TMPDIR="$work/tmp" URSA_INSTALL_DIR="$dir" "$@" \
        "$shell" -s < "$installer" > "$work/out" 2>&1
}

shells="sh"
for s in dash bash; do
    command -v "$s" > /dev/null && shells="$shells $s"
done

for shell in $shells; do
    dir="$work/$shell/bin"
    install "$dir" "$shell" URSA_DOWNLOAD_BASE="$www/good" URSA_VERSION="$version" ||
        fail "$shell: install with URSA_VERSION=$version failed: $(cat "$work/out")"
    [ "$("$dir/ursa" --version)" = "$full" ] || fail "$shell: installed ursa reports $("$dir/ursa" --version)"
    [ "$("$dir/ursa" --format csv -c "SELECT 42 AS answer" | tail -n 1)" = 42 ] || fail "$shell: installed ursa cannot run a query"
    grep -q "Checksum OK" "$work/out" || fail "$shell: no checksum line: $(cat "$work/out")"
    grep -qF "export PATH=\"$dir:\$PATH\"" "$work/out" || fail "$shell: no PATH hint: $(cat "$work/out")"
    grep -qF 'ursa -c "SELECT 42"' "$work/out" || fail "$shell: no next step: $(cat "$work/out")"
    grep -qF "Docs: https://cldixon.github.io/ursa/sql/" "$work/out" || fail "$shell: no docs link: $(cat "$work/out")"
    pass "$shell: installs ursa $version into a new directory, with a PATH hint"
done
cat "$work/out"

# Again into the same directory, with a leading v: replaces the binary in place,
# and with the directory on PATH, prints no PATH hint.
dir="$work/sh/bin"
before=$(ls -i "$dir/ursa")
env -i PATH="$dir:$base_path" HOME="$work/home" URSA_INSTALL_DIR="$dir" \
    URSA_DOWNLOAD_BASE="$www/good" URSA_VERSION="v$version" sh -s < "$installer" > "$work/out" 2>&1 ||
    fail "reinstall failed: $(cat "$work/out")"
[ "$("$dir/ursa" --version)" = "$full" ] || fail "reinstalled ursa is broken"
[ "$(ls -i "$dir/ursa")" != "$before" ] || fail "reinstall did not replace the file"
! grep -q "not on your PATH" "$work/out" || fail "PATH hint printed with the directory on PATH"
grep -qF "Docs: https://cldixon.github.io/ursa/sql/" "$work/out" || fail "no docs link with the directory on PATH: $(cat "$work/out")"
[ "$(ls -A "$dir")" = ursa ] || fail "reinstall left files behind: $(ls -A "$dir")"
pass "reinstalls over an existing ursa, atomically, with no PATH hint when on PATH"

# The newest version, from a releases listing, compact and pretty-printed.
for listing in releases.json releases-pretty.json; do
    dir="$work/latest-$listing/bin"
    install "$dir" sh URSA_DOWNLOAD_BASE="$www/good" URSA_RELEASES_URL="$www/$listing" ||
        fail "install of the newest version ($listing) failed: $(cat "$work/out")"
    [ "$("$dir/ursa" --version)" = "$full" ] || fail "newest version ($listing): wrong binary"
    grep -qF "Downloading ursa $version for $target" "$work/out" || fail "newest version ($listing) is not $version: $(cat "$work/out")"
done
pass "finds the newest ursa-cli release, skipping Python releases, drafts and prereleases"

# No ursa-cli release at all: a clear error.
dir="$work/none/bin"
if install "$dir" sh URSA_DOWNLOAD_BASE="$www/good" URSA_RELEASES_URL="$www/no-releases.json"; then
    fail "installed with no ursa-cli release"
fi
grep -q "found no ursa-cli release" "$work/out" || fail "no-release error is unclear: $(cat "$work/out")"
pass "fails clearly when there is no ursa-cli release"

# A checksum that does not match: refuses, and installs nothing.
dir="$work/bad/bin"
if install "$dir" sh URSA_DOWNLOAD_BASE="$www/bad" URSA_VERSION="$version"; then
    fail "installed despite a bad checksum: $(cat "$work/out")"
fi
grep -q "checksum mismatch" "$work/out" || fail "bad checksum error is unclear: $(cat "$work/out")"
[ ! -e "$dir/ursa" ] || fail "a binary was installed despite the bad checksum"
pass "refuses an archive whose checksum does not match"

# A version that does not exist.
dir="$work/missing/bin"
if install "$dir" sh URSA_DOWNLOAD_BASE="$www/good" URSA_VERSION=0.0.0; then
    fail "installed a version that does not exist"
fi
grep -q "could not download" "$work/out" || fail "missing version error is unclear: $(cat "$work/out")"
[ ! -e "$dir/ursa" ] || fail "a binary was installed for a missing version"
pass "fails clearly for a version that does not exist"

# An unsupported platform, via a fake uname.
mkdir -p "$work/fake"
cat > "$work/fake/uname" << 'EOF'
#!/bin/sh
case "$1" in -s) echo FreeBSD ;; -m) echo amd64 ;; esac
EOF
chmod +x "$work/fake/uname"
if env -i PATH="$work/fake:$base_path" HOME="$work/home" URSA_INSTALL_DIR="$work/fbsd" \
    URSA_DOWNLOAD_BASE="$www/good" URSA_VERSION="$version" sh -s < "$installer" > "$work/out" 2>&1; then
    fail "installed on an unsupported platform"
fi
grep -q "cargo install ursa-cli" "$work/out" || fail "unsupported platform error does not suggest cargo: $(cat "$work/out")"
pass "fails on an unsupported platform, suggesting cargo install"

# Every run above, failed ones included, removed its temporary directory.
[ -z "$(ls -A "$work/tmp")" ] || fail "temporary files left behind: $(ls -A "$work/tmp")"
pass "cleans up its temporary directory"

# The documented command, `curl -fsSL <url>/install.sh | sh`, with the script
# served over HTTP.
if command -v curl > /dev/null; then
    dir="$work/curl/bin"
    env -i PATH="$base_path" HOME="$work/home" URSA_INSTALL_DIR="$dir" \
        URSA_DOWNLOAD_BASE="$www/good" URSA_VERSION="$version" \
        sh -c "curl -fsSL '$www/install.sh' | sh" > "$work/out" 2>&1 ||
        fail "curl | sh failed: $(cat "$work/out")"
    [ "$("$dir/ursa" --version)" = "$full" ] || fail "curl | sh: wrong binary"
    pass "curl -fsSL .../install.sh | sh"
else
    echo "skipped: curl | sh (curl not installed)"
fi

# The nightly channel, chosen with URSA_CHANNEL, with `sh -s -- --nightly`, and
# undone with --stable. No releases listing is needed: it is never read.
dir="$work/nightly/bin"
install "$dir" sh URSA_DOWNLOAD_BASE="$www/good" URSA_RELEASES_URL="$www/no-releases.json" URSA_CHANNEL=nightly ||
    fail "URSA_CHANNEL=nightly install failed: $(cat "$work/out")"
[ "$("$dir/ursa" --version)" = "$full" ] || fail "nightly: wrong binary"
grep -qF "Downloading the newest ursa nightly for $target" "$work/out" || fail "nightly: no download line: $(cat "$work/out")"
grep -qF "/nightly/$nname.tar.gz" "$work/out" || fail "nightly: not from the nightly release: $(cat "$work/out")"
grep -q "Checksum OK" "$work/out" || fail "nightly: no checksum line: $(cat "$work/out")"
grep -q "This is a nightly build" "$work/out" || fail "nightly: no nightly notice: $(cat "$work/out")"
cat "$work/out"
pass "URSA_CHANNEL=nightly installs the nightly"

dir="$work/nightly-arg/bin"
env -i PATH="$base_path" HOME="$work/home" TMPDIR="$work/tmp" URSA_INSTALL_DIR="$dir" \
    URSA_DOWNLOAD_BASE="$www/good" sh -s -- --nightly < "$installer" > "$work/out" 2>&1 ||
    fail "sh -s -- --nightly failed: $(cat "$work/out")"
grep -qF "/nightly/$nname.tar.gz" "$work/out" || fail "--nightly: not from the nightly release: $(cat "$work/out")"
env -i PATH="$base_path" HOME="$work/home" TMPDIR="$work/tmp" URSA_INSTALL_DIR="$dir" URSA_CHANNEL=nightly \
    URSA_DOWNLOAD_BASE="$www/good" URSA_VERSION="$version" sh -s -- --stable < "$installer" > "$work/out" 2>&1 ||
    fail "sh -s -- --stable failed: $(cat "$work/out")"
grep -qF "/ursa-cli-v$version/$name.tar.gz" "$work/out" || fail "--stable did not override URSA_CHANNEL: $(cat "$work/out")"
! grep -q "This is a nightly build" "$work/out" || fail "--stable printed the nightly notice"
pass "sh -s -- --nightly installs the nightly, and --stable goes back"

# Nightly errors: a version with the nightly, an unknown channel or option, and
# no nightly published.
dir="$work/nightly-err/bin"
if install "$dir" sh URSA_DOWNLOAD_BASE="$www/good" URSA_CHANNEL=nightly URSA_VERSION="$version"; then
    fail "installed with both URSA_VERSION and the nightly channel"
fi
grep -q "URSA_VERSION picks a stable release" "$work/out" || fail "version-with-nightly error is unclear: $(cat "$work/out")"
if install "$dir" sh URSA_DOWNLOAD_BASE="$www/good" URSA_CHANNEL=beta; then
    fail "installed from an unknown channel"
fi
grep -q "it is stable or nightly" "$work/out" || fail "unknown channel error is unclear: $(cat "$work/out")"
if env -i PATH="$base_path" HOME="$work/home" TMPDIR="$work/tmp" URSA_INSTALL_DIR="$dir" \
    URSA_DOWNLOAD_BASE="$www/good" sh -s -- --beta < "$installer" > "$work/out" 2>&1; then
    fail "installed with an unknown option"
fi
grep -q "unknown option '--beta'" "$work/out" || fail "unknown option error is unclear: $(cat "$work/out")"
if install "$dir" sh URSA_DOWNLOAD_BASE="$www/bad" URSA_CHANNEL=nightly; then
    fail "installed a nightly that does not exist"
fi
grep -q "Check that there is a nightly" "$work/out" || fail "missing nightly error is unclear: $(cat "$work/out")"
[ ! -e "$dir/ursa" ] || fail "a binary was installed by a failed nightly install"
[ -z "$(ls -A "$work/tmp")" ] || fail "temporary files left behind: $(ls -A "$work/tmp")"
pass "nightly errors: a version, an unknown channel or option, no nightly"

if command -v shellcheck > /dev/null; then
    shellcheck -s sh "$installer" "$0"
    pass "shellcheck"
else
    echo "skipped: shellcheck (not installed)"
fi

# install.ps1, with PowerShell 7 on this machine. The .zip is laid out as
# cli-build.yml's 7z command lays it out, with the executable bit kept so
# the script's `ursa.exe --version` can run here.
if command -v pwsh > /dev/null; then
    winname="ursa-cli-v$version-x86_64-pc-windows-msvc"
    python3 -I - "$release/$winname.zip" "$winname" "$bin" "$here/../README.md" \
        "$here/../../LICENSE-MIT" "$here/../../LICENSE-APACHE" << 'EOF'
import sys, zipfile
out, name, exe, *docs = sys.argv[1:]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    info = zipfile.ZipInfo(f"{name}/ursa.exe")
    info.external_attr = 0o100755 << 16
    info.compress_type = zipfile.ZIP_DEFLATED
    z.writestr(info, open(exe, "rb").read())
    for doc in docs:
        z.write(doc, f"{name}/{doc.rsplit('/', 1)[-1]}")
EOF
    (cd "$release" && sha256 "$winname.zip" > "$winname.zip.sha256")
    # The nightly .zip: the same files under the nightly name.
    nwinname="ursa-cli-nightly-x86_64-pc-windows-msvc"
    python3 -I - "$release/$winname.zip" "$winname" "$nightly/$nwinname.zip" "$nwinname" << 'EOF'
import sys, zipfile
src, old, out, new = sys.argv[1:]
with zipfile.ZipFile(src) as a, zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as b:
    for info in a.infolist():
        data = a.read(info)
        info.filename = new + info.filename[len(old):]
        b.writestr(info, data)
EOF
    (cd "$nightly" && sha256 "$nwinname.zip" > "$nwinname.zip.sha256")
    cp "$release/$winname.zip" "$bad/"
    printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$winname.zip" > "$bad/$winname.zip.sha256"

    # ps1 <dir> [VAR=value...]: run `irm <url>/install.ps1 | iex`.
    ps1() {
        dir=$1
        shift
        env -i PATH="$base_path" HOME="$work/home" TMPDIR="$work/tmp" PROCESSOR_ARCHITECTURE=AMD64 \
            POWERSHELL_TELEMETRY_OPTOUT=1 POWERSHELL_UPDATECHECK=Off URSA_INSTALL_DIR="$dir" "$@" \
            pwsh -NoProfile -NonInteractive -Command "irm '$www/install.ps1' | iex" > "$work/out" 2>&1
    }

    dir="$work/ps1/bin"
    ps1 "$dir" URSA_DOWNLOAD_BASE="$www/good" URSA_VERSION="$version" ||
        fail "install.ps1 failed: $(cat "$work/out")"
    [ "$("$dir/ursa.exe" --version)" = "$full" ] || fail "install.ps1: wrong binary"
    grep -q "Checksum OK" "$work/out" || fail "install.ps1: no checksum line: $(cat "$work/out")"
    grep -qF "Installed ursa $version" "$work/out" || fail "install.ps1: no version line: $(cat "$work/out")"
    grep -qF "Docs: https://cldixon.github.io/ursa/sql/" "$work/out" || fail "install.ps1: no docs link: $(cat "$work/out")"
    cat "$work/out"
    pass "install.ps1 installs ursa $version"

    dir="$work/ps1-latest/bin"
    ps1 "$dir" URSA_DOWNLOAD_BASE="$www/good" URSA_RELEASES_URL="$www/releases-pretty.json" ||
        fail "install.ps1 (newest version) failed: $(cat "$work/out")"
    grep -qF "Downloading ursa $version for" "$work/out" || fail "install.ps1: newest version is not $version: $(cat "$work/out")"
    pass "install.ps1 finds the newest ursa-cli release"

    dir="$work/ps1-bad/bin"
    if ps1 "$dir" URSA_DOWNLOAD_BASE="$www/bad" URSA_VERSION="$version"; then
        fail "install.ps1 installed despite a bad checksum: $(cat "$work/out")"
    fi
    grep -q "checksum mismatch" "$work/out" || fail "install.ps1: bad checksum error is unclear: $(cat "$work/out")"
    [ ! -e "$dir/ursa.exe" ] || fail "install.ps1 installed a binary despite the bad checksum"
    pass "install.ps1 refuses an archive whose checksum does not match"

    dir="$work/ps1-nightly/bin"
    ps1 "$dir" URSA_DOWNLOAD_BASE="$www/good" URSA_RELEASES_URL="$www/no-releases.json" URSA_CHANNEL=nightly ||
        fail "install.ps1 (nightly) failed: $(cat "$work/out")"
    [ "$("$dir/ursa.exe" --version)" = "$full" ] || fail "install.ps1 (nightly): wrong binary"
    grep -qF "/nightly/$nwinname.zip" "$work/out" || fail "install.ps1: nightly not from the nightly release: $(cat "$work/out")"
    grep -q "This is a nightly build" "$work/out" || fail "install.ps1: no nightly notice: $(cat "$work/out")"
    pass "install.ps1 installs the nightly with URSA_CHANNEL=nightly"

    dir="$work/ps1-nightly-err/bin"
    if ps1 "$dir" URSA_DOWNLOAD_BASE="$www/good" URSA_CHANNEL=nightly URSA_VERSION="$version"; then
        fail "install.ps1 installed with both URSA_VERSION and the nightly channel"
    fi
    grep -q "URSA_VERSION picks a stable release" "$work/out" || fail "install.ps1: version-with-nightly error is unclear: $(cat "$work/out")"
    if ps1 "$dir" URSA_DOWNLOAD_BASE="$www/good" URSA_CHANNEL=beta; then
        fail "install.ps1 installed from an unknown channel"
    fi
    grep -q "it is stable or nightly" "$work/out" || fail "install.ps1: unknown channel error is unclear: $(cat "$work/out")"
    pass "install.ps1 nightly errors: a version, an unknown channel"
else
    echo "skipped: install.ps1 (pwsh not installed)"
fi

echo "all installer tests passed"
