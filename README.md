<p align="center">
  <img src="assets/hero.png" alt="A small ferry crossing dark water at night, carrying glowing crates of data" width="100%">
</p>

<h1 align="center">Kevat</h1>

<p align="center">
  <strong>Every byte reaches the far shore.</strong><br>
  Copies and moves large folders — or a single big file — to external drives, survives unplugs,<br>
  and resumes exactly where it stopped.
</p>

<p align="center">
  <a href="https://github.com/singhpratech/kevatapp/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/singhpratech/kevatapp?style=flat-square&label=release&color=5BB8A8&labelColor=16211F"></a>
  <a href="https://github.com/singhpratech/kevatapp/releases"><img alt="Total downloads" src="https://img.shields.io/github/downloads/singhpratech/kevatapp/total?style=flat-square&label=downloads&color=5BB8A8&labelColor=16211F"></a>
  <a href="https://github.com/singhpratech/kevatapp/actions/workflows/release.yml"><img alt="Release build" src="https://img.shields.io/github/actions/workflow/status/singhpratech/kevatapp/release.yml?style=flat-square&label=build&color=5BB8A8&labelColor=16211F"></a>
  <img alt="Platforms" src="https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-5BB8A8?style=flat-square&labelColor=16211F">
  <a href="https://crates.io/crates/kevat"><img alt="crates.io" src="https://img.shields.io/crates/v/kevat?style=flat-square&label=crates.io&color=5BB8A8&labelColor=16211F"></a>
  <a href="LICENSE"><img alt="MIT license" src="https://img.shields.io/badge/license-MIT-5BB8A8?style=flat-square&labelColor=16211F"></a>
  <img alt="Binary size" src="https://img.shields.io/badge/app-5.5%20MB-5BB8A8?style=flat-square&labelColor=16211F">
  <img alt="CLI binary size" src="https://img.shields.io/badge/static%20CLI-0.7%20MB-5BB8A8?style=flat-square&labelColor=16211F">
</p>

<p align="center">
  <a href="https://kevat.app">kevat.app</a>
</p>

---

> **Copy, move, resume, verify — as a window or a command line.** Run Kevat with no arguments and it
> opens a window: pick a folder, a file, or several items at once (Ctrl-click, Shift-click) and a
> place to put them, watch it work, stop and continue. Give it two paths and it stays a
> command-line tool. The window renders on the CPU and links no OpenGL. An **MSI** puts it in the
> Start Menu, a **DMG** drags into Applications, an **AppImage** is one runnable file; a separate
> static musl **CLI-only build** (~0.7 MB) runs on any distribution, servers and containers included.
>
> **Runs on Linux and Windows** — launched and used on both. The macOS build, the MSI and the DMG
> are CI-built and have not been run by anyone. The full kill-resume test suite is run on x86_64
> Linux; the ARM builds are published but unexercised.

## Install

**The installers are the front door.** Download one, open it, and Kevat is in your Start Menu,
Applications folder or app menu:

