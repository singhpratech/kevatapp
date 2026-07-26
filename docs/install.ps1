# Kevat installer for Windows — https://kevat.app
#
#   irm https://kevat.app/install.ps1 | iex
#
# Downloads the build for this machine, checks it against the release SHA256SUMS,
# and installs the application: kevat.exe (the command line), kevatw.exe (the same
# program linked as a windowed app, so no console flashes up), the icon, a Start
# Menu shortcut, and your user PATH. No service is registered and nothing is
# written outside the install directory, the Start Menu and your user PATH.
#
# Uninstall: delete the install directory (default %LOCALAPPDATA%\Programs\Kevat),
# the Start Menu shortcut (%APPDATA%\Microsoft\Windows\Start Menu\Programs\Kevat.lnk),
# the Desktop shortcut if you asked for one, and the PATH entry added below. The
# exact paths are printed at the end of the install.
#
# Environment:
#   KEVAT_INSTALL_DIR   where to put the binaries  (default: %LOCALAPPDATA%\Programs\Kevat)
#   KEVAT_VERSION       tag to install, e.g. v0.1.0 (default: latest)
#   KEVAT_DESKTOP       set to 1 to also create a Desktop shortcut

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = 'singhpratech/kevatapp'
$installDir = if ($env:KEVAT_INSTALL_DIR) { $env:KEVAT_INSTALL_DIR }
              else { Join-Path $env:LOCALAPPDATA 'Programs\Kevat' }
$version = if ($env:KEVAT_VERSION) { $env:KEVAT_VERSION } else { 'latest' }

function Write-Ok   { param($m) Write-Host "  " -NoNewline; Write-Host "OK" -ForegroundColor Green -NoNewline; Write-Host " $m" }
function Write-Warn { param($m) Write-Host "  " -NoNewline; Write-Host "!" -ForegroundColor Yellow -NoNewline; Write-Host " $m" }
# `throw`, never `exit`. This script is meant to be run as `irm ... | iex`, which
# executes in the caller's session scope — `exit` there closes the user's PowerShell
# window, taking the error text with it. That would make the checksum refusal, the one
# message that most needs reading, the one least likely to be seen.
function Die {
    param($m)
    Write-Host ""
    Write-Host "error: " -ForegroundColor Red -NoNewline
    Write-Host $m
    throw $m
}

# Older Windows PowerShell defaults to TLS 1.0, which GitHub refuses.
# -bor, not assignment: assigning would switch off TLS 1.3 / SystemDefault, and because
# iex runs in the caller's session that downgrade would outlive the installer.
try {
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {}

# ── what are we running on ───────────────────────────────────────────────────
# PROCESSOR_ARCHITECTURE describes the current *process*: 32-bit PowerShell on 64-bit
# Windows reports x86, which used to abort the install on a perfectly capable machine.
# ARCHITEW6432 is set only in that WOW64 case and names the real machine architecture.
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 }
        else { $env:PROCESSOR_ARCHITECTURE }
