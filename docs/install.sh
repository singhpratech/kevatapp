#!/bin/sh
# Kevat installer — https://kevat.app
#
#   curl -fsSL https://kevat.app/install.sh | sh
#
# POSIX sh on purpose: works under bash, zsh, dash and ash without modification.
# Downloads the build for this machine, checks it against the release SHA256SUMS,
# and installs the binary. On Linux it also puts Kevat in your applications menu
# (a .desktop entry plus icons under ~/.local/share); on macOS it assembles
# Kevat.app into ~/Applications. No service is registered.
#
# Uninstall: remove the binary and the launcher entry — the exact paths are
# printed at the end of the install, and listed again at the bottom of this file.
#
# Environment:
#   KEVAT_INSTALL_DIR   where to put the binary   (default: ~/.local/bin)
#   KEVAT_VERSION       tag to install, e.g. v0.1.0 (default: latest)
#   KEVAT_VARIANT       Linux only: "gui" (default) installs the application build,
#                       which links glibc and wants X11 or Wayland; "cli" installs
#                       the static musl command-line build that runs on any
#                       distribution — servers, containers, headless Raspberry Pi.
#   KEVAT_BASE_URL      override the download base (mirror or offline test);
#                       must serve the release assets and SHA256SUMS.

set -eu

REPO="singhpratech/kevatapp"
INSTALL_DIR="${KEVAT_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${KEVAT_VERSION:-latest}"
VARIANT="${KEVAT_VARIANT:-gui}"

# Colour only when stdout is a terminal — piped into sh, this is often not the case.
if [ -t 1 ]; then
  B=$(printf '\033[1m'); G=$(printf '\033[32m'); Y=$(printf '\033[33m')
  R=$(printf '\033[31m'); N=$(printf '\033[0m')
else
  B=''; G=''; Y=''; R=''; N=''
fi

say()  { printf '%s\n' "$*"; }
ok()   { printf '  %s✓%s %s\n' "$G" "$N" "$*"; }
warn() { printf '  %s!%s %s\n' "$Y" "$N" "$*"; }
die()  { printf '\n%serror:%s %s\n' "$R" "$N" "$*" >&2; exit 1; }

TMP=''
cleanup() { [ -n "$TMP" ] && rm -rf "$TMP"; }
trap cleanup EXIT
# Re-raise the signal after cleaning up, rather than returning into the script: a plain
# `trap cleanup INT` lets execution continue, so Ctrl-C during the download was reported
# as "download failed" with exit 1, and a Ctrl-C after the install was ignored entirely.
trap 'cleanup; trap - INT; kill -INT $$' INT
trap 'cleanup; trap - TERM; kill -TERM $$' TERM

# ── what are we running on ───────────────────────────────────────────────────
os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Linux)  os_name=linux ;;
  Darwin) os_name=macos ;;
  *) die "unsupported operating system: $os
    Kevat ships builds for Linux, macOS and Windows.
    On Windows use PowerShell:  irm https://kevat.app/install.ps1 | iex" ;;
esac

case "$arch" in
  x86_64|amd64)  arch_name=x86_64 ;;
  aarch64|arm64) arch_name=aarch64 ;;
  armv7l|armv6l|arm)
    die "32-bit ARM ($arch) is not built yet.
    A 64-bit Raspberry Pi OS will work. Otherwise build from source:
      cargo install --git https://github.com/$REPO" ;;
  *) die "unsupported architecture: $arch" ;;
esac

# The only combination that resolves to a build we do not publish.
if [ "$os_name" = macos ] && [ "$arch_name" = x86_64 ]; then
  die "Intel Macs are not built yet — only Apple Silicon.
    Build from source instead:
      cargo install --git https://github.com/$REPO"
fi

if [ "$os_name" = linux ]; then
  case "$VARIANT" in
    # The application build: dynamically linked (glibc), opens a window when run
    # with no arguments, and gets a desktop entry below. Needs X11 or Wayland.
    gui) asset="kevat-${arch_name}-linux.tar.gz" ;;
    # The static musl build: command line only, zero runtime dependencies, runs on
    # any distribution. The right answer for servers, containers and headless Pis.
    cli) asset="kevat-${arch_name}-linux-cli.tar.gz" ;;
    *) die "KEVAT_VARIANT must be 'gui' or 'cli', not '$VARIANT'" ;;
  esac
else
  asset="kevat-${arch_name}-macos.tar.gz"
fi

# Release assets carry the version in their names (kevat-1.2.3-x86_64-linux.tar.gz), so a
# downloaded file says what it is. That means the plain `/latest/download/` link no longer
# resolves — instead ask the API for the current tag and build the exact URL. `cut` on the
# quote, not a regex, so it works the same under dash, bash, ash and zsh.
resolve_latest_tag() {
  api="https://api.github.com/repos/$REPO/releases/latest"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$api" 2>/dev/null
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$api" 2>/dev/null
  fi | grep -m1 '"tag_name"' | cut -d'"' -f4
}