| Platform | Installer | What it does |
| --- | --- | --- |
| Windows x86_64 | [`kevat-x86_64-windows.msi`](https://github.com/singhpratech/kevatapp/releases/latest) | Start Menu shortcut (windowed — no console flash), `kevat` on your PATH, uninstall from Apps & Features |
| macOS Apple Silicon | [`kevat-aarch64-macos.dmg`](https://github.com/singhpratech/kevatapp/releases/latest) | open it, drag `Kevat.app` to Applications |
| Linux x86_64 | [`kevat-x86_64-linux.AppImage`](https://github.com/singhpratech/kevatapp/releases/latest) | one ~3.4 MB file: `chmod +x`, run — nothing to unpack |
| Linux ARM64 | [`kevat-aarch64-linux.AppImage`](https://github.com/singhpratech/kevatapp/releases/latest) | the same, for 64-bit ARM desktops |

Honesty where it costs: the MSI and DMG are **unsigned**. SmartScreen will object
once (**More info → Run anyway**); Gatekeeper needs **right-click → Open**, or on newer macOS
System Settings → Privacy & Security → **Open Anyway**, or
`xattr -dr com.apple.quarantine /Applications/Kevat.app`. The AppImage has been built and run on
Linux; it needs FUSE (without it: `./kevat-…AppImage --appimage-extract-and-run`) and an
X11/Wayland desktop, and its size was measured on a maintainer build from the same source.

**Prefer the terminal?** The scripts install the archive build and wire up the same menu entry,
`Kevat.app` or Start Menu shortcut, and print the exact paths a full uninstall deletes.

**Linux and macOS** — bash, zsh or sh:

```sh
curl -fsSL https://kevat.app/install.sh | sh
```

**Windows** — PowerShell:

```powershell
irm https://kevat.app/install.ps1 | iex
```

Both scripts detect your platform, verify the download against the release `SHA256SUMS` before
installing, and refuse to continue on a mismatch. Set `KEVAT_INSTALL_DIR` to choose where the binary
lands, or `KEVAT_VERSION` to pin a tag. On Linux, `KEVAT_VARIANT=cli` installs the static
command-line build instead (see below). On Windows, `KEVAT_DESKTOP=1` adds a Desktop shortcut too.

Or take a plain archive:

| Platform | Build | Download |
| --- | --- | --- |
| Linux | x86_64 — the application (window + CLI), needs glibc and X11/Wayland | [`kevat-x86_64-linux.tar.gz`](https://github.com/singhpratech/kevatapp/releases/latest) |
| Linux | ARM64 — the application, for ARM desktops | [`kevat-aarch64-linux.tar.gz`](https://github.com/singhpratech/kevatapp/releases/latest) |
| Linux | x86_64, **CLI only, static (musl)** — any distribution, servers, containers | [`kevat-x86_64-linux-cli.tar.gz`](https://github.com/singhpratech/kevatapp/releases/latest) |
| Linux | ARM64, **CLI only, static (musl)** — headless ARM machines | [`kevat-aarch64-linux-cli.tar.gz`](https://github.com/singhpratech/kevatapp/releases/latest) |
| macOS | Apple Silicon — the application (window + CLI) | [`kevat-aarch64-macos.tar.gz`](https://github.com/singhpratech/kevatapp/releases/latest) |
| Windows | x86_64 — `kevat.exe` (console/CLI) and `kevatw.exe` (windowed, for shortcuts) | [`kevat-x86_64-windows.zip`](https://github.com/singhpratech/kevatapp/releases/latest) |

The application builds are dynamically linked; the "runs on any distribution" property belongs to
the `-cli` archives, which are statically linked against musl (~0.7 MB) and need no desktop, no
FUSE and no particular glibc.

Every release ships a `SHA256SUMS` covering **every** asset, installers included. Check it before
trusting anything:

```sh
sha256sum -c SHA256SUMS --ignore-missing
```

**Uninstall:** the MSI uninstalls from Apps & Features; the AppImage is one file — delete it.
For the script installs, delete the binary plus the launcher entry the script created — on Linux
`~/.local/share/applications/kevat.desktop` and `~/.local/share/icons/hicolor/*/apps/kevat.png`, on
macOS `~/Applications/Kevat.app`, on Windows the install folder and
`%APPDATA%\Microsoft\Windows\Start Menu\Programs\Kevat.lnk`. The install scripts print these paths.

Have Rust? It's [on crates.io](https://crates.io/crates/kevat) — no C dependencies, so it compiles
anywhere Rust does:

```sh
cargo install kevat                   # the CLI
cargo install kevat --features gui    # the application (window when run with no arguments)
```

Run it with no arguments and it opens a window; give it two paths and it stays a command-line tool.
Note `cargo install` puts the binary on your PATH but adds no menu entry — use the installers or
install scripts above if you want one.

```sh
git clone https://github.com/singhpratech/kevatapp && cd kevatapp
cargo build --release --features gui
./target/release/kevat
```

```sh
kevat ~/Photos /media/backup/Photos     # copy; run it again to resume
kevat ~/Photos /media/backup/Photos --move
```

## The problem

Copying a large folder to an external drive is worse than the hardware deserves.

- Operating-system file managers copy single-threaded with small buffers, and Windows disables write
  caching on removable drives — so transfers crawl through the USB bridge.
- Thousands of small files pay per-file open and close overhead, plus a synchronous anti-malware scan
  per file on Windows. The drive could stream at full speed while the copy manages a fraction of it.
- An interrupted three-hour copy leaves nothing usable: no record of which files finished, and no way
  to resume a half-written 80 GB file.

## How it works

Copying a big folder to an external drive is worse than the hardware deserves: file managers copy one
stream at a time with small buffers, thousands of small files pay per-file overhead each, and an
interruption three hours in leaves nothing you can continue — no record of what finished, no way to
resume a half-written 80 GB file.

Kevat answers the last of those directly.

- **A journal kept off the drive.** Completed files, their hashes, and periodic checkpoints for the
  file in flight are recorded in the OS configuration directory — never on the destination. The only
  thing left on your external disk is the file being written, named `.kpart` until its bytes are
  proven and it is renamed into place.
- **Checkpoints are re-hashed before they are trusted.** On resume, a checkpoint is validated against
  what is *actually on disk* before a single byte is reused — a USB yank corrupts the tail, so a
  checkpoint on its own proves nothing. Copying never mutates the source, so the worst case after an
  interruption is re-copying the file that was in flight.
- **Verify, then move.** Every file is hashed with xxh3 as it is read; with `--verify` (on by default
  for a move) the destination is read back and compared before the file is accepted. In move mode the
  original is deleted only after its copy is verified and that fact is durable — a file is never in
  neither place. A move whose source and destination resolve to the same folder, through any alias, is
  refused before a byte moves.
- **One reader, one writer, a bounded channel.** The shape a rotational USB disk wants, and memory
  stays flat however large the files are.

The kill-resume suite is this promise as a test: a mixed tree of thousands of small files and
multi-gigabyte ones, `kill -9` at random points, resumed each time, with the result checked
byte-for-byte against the source — plus corruption injection, mount-alias refusal, concurrent-run
refusal, and a source edited mid-transfer. No release ships without it green.

## Interface

Two front ends in one binary: no arguments opens the window, arguments run the command line. The
source can be a whole folder or a single file; the destination is the folder it goes into. Resume
is automatic in both — never a flag you have to remember. The window renders on the CPU (no OpenGL,
no GPU driver), which is why the whole interface fits in one small binary.

```
kevat SRC DEST [--move] [--verify] [--no-verify] [--paranoid] [--dry-run]
```

## Building

```sh
cargo build --release                  # CLI only → target/release/kevat, ~0.7 MB
cargo build --release --features gui   # the application → ~5.5 MB
./tests/kill-resume.sh                 # the suite that gates every release
```

## What it does not copy

Said plainly, because finding out afterwards is worse:

- **Symbolic links are skipped**, and reported at the end of the run. A home directory
  full of links will copy its real files and list the links it left behind.
- **Permissions and ownership are not preserved.** Modification times are; the executable
  bit and the owner are not. An ext4-to-ext4 backup will need `chmod +x` afterwards.
- **It is additive, not a mirror.** Files already on the destination that no longer exist
  in the source are kept, never deleted. Files that exist in both and differ are replaced
  by default — the count is always shown first, and `--exists=keep` leaves them alone.

## Platforms

| Platform | Build |
|---|---|
| Linux x86_64 · ARM64 | application (window + CLI), plus a static musl CLI-only build |
| macOS Apple silicon | application (window + CLI) |
| Windows x86_64 | application (window + CLI) |

The application builds are dynamically linked and need a desktop session (glibc plus X11 or Wayland
on Linux). The static musl `-cli` build needs none of that — no glibc version to satisfy, no desktop,
no FUSE — one file for servers and containers.

**Runs on Linux and Windows** — the application has been launched and used on both. The macOS build,
the MSI and the DMG are CI-built and have not been run by anyone. The full kill-resume suite is run on
x86_64 Linux; the ARM builds are published but unexercised. Issues are welcome.

**Flatpak and Snap are excluded on purpose.** Their sandboxes route the destination through a portal
that hides the volume and device details Kevat relies on — inside one, a USB drive looks like a
network mount and resume can no longer tie the journal to it.

## Privacy

**The binary contains no network code.** No update checker, no telemetry, no HTTP client compiled in —
verifiable by reading the source or capturing traffic, not merely promised. Updates come from OS
package managers. Nothing is written to your destination drive except the files you asked for.

## Name

**Kevat** (केवट) is the ferryman who carried Rām (राम) across the river. He does not own the water and he
does not hurry it — he simply gets everything to the other side, and nothing is lost on the way.

## A sibling

[**Diskhoji**](https://diskhoji.org) is a disk space analyzer built to the same shape — one small
native binary, a parallel scanner, no network and no telemetry. Diskhoji finds what is eating your
disk; Kevat moves it.

## Credit and license

**Kevat** was created by **Prateek Singh** — the name, the design, and the project.

MIT licensed — see [LICENSE](LICENSE). Copyright © 2026 Prateek Singh.