switch ($arch) {
    'AMD64' { $asset = 'kevat-x86_64-windows.zip' }
    'ARM64' {
        # No native ARM64 build yet. Windows on ARM runs x64 under emulation, so
        # this works — it is just not as fast as a native build would be.
        $asset = 'kevat-x86_64-windows.zip'
        Write-Warn 'No native ARM64 build yet — installing the x64 build, which runs under emulation.'
    }
    'x86'   { Die "This is a 32-bit Windows installation, and there is no 32-bit build.
    Build from source instead:  cargo install --git https://github.com/$repo" }
    default { Die "unsupported architecture: $arch" }
}

# Assets carry the version in their names, so the plain /latest/download/ link no longer
# resolves — ask the API for the current tag and build the exact URL and filename.
if ($version -eq 'latest') {
    try {
        $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -UseBasicParsing
        $tag = $rel.tag_name
    } catch {
        Die "could not reach GitHub to find the latest version. Download from https://github.com/$repo/releases/latest"
    }
} else {
    $tag = $version
}
$ver = $tag -replace '^v', ''
# kevat-x86_64-windows.zip -> kevat-<ver>-x86_64-windows.zip
$dl = "kevat-$ver-" + ($asset -replace '^kevat-', '')
$base = "https://github.com/$repo/releases/download/$tag"

Write-Host ""
Write-Host "Installing Kevat" -ForegroundColor White
Write-Ok "windows / $arch -> $dl"

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("kevat-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
    # ── fetch ────────────────────────────────────────────────────────────────
    $zip  = Join-Path $tmp $dl
    $sums = Join-Path $tmp 'SHA256SUMS'
    try {
        Invoke-WebRequest -Uri "$base/$dl"         -OutFile $zip  -UseBasicParsing
        Invoke-WebRequest -Uri "$base/SHA256SUMS"  -OutFile $sums -UseBasicParsing
    } catch {
        # Name the URL — a bare "(404) Not Found" gives the user nothing to check.
        # Releases before v0.2.7 used unversioned asset names, so pinning one of those
        # via KEVAT_VERSION can only 404 here; say so rather than looking broken.
        $hint = if ($version -ne 'latest') {
            "`n    (Releases before v0.2.7 use different file names and cannot be installed by this script." +
            "`n     Download it directly from https://github.com/$repo/releases/tag/$tag)"
        } else { '' }
        Die "download failed: $base/$dl`n    $($_.Exception.Message)$hint"
    }
    Write-Ok 'downloaded'

    # ── verify before trusting a single byte ─────────────────────────────────
    $actual = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLower()
    # Anchor on the whitespace before the name too, or an entry called
    # "evil-kevat-x86_64-windows.zip" would satisfy a lookup for "kevat-x86_64-windows.zip".
    $line = Select-String -Path $sums -Pattern ('\s' + [Regex]::Escape($dl) + '$') | Select-Object -First 1
    if (-not $line) { Die "$dl is not listed in SHA256SUMS" }
    $expected = ($line.Line -split '\s+')[0].ToLower()

    if ($actual -ne $expected) {
        Die "checksum mismatch for $dl`n    expected $expected`n    actual   $actual`n    Refusing to install. This is what that check is for."
    }
    Write-Ok 'sha256 verified'

    # ── install ──────────────────────────────────────────────────────────────
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $exe = Join-Path $tmp 'kevat.exe'
    if (-not (Test-Path $exe)) { Die 'archive did not contain kevat.exe' }

    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
    $target = Join-Path $installDir 'kevat.exe'
    try {
        Copy-Item $exe $target -Force
    } catch {
        Die "could not write to $installDir — is kevat.exe currently running?"
    }
    Write-Ok "installed to $target"

    # kevatw.exe is the same program linked with the GUI subsystem: launched from a
    # shortcut it opens the window with no console flashing behind it, which the
    # console-subsystem kevat.exe cannot avoid. Older releases predate it — those
    # get the PATH install only, and no shortcut is fabricated for them.
    $targetW = Join-Path $installDir 'kevatw.exe'
    $haveGui = Test-Path (Join-Path $tmp 'kevatw.exe')
    if ($haveGui) {
        Copy-Item (Join-Path $tmp 'kevatw.exe') $targetW -Force
        if (Test-Path (Join-Path $tmp 'kevat.ico')) {
            Copy-Item (Join-Path $tmp 'kevat.ico') (Join-Path $installDir 'kevat.ico') -Force
        }
    }

    $reported = & $target --version 2>$null
    if ($reported) { Write-Ok $reported }

    # ── Start Menu (and optional Desktop) shortcut ───────────────────────────
    # Per-user Programs folder — no admin rights, nothing machine-wide. The shortcut
    # points at kevatw.exe so launching it never flashes a console window.
    $lnkPath = $null
    $desktopLnk = $null
    if ($haveGui) {
        try {
            $lnkPath = Join-Path ([Environment]::GetFolderPath('Programs')) 'Kevat.lnk'
            $shell = New-Object -ComObject WScript.Shell
            $lnk = $shell.CreateShortcut($lnkPath)
            $lnk.TargetPath = $targetW
            $lnk.WorkingDirectory = $installDir
            $lnk.Description = 'Copy and move large folders to external drives - survives unplugs and resumes'
            # The .ico beside the exe, when the archive shipped one; the exe itself
            # carries the same icon as an embedded resource, so either way the entry
            # shows the Kevat mark rather than a generic program icon.
            $ico = Join-Path $installDir 'kevat.ico'
            $lnk.IconLocation = if (Test-Path $ico) { "$ico,0" } else { "$targetW,0" }
            $lnk.Save()
            Write-Ok 'added Kevat to the Start Menu'
        } catch {
            $lnkPath = $null
            Write-Warn "could not create the Start Menu shortcut: $($_.Exception.Message)"
        }

        if ($env:KEVAT_DESKTOP -eq '1' -and $lnkPath) {
            try {
                $desktopLnk = Join-Path ([Environment]::GetFolderPath('Desktop')) 'Kevat.lnk'
                Copy-Item $lnkPath $desktopLnk -Force
                Write-Ok 'added a Desktop shortcut'
            } catch {
                $desktopLnk = $null
                Write-Warn "could not create the Desktop shortcut: $($_.Exception.Message)"
            }
        }
    } else {
        Write-Warn 'this release has no windowed build (kevatw.exe) — installed the command line only'
    }

    # ── is it reachable ──────────────────────────────────────────────────────
    # User PATH only: this never touches the machine-wide value, so no admin
    # rights are needed and nothing is changed for other accounts.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { $userPath = '' }
    $onPath = ($userPath -split ';' | Where-Object { $_.TrimEnd('\') -ieq $installDir.TrimEnd('\') })

    if (-not $onPath) {
        $newPath = if ($userPath.Length -gt 0) { "$($userPath.TrimEnd(';'));$installDir" } else { $installDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        $env:Path = "$env:Path;$installDir"
        Write-Ok "added $installDir to your user PATH"
        Write-Host ""
        Write-Warn 'Open a new terminal for the PATH change to apply everywhere.'
    }

    Write-Host ""
    if ($haveGui) {
        Write-Host "  Open " -NoNewline
        Write-Host "Kevat" -ForegroundColor Cyan -NoNewline
        Write-Host " from the Start Menu, or run " -NoNewline
        Write-Host "kevat SRC DEST" -ForegroundColor Cyan -NoNewline
        Write-Host " to copy a folder."
    } else {
        Write-Host "  Run " -NoNewline
        Write-Host "kevat SRC DEST" -ForegroundColor Cyan -NoNewline
        Write-Host " to copy a folder. Run it again to resume."
    }
    # Deleting the exe alone would leave a dead Start Menu entry, so spell out what a
    # full uninstall removes while the paths are known.
    Write-Host ""
    Write-Host "  Uninstall: delete $installDir" -NoNewline
    if ($lnkPath) { Write-Host " and $lnkPath" -NoNewline }
    if ($desktopLnk) { Write-Host " and $desktopLnk" -NoNewline }
    Write-Host ", then remove the PATH entry."
    Write-Host ""
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
