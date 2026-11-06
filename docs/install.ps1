# Install the `ursa` command-line tool (Ursa's SQL engine for graph analytics)
# on Windows.
#
#   irm https://cldixon.github.io/ursa/install.ps1 | iex
#
# Downloads the prebuilt x86_64 Windows binary from the repository's GitHub
# Releases (the `ursa-cli-v*` releases, which .github/workflows/release-cli.yml
# makes), checks it against its published SHA-256 checksum, puts it in
# %LOCALAPPDATA%\ursa\bin, and adds that directory to your user PATH. No
# administrator rights needed.
#
# Settings, as environment variables (set them first, e.g.
# `$env:URSA_VERSION = "0.6.0"`):
#
#   URSA_VERSION        the version to install, e.g. 0.6.0. Default: the newest
#                       ursa-cli release.
#   URSA_INSTALL_DIR    where to put ursa.exe. Default: %LOCALAPPDATA%\ursa\bin.
#   URSA_DOWNLOAD_BASE  the URL that holds ursa-cli-v<version>/<archive>.
#                       Default: https://github.com/cldixon/ursa/releases/download
#   URSA_RELEASES_URL   the GitHub API URL that lists the releases, read to find
#                       the newest version.
#
# Works in Windows PowerShell 5.1 and PowerShell 7. On Windows on Arm, the x64
# build runs under emulation.

# Everything is in a script block that runs from the last line, so a download
# cut short runs nothing, and `iex` leaves no variables behind.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is slow with a progress bar.
    $Repo = 'cldixon/ursa'

    function Fail([string] $Message) {
        Write-Host "ursa install: error: $Message" -ForegroundColor Red
        throw "ursa install failed"
    }

    # Windows PowerShell 5.1 may default to TLS 1.0, which GitHub refuses.
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch { }

    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne 'AMD64' -and $arch -ne 'ARM64') {
        Fail "there is no prebuilt ursa binary for Windows on $arch. Build it from source with Rust instead: cargo install ursa-cli"
    }
    $target = 'x86_64-pc-windows-msvc'

    if ($env:URSA_VERSION) {
        $version = $env:URSA_VERSION -replace '^ursa-cli-v', '' -replace '^v', ''
    } else {
        # The repository's releases also hold the Python package (tags v*), so
        # GitHub's "latest release" is not ursa-cli's: take the first published,
        # non-prerelease ursa-cli-v* release in the list (newest first).
        Write-Host 'Finding the newest ursa release...'
        $releasesUrl = if ($env:URSA_RELEASES_URL) { $env:URSA_RELEASES_URL } else { "https://api.github.com/repos/$Repo/releases?per_page=100" }
        try {
            $releases = Invoke-RestMethod -Uri $releasesUrl -UseBasicParsing
        } catch {
            Fail "could not list ursa's releases from $releasesUrl (GitHub limits unauthenticated requests per hour). Set `$env:URSA_VERSION, e.g. `$env:URSA_VERSION = '0.6.0', to skip this lookup. ($_)"
        }
        $release = @($releases) | Where-Object {
            -not $_.draft -and -not $_.prerelease -and $_.tag_name -like 'ursa-cli-v*'
        } | Select-Object -First 1
        if (-not $release) {
            Fail "found no ursa-cli release at $releasesUrl. The first one is 0.6.0; until it is out, build from source: cargo install --git https://github.com/$Repo ursa-cli"
        }
        $version = $release.tag_name.Substring('ursa-cli-v'.Length)
    }

    $base = if ($env:URSA_DOWNLOAD_BASE) { $env:URSA_DOWNLOAD_BASE.TrimEnd('/') } else { "https://github.com/$Repo/releases/download" }
    $name = "ursa-cli-v$version-$target"
    $archive = "$name.zip"
    $url = "$base/ursa-cli-v$version/$archive"
    $installDir = if ($env:URSA_INSTALL_DIR) { $env:URSA_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'ursa\bin' }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("ursa-install-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading ursa $version for $target"
        Write-Host "  from $url"
        $zip = Join-Path $tmp $archive
        try {
            Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        } catch {
            Fail "could not download $url. Check that ursa $version exists and has a $target build: https://github.com/$Repo/releases ($_)"
        }
        try {
            Invoke-WebRequest -Uri "$url.sha256" -OutFile "$zip.sha256" -UseBasicParsing
        } catch {
            Fail "could not download the checksum $url.sha256 ($_)"
        }

        $expected = ((Get-Content -Raw "$zip.sha256").Trim() -split '\s+')[0].ToLowerInvariant()
        $actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLowerInvariant()
        if (-not $expected -or $expected -ne $actual) {
            Fail "checksum mismatch for ${archive}: expected '$expected', got '$actual'. The download is damaged or was tampered with; nothing was installed"
        }
        Write-Host "Checksum OK (sha256 $actual)"

        Expand-Archive -Path $zip -DestinationPath $tmp -Force
        $exe = Join-Path (Join-Path $tmp $name) 'ursa.exe'
        if (-not (Test-Path $exe)) { Fail "$archive holds no $name\ursa.exe" }

        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        $dest = Join-Path $installDir 'ursa.exe'
        # Copy next to the target, then replace it in one move.
        $staged = Join-Path $installDir ".ursa.$PID.tmp"
        Copy-Item -Path $exe -Destination $staged -Force
        try {
            Move-Item -Path $staged -Destination $dest -Force
        } catch {
            Remove-Item -Force -ErrorAction SilentlyContinue $staged
            Fail "could not replace $dest; is ursa running? Close it and run this again. ($_)"
        }
    } finally {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
    }

    Write-Host ''
    Write-Host "Installed $(& $dest --version) to $dest"

    # Add the directory to the user PATH, for new terminals, and to this one.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @($userPath -split ';' | Where-Object { $_ })
    if ($entries -notcontains $installDir) {
        [Environment]::SetEnvironmentVariable('Path', (@($entries) + $installDir) -join ';', 'User')
        Write-Host "Added $installDir to your user PATH. Open a new terminal to use it there."
    }
    if (@($env:Path -split ';') -notcontains $installDir) {
        $env:Path = "$env:Path;$installDir"
    }

    Write-Host ''
    Write-Host 'Try it:'
    Write-Host '  ursa -c "SELECT 42"'
}