if [ -n "${KEVAT_BASE_URL:-}" ]; then
  # Mirror or offline test: serve the plain (unversioned) names from a directory.
  base="$KEVAT_BASE_URL"
  dl="$asset"
else
  if [ "$VERSION" = latest ]; then
    tag=$(resolve_latest_tag)
    [ -n "$tag" ] || die "could not reach GitHub to find the latest version.
    Check your connection, or download from https://github.com/$REPO/releases/latest"
  else
    tag="$VERSION"
  fi
  ver="${tag#v}"
  dl="kevat-${ver}-${asset#kevat-}"
  base="https://github.com/$REPO/releases/download/$tag"
fi

say ""
say "${B}Installing Kevat${N}"
ok "$os_name / $arch_name → $dl"

# A GUI build on a machine with no display is usually a headless box that wanted the
# static build. Only a hint — SSH sessions into desktops trip this too, and the GUI
# binary is still a full CLI, so installing it is not wrong.
if [ "$os_name" = linux ] && [ "$VARIANT" = gui ] \
   && [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
  warn "no display detected — for a headless server or container, the static build"
  warn "is the better fit:  curl -fsSL https://kevat.app/install.sh | KEVAT_VARIANT=cli sh"
fi

# ── fetch ────────────────────────────────────────────────────────────────────
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  die "neither curl nor wget is available"
fi

TMP=$(mktemp -d 2>/dev/null || mktemp -d -t kevat)
[ -d "$TMP" ] || die "could not create a temporary directory"

fetch "$base/$dl" "$TMP/$dl" || {
    # Releases before v0.2.7 used unversioned asset names, so pinning one of those via
    # KEVAT_VERSION can only 404 here — say so rather than looking broken.
    case "$VERSION" in
        latest) die "download failed: $base/$dl" ;;
        *) die "download failed: $base/$dl
    (Releases before v0.2.7 use different file names and cannot be installed by this
     script. Download it directly from https://github.com/$repo/releases/tag/$tag)" ;;
    esac
}
fetch "$base/SHA256SUMS" "$TMP/SHA256SUMS" || die "could not fetch SHA256SUMS"
ok "downloaded"

# ── verify before trusting a single byte ─────────────────────────────────────
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$TMP/$dl" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$TMP/$dl" | cut -d' ' -f1)
else
  actual=''
fi

# -F so the dots in the filename are literal, not regex wildcards; head -1 so a
# duplicated line yields one hash rather than a two-line string that can never match.
expected=$(grep -F " $dl" "$TMP/SHA256SUMS" 2>/dev/null | grep -F -- "$dl" | head -1 | cut -d' ' -f1 || true)

if [ -z "$actual" ]; then
  # Refuse rather than warn. The website promises these scripts check the download and
  # stop if it does not match; installing anyway on a machine with no sha256 tool would
  # make that untrue exactly where it matters most, and in `curl | sh` the warning
  # scrolls past unread.
  die "no sha256 tool found (looked for sha256sum and shasum).
    Refusing to install unverified. Install coreutils, or download the archive and
    check it against SHA256SUMS yourself."
elif [ -z "$expected" ]; then
  die "$dl is not listed in SHA256SUMS"
elif [ "$actual" != "$expected" ]; then
  die "checksum mismatch for $dl
    expected $expected
    actual   $actual
    Refusing to install. This is what that check is for."
else
  ok "sha256 verified"
fi

# ── install the binary ───────────────────────────────────────────────────────
tar -xzf "$TMP/$dl" -C "$TMP" || die "could not unpack $dl"
[ -f "$TMP/kevat" ] || die "archive did not contain the kevat binary"

mkdir -p "$INSTALL_DIR" || die "could not create $INSTALL_DIR"
if [ ! -w "$INSTALL_DIR" ]; then
  die "$INSTALL_DIR is not writable.
    Choose another location:
      curl -fsSL https://kevat.app/install.sh | KEVAT_INSTALL_DIR=\$HOME/bin sh"
fi

# Replace via a temporary name and mv: a running binary cannot be overwritten in
# place on some systems, but it can always be replaced by a rename.
mv "$TMP/kevat" "$INSTALL_DIR/kevat.new" || die "could not write to $INSTALL_DIR"
chmod 755 "$INSTALL_DIR/kevat.new"
mv "$INSTALL_DIR/kevat.new" "$INSTALL_DIR/kevat"
ok "installed to $INSTALL_DIR/kevat"

installed=$("$INSTALL_DIR/kevat" --version 2>/dev/null) || installed=''
if [ -n "$installed" ]; then
  ok "$installed"
elif [ "$os_name" = linux ] && [ "$VARIANT" = gui ]; then
  # The application build links glibc; on a musl distribution (Alpine, postmarketOS)
  # it installs cleanly but cannot start. The static build exists for exactly this.
  warn "the binary did not run — this build needs glibc. On a musl-based"
  warn "distribution install the static one:  KEVAT_VARIANT=cli"
