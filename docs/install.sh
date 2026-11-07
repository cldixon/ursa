#!/bin/sh
# Install the `ursa` command-line tool (Ursa's SQL engine for graph analytics).
#
#   curl -fsSL https://cldixon.github.io/ursa/install.sh | sh
#
# Downloads the prebuilt binary for this machine from the repository's GitHub
# Releases, checks it against its published SHA-256 checksum, and puts it in
# ~/.local/bin. No sudo, and nothing outside the install directory changes.
#
# Two channels:
#
#   stable   (the default) the newest `ursa-cli-v*` release, which
#            .github/workflows/release-cli.yml makes and crates.io also has.
#   nightly  the newest build of main: the rolling `nightly` prerelease, which
#            .github/workflows/nightly-cli.yml replaces on every merge. New
#            features first, and they may change or break.
#
#   curl -fsSL https://cldixon.github.io/ursa/install.sh | URSA_CHANNEL=nightly sh
#   curl -fsSL https://cldixon.github.io/ursa/install.sh | sh -s -- --nightly
#
# Running it again without the option goes back to stable.
#
# Options: --nightly, --stable (the channel; they override URSA_CHANNEL),
# --help.
#
# Settings, as environment variables (put them after the pipe, e.g.
# `curl -fsSL .../install.sh | URSA_VERSION=0.6.0 sh`):
#
#   URSA_CHANNEL        stable or nightly. Default: stable.
#   URSA_VERSION        the stable version to install, e.g. 0.6.0. Default: the
#                       newest ursa-cli release. Not with the nightly channel.
#   URSA_INSTALL_DIR    where to put the binary. Default: $HOME/.local/bin.
#   URSA_DOWNLOAD_BASE  the URL that holds ursa-cli-v<version>/<archive> and
#                       nightly/<archive>.
#                       Default: https://github.com/cldixon/ursa/releases/download
#                       (for testing against a local copy).
#   URSA_RELEASES_URL   the GitHub API URL that lists the releases, read to find
#                       the newest version (for testing).
#
# Supported: Linux (x86_64, aarch64; glibc 2.35 or later) and macOS (Intel,
# Apple silicon). Anywhere else, `cargo install ursa-cli` builds it from source.
#
# The whole script is in functions and runs from the last line, so a download
# cut short runs nothing.

set -eu

REPO="cldixon/ursa"
DOCS_URL="https://cldixon.github.io/ursa/sql/"

say() {
    printf '%s\n' "$*"
}

err() {
    printf 'ursa install: error: %s\n' "$*" >&2
    exit 1
}

has() {
    command -v "$1" > /dev/null 2>&1
}

# Download $1 to the file $2. Returns non-zero on any failure, HTTP errors included.
download() {
    if has curl; then
        curl --proto '=https,http' --fail --silent --show-error --location --retry 3 --output "$2" "$1"
    elif has wget; then
        wget --quiet --tries=3 --output-document="$2" "$1"
    else
        err "need curl or wget to download ursa; install one of them and run this again"
    fi
}

# The Rust target triple of this machine, as the release archives are named.
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) unsupported "$os $arch" ;;
    esac
    case "$os" in
        Linux)
            # The Linux builds link glibc; Alpine and other musl systems cannot run them.
            if ls /lib/ld-musl-* > /dev/null 2>&1; then
                unsupported "Linux with musl libc"
            fi
            target="$arch-unknown-linux-gnu"
            ;;
        Darwin)
            # An x86_64 shell on Apple silicon runs under Rosetta: install the native build.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2> /dev/null || true)" = 1 ]; then
                arch=aarch64
            fi
            target="$arch-apple-darwin"
            ;;
        MINGW* | MSYS* | CYGWIN* | Windows_NT)
            err "this is Windows; in PowerShell, run: irm https://cldixon.github.io/ursa/install.ps1 | iex"
            ;;
        *) unsupported "$os $arch" ;;
    esac
    printf '%s\n' "$target"
}

