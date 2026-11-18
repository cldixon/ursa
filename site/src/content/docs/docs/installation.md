---
title: Installation
description: Install the ursa command-line tool on Linux, macOS or Windows, from a stable release or a nightly build.
---

:::note[Versions]
The newest release is `ursa-cli` 0.10.0. Features marked **New in 0.7** or **New in 0.8** need that version or later (`ursa --version` says which you have). The SQL surface may still change between minor versions, and the [changelog](https://github.com/cldixon/ursa/blob/main/CHANGELOG.md) records every change.
:::

Ursa is one binary, `ursa`, with no server and no dependencies. To try it without installing anything, use [Ursa Shell](https://ursa-shell.cl-dixon.workers.dev), which runs the same engine in your browser.

## Install script

On Linux or macOS, run the install script:

```bash
curl -fsSL https://cldixon.github.io/ursa/install.sh | sh
```

It downloads the newest `ursa` for your machine from the [releases page](https://github.com/cldixon/ursa/releases), checks it against its SHA-256 checksum, and puts it in `~/.local/bin`. It needs no `sudo`. If `~/.local/bin` is not on your `PATH`, it prints the line to add. To choose the version or the directory, set `URSA_VERSION` or `URSA_INSTALL_DIR` after the pipe:

```bash
curl -fsSL https://cldixon.github.io/ursa/install.sh | URSA_VERSION=0.10.0 URSA_INSTALL_DIR=/opt/bin sh
```

On Windows, in PowerShell. This installs to `%LOCALAPPDATA%\ursa\bin` and adds it to your user `PATH`:

```powershell
irm https://cldixon.github.io/ursa/install.ps1 | iex
```

## Cargo

The prebuilt binaries cover Linux (x86_64 and aarch64, glibc 2.35 or later), macOS (Intel and Apple silicon) and Windows (x64). On anything else, install with Cargo. You need Rust 1.94 or later:

```bash
cargo install ursa-cli
```

## Prebuilt binaries

Or download a prebuilt binary yourself from the [releases page](https://github.com/cldixon/ursa/releases). Each release has one archive per platform:

| Platform              | Archive                                             |
| --------------------- | --------------------------------------------------- |
| Linux x86_64          | `ursa-cli-v0.10.0-x86_64-unknown-linux-gnu.tar.gz`  |
| Linux aarch64         | `ursa-cli-v0.10.0-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel)         | `ursa-cli-v0.10.0-x86_64-apple-darwin.tar.gz`       |
| macOS (Apple silicon) | `ursa-cli-v0.10.0-aarch64-apple-darwin.tar.gz`      |
| Windows x64           | `ursa-cli-v0.10.0-x86_64-pc-windows-msvc.zip`       |

Each archive has a `.sha256` file next to it. For example, on an Apple silicon Mac:

```bash
curl -LO https://github.com/cldixon/ursa/releases/download/ursa-cli-v0.10.0/ursa-cli-v0.10.0-aarch64-apple-darwin.tar.gz
tar xzf ursa-cli-v0.10.0-aarch64-apple-darwin.tar.gz
sudo mv ursa-cli-v0.10.0-aarch64-apple-darwin/ursa /usr/local/bin/
```

## Check it runs

The binary is called `ursa`:

```bash
ursa --version
```

```text
ursa 0.10.0
```

## Nightly builds

The installers have two channels. **Stable**, the default, is the newest release, the same version as on crates.io. **Nightly** is the newest build of `main`, rebuilt on every merge. It has features before they are released, including ones these docs mark "New in" a version that isn't out yet. Those features may still change or break.

```bash
curl -fsSL https://cldixon.github.io/ursa/install.sh | URSA_CHANNEL=nightly sh
```

or `curl -fsSL https://cldixon.github.io/ursa/install.sh | sh -s -- --nightly`. On Windows, in PowerShell:

```powershell
$env:URSA_CHANNEL = 'nightly'; irm https://cldixon.github.io/ursa/install.ps1 | iex
```

The download is checked against its SHA-256 checksum, as a stable one is. A nightly names its version, build date and commit:

```text
ursa 0.11.0-nightly.20261117 (a1b2c3d4e)
```

Only the newest nightly is kept, on the [`nightly` release](https://github.com/cldixon/ursa/releases/tag/nightly). To go back to stable, run the installer again without the nightly option (in PowerShell, `Remove-Item Env:URSA_CHANNEL` first). When you report a bug, include the output of `ursa --version`: it says which channel and commit you have. To build `main` yourself instead: `cargo install --git https://github.com/cldixon/ursa ursa-cli`.

## Next steps

The [Quickstart](/docs/quickstart/) defines a graph and runs algorithms on it.