fi

# ── make it an application ───────────────────────────────────────────────────
# Only when the archive carries the desktop payload: the CLI archives and pre-GUI
# releases do not, and this step must not fabricate an entry for a build that
# cannot open a window.
uninstall_extra=''
if [ "$os_name" = linux ] && [ -f "$TMP/kevat.desktop" ]; then
  data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
  apps_dir="$data_home/applications"
  mkdir -p "$apps_dir"

  # Icons first, one per hicolor size bucket, all named kevat.png so the desktop
  # entry's Icon=kevat resolves through the theme like any packaged application.
  for f in "$TMP"/icons/icon-*.png; do
    [ -f "$f" ] || continue
    px=$(basename "$f" .png)
    px=${px#icon-}
    mkdir -p "$data_home/icons/hicolor/${px}x${px}/apps"
    cp "$f" "$data_home/icons/hicolor/${px}x${px}/apps/kevat.png"
  done

  # Written fresh rather than copied from the archive so Exec can carry the
  # absolute path this install actually used — a PATH-relative Exec breaks when
  # KEVAT_INSTALL_DIR is somewhere the desktop session does not search.
  # StartupWMClass matches the window's WM_CLASS ("kevat"); without it the running
  # window shows up as a second, unnamed taskbar entry instead of binding to this
  # launcher. On Wayland the same binding happens through the file name kevat.desktop
  # matching the window's app_id.
  cat > "$apps_dir/kevat.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Version=1.0
Name=Kevat
GenericName=File Copier
Comment=Copy and move large folders to external drives — survives unplugs and resumes
TryExec=$INSTALL_DIR/kevat
Exec=$INSTALL_DIR/kevat
Icon=kevat
Terminal=false
Categories=Utility;FileTools;
Keywords=copy;move;backup;transfer;usb;drive;resume;external;
StartupWMClass=kevat
DESKTOP

  # Refresh the caches when the tools exist; when they do not, desktops rescan on
  # their own schedule and the entry still appears, just not instantly.
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$apps_dir" >/dev/null 2>&1 || true
  fi
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$data_home/icons/hicolor" >/dev/null 2>&1 || true
  fi
  ok "added Kevat to your applications menu"
  uninstall_extra="$apps_dir/kevat.desktop and $data_home/icons/hicolor/*/apps/kevat.png"
fi

if [ "$os_name" = macos ]; then
  # A minimal but real bundle, so Kevat shows up in Launchpad and Spotlight and can
  # be dragged to the Dock. The binary is copied, not symlinked: the bundle must
  # survive the CLI copy being moved or deleted, and vice versa.
  app="$HOME/Applications/Kevat.app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cp "$INSTALL_DIR/kevat" "$app/Contents/MacOS/kevat"
  chmod 755 "$app/Contents/MacOS/kevat"
  [ -f "$TMP/kevat.icns" ] && cp "$TMP/kevat.icns" "$app/Contents/Resources/kevat.icns"

  # CFBundleShortVersionString from the binary itself so the Finder's Get Info
  # matches --version; a build that failed to run above falls back to 0.
  app_ver=$(printf '%s' "$installed" | sed 's/^kevat //')
  [ -n "$app_ver" ] || app_ver=0
  cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>app.kevat.kevat</string>
	<key>CFBundleName</key>
	<string>Kevat</string>
	<key>CFBundleDisplayName</key>
	<string>Kevat</string>
	<key>CFBundleExecutable</key>
	<string>kevat</string>
	<key>CFBundleIconFile</key>
	<string>kevat</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleShortVersionString</key>
	<string>$app_ver</string>
	<key>CFBundleVersion</key>
	<string>$app_ver</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.utilities</string>
</dict>
</plist>
PLIST
  ok "assembled $app"
  uninstall_extra="$app"
fi

# ── is it reachable ──────────────────────────────────────────────────────────
case ":${PATH}:" in
  *":$INSTALL_DIR:"*)
    say ""
    say "  Run ${B}kevat${N} with no arguments to open the window, or"
    say "  ${B}kevat SRC DEST${N} to copy a folder. Run it again to resume."
    ;;
  *)
    say ""
    warn "$INSTALL_DIR is not on your PATH. Add it:"
    say ""
    say "    export PATH=\"\$PATH:$INSTALL_DIR\""
    say ""
    say "  Put that line in your ~/.bashrc or ~/.zshrc to make it permanent."
    ;;
esac

# Deleting the binary alone would leave a dead menu entry behind, so always say
# exactly what a full uninstall removes.
say ""
if [ -n "$uninstall_extra" ]; then
  say "  Uninstall: delete $INSTALL_DIR/kevat, plus $uninstall_extra"
else
  say "  Uninstall: delete $INSTALL_DIR/kevat — that is all of it"
fi
say ""