usage() {
    say "Install the ursa command-line tool."
    say ""
    say "  curl -fsSL https://cldixon.github.io/ursa/install.sh | sh"
    say "  curl -fsSL https://cldixon.github.io/ursa/install.sh | sh -s -- --nightly"
    say ""
    say "Options:"
    say "  --stable   the newest stable release (the default)"
    say "  --nightly  the newest nightly build of main"
    say "  --help     show this"
    say ""
    say "Settings, as environment variables: URSA_CHANNEL (stable or nightly),"
    say "URSA_VERSION (a stable version, e.g. 0.6.0) and URSA_INSTALL_DIR (where"
    say "to put ursa; the default is \$HOME/.local/bin). The comments at the top"
    say "of the script describe the rest."
}

unsupported() {
    err "there is no prebuilt ursa binary for $1. Build it from source with Rust instead: cargo install ursa-cli"
}

# The newest published, non-prerelease ursa-cli release, e.g. 0.6.0. The
# repository's releases also hold the Python package (tags v*), so GitHub's
# "latest release" is not ursa-cli's; scan the list for the first ursa-cli-v* tag.
latest_version() {
    url="${URSA_RELEASES_URL:-https://api.github.com/repos/$REPO/releases?per_page=100}"
    json="$tmp/releases.json"
    download "$url" "$json" || err "could not list ursa's releases from $url (GitHub limits unauthenticated requests per hour). Set URSA_VERSION, e.g. URSA_VERSION=0.6.0, to skip this lookup"
    # One JSON field per line, then walk the releases in order (newest first).
    # Within a release, tag_name comes before draft and prerelease. Quotes
    # inside string values are escaped, so text in a release's notes never
    # matches these keys.
    # shellcheck disable=SC2020 # three characters, each to a newline
    version=$(tr ',{}' '\n\n\n' < "$json" | sed -n \
        -e 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*/tag \1/p' \
        -e 's/^[[:space:]]*"draft":[[:space:]]*\([a-z]*\).*/draft \1/p' \
        -e 's/^[[:space:]]*"prerelease":[[:space:]]*\([a-z]*\).*/prerelease \1/p' |
        awk '
            $1 == "tag" { tag = $2; draft = ""; next }
            $1 == "draft" { draft = $2; next }
            $1 == "prerelease" {
                if (draft == "false" && $2 == "false" && index(tag, "ursa-cli-v") == 1) {
                    print substr(tag, 11)
                    exit
                }
            }
        ')
    [ -n "$version" ] || err "found no ursa-cli release at $url. The first one is 0.6.0; until it is out, build from source: cargo install --git https://github.com/$REPO ursa-cli"
    printf '%s\n' "$version"
}

sha256_of() {
    if has sha256sum; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif has shasum; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    elif has openssl; then
        openssl dgst -sha256 -r "$1" | cut -d ' ' -f 1
    else
        err "need sha256sum, shasum or openssl to check the download; install one of them and run this again"
    fi
}

main() {
    has uname || err "need uname"
    has tar || err "need tar"
    has mktemp || err "need mktemp"

    tmp=$(mktemp -d 2> /dev/null || mktemp -d -t ursa-install)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    channel=${URSA_CHANNEL:-stable}
    for arg in "$@"; do
        case "$arg" in
            --nightly) channel=nightly ;;
            --stable) channel=stable ;;
            -h | --help)
                usage
                exit 0
                ;;
            *) err "unknown option '$arg'. Options: --stable, --nightly, --help" ;;
        esac
    done
    case "$channel" in
        stable | nightly) ;;
        *) err "URSA_CHANNEL is '$channel'; it is stable or nightly" ;;
    esac

    target=$(detect_target)

    base="${URSA_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download}"
    base=${base%/}
    if [ "$channel" = nightly ]; then
        [ -z "${URSA_VERSION:-}" ] || err "URSA_VERSION picks a stable release, and there is only one nightly, the newest. Leave out URSA_VERSION for the nightly, or the nightly option for version ${URSA_VERSION}"
        # One rolling release, with archives of fixed names: no lookup needed.
        name="ursa-cli-nightly-$target"
        archive="$name.tar.gz"
        url="$base/nightly/$archive"
        say "Downloading the newest ursa nightly for $target"
        missing="Check that there is a nightly with a $target build: https://github.com/$REPO/releases/tag/nightly"
    else
        if [ -n "${URSA_VERSION:-}" ]; then
            version=${URSA_VERSION#v}
            version=${version#ursa-cli-v}
        else
            say "Finding the newest ursa release..."
            version=$(latest_version)
        fi
        name="ursa-cli-v$version-$target"
        archive="$name.tar.gz"
        url="$base/ursa-cli-v$version/$archive"
        say "Downloading ursa $version for $target"
        missing="Check that ursa $version exists and has a $target build: https://github.com/$REPO/releases"
    fi
    install_dir="${URSA_INSTALL_DIR:-${HOME:?HOME is not set; set URSA_INSTALL_DIR}/.local/bin}"

    say "  from $url"
    download "$url" "$tmp/$archive" || err "could not download $url. $missing"
    download "$url.sha256" "$tmp/$archive.sha256" || err "could not download the checksum $url.sha256"

    expected=$(cut -d ' ' -f 1 < "$tmp/$archive.sha256" | tr 'A-F' 'a-f')
    actual=$(sha256_of "$tmp/$archive")
    if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
        err "checksum mismatch for $archive: expected '$expected', got '$actual'. The download is damaged or was tampered with; nothing was installed"
    fi
    say "Checksum OK (sha256 $actual)"

    (cd "$tmp" && tar xzf "$archive") || err "could not unpack $archive"
    [ -f "$tmp/$name/ursa" ] || err "$archive holds no $name/ursa"

    mkdir -p "$install_dir" || err "could not create $install_dir; set URSA_INSTALL_DIR to a directory you can write to"
    # Copy next to the target, then rename: the rename is atomic, so a running
    # `ursa` is never half-replaced.
    staged="$install_dir/.ursa.$$.tmp"
    cp "$tmp/$name/ursa" "$staged" || err "could not write to $install_dir; set URSA_INSTALL_DIR to a directory you can write to"
    chmod 755 "$staged"
    mv -f "$staged" "$install_dir/ursa" || {
        rm -f "$staged"
        err "could not move ursa into $install_dir"
    }

    say ""
    say "Installed $("$install_dir/ursa" --version) to $install_dir/ursa"
    if [ "$channel" = nightly ]; then
        say ""
        say "This is a nightly build of main: new features first, and they may change"
        say "or break. When you report a bug, include the output of: ursa --version"
        say "To go back to stable, run the installer again without the nightly option."
    fi

    case ":${PATH:-}:" in
        *":$install_dir:"*)
            say ""
            say "Try it:"
            say "  ursa -c \"SELECT 42\""
            say ""
            say "Docs: $DOCS_URL"
            ;;
        *)
            case "${SHELL:-}" in
                */zsh) rc="$HOME/.zshrc" ;;
                */bash) rc="$HOME/.bashrc" ;;
                */fish) rc="" ;;
                *) rc="$HOME/.profile" ;;
            esac
            say ""
            say "$install_dir is not on your PATH."
            if [ -z "$rc" ]; then
                say "Add it, in fish:"
                say "  fish_add_path \"$install_dir\""
            else
                say "Add it by putting this line in $rc, then open a new terminal:"
                say "  export PATH=\"$install_dir:\$PATH\""
            fi
            say ""
            say "Then try it:"
            say "  ursa -c \"SELECT 42\""
            say ""
            say "Docs: $DOCS_URL"
            ;;
    esac
}

main "$@"
