//! The graphical front end.
//!
//! No eframe: it exists to bind egui to `glow` (OpenGL) or `wgpu`, and this GUI
//! deliberately links neither. So it drives `winit` directly, paints into a `softbuffer` framebuffer with
//! the CPU rasterizer in [`raster`], and pulls in no GL, no Mesa and no GPU driver.
//!
//! Threading model: the engine runs on a worker thread and publishes into atomics, and
//! the UI samples them at 10 fps. The UI thread never touches the filesystem while a
//! transfer is running, so a stalled drive cannot freeze the window.

mod icon;
mod raster;
mod theme;

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Align, Color32, FontFamily, FontId, Layout, RichText, Rounding, Stroke, Vec2};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::engine::{self, Options, Progress, Summary};
use crate::journal;
use crate::journal::Mode;
use crate::scan;
use theme::Palette;

/// Transfers repaint at 10 fps. A CPU rasterizer makes a full-window repaint costly,
/// and nothing in a progress readout benefits from more.
const TICK: Duration = Duration::from_millis(100);

// ── shared state between the UI and the worker ───────────────────────────────

#[derive(Default)]
struct Shared {
    /// Move mode phase 2: how many originals there are to remove, and how many are
    /// gone. `deleting_total` is 0 until every file is copied and proven.
    deleting_total: AtomicUsize,
    deleted: AtomicUsize,
    /// Bytes of an already-written prefix verified so far, while a resume re-checks it;
    /// 0 when not checking. Drives the "Checking what's already there" line.
    checking: AtomicU64,
    /// Entries the scan could not take (symlinks, unreadable files) with the reason.
    /// The CLI prints these; the GUI used to drop them silently and show a clean
    /// "Copied" over a transfer that had quietly omitted files.
    scan_skipped: Mutex<Vec<(PathBuf, String)>>,
    files_total: AtomicUsize,
    files_done: AtomicUsize,
    files_skipped: AtomicUsize,
    bytes_total: AtomicU64,
    /// Everything done so far — freshly written plus carried-forward — drives the bar.
    bytes_done: AtomicU64,
    /// Only bytes actually written this run — drives the speed and the ETA, so a resume
    /// that carries forward 39 of 40 GB does not report a false 4 GB/s or a wrong ETA.
    bytes_fresh: AtomicU64,
    finished: AtomicBool,
    cancel: AtomicBool,
    /// Set when the user presses Stop, so the button can show it was heard.
    stopping: AtomicBool,
    current: Mutex<String>,
    outcome: Mutex<Option<Result<Summary, String>>>,
    /// Folder-skeleton phase: folders made so far and the total. Total stays 0 until
    /// the phase starts; on a big tree it is minutes of drive work that moves no file
    /// bytes, and without its own line it reads as a hang.
    dirs_done: AtomicUsize,
    dirs_total: AtomicUsize,
    /// The engine's itemised time estimate. Seconds, with 0 meaning "not known yet";
    /// a known sub-minute estimate is stored as 1. Split kept so the running screen
    /// can say *why* — "the small files are most of it".
    eta_secs: AtomicI64,
    eta_small_left: AtomicU64,
    eta_small_secs: AtomicI64,
    eta_big_bytes: AtomicU64,
    eta_big_secs: AtomicI64,
    /// How many manifest files sit inside app-cache folders (AppData, node_modules…)
    /// when the skip-caches choice was OFF. Drives the mid-copy hint that stopping and
    /// re-starting without them would finish much sooner.
    cache_files: AtomicUsize,
    /// Files that could not be copied so far. Shown live: 24,000 failures must build
    /// up in front of the user, not ambush them on the final screen.
    errors: AtomicUsize,
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Pick,
    Running,
    Done,
    Erase,
    History,
}

/// State for the erase-a-drive flow, kept separate so the dangerous path shares nothing
/// with the ordinary copy state.
#[derive(Default)]
struct EraseState {
    drives: Vec<DriveInfo>,
    selected: Option<usize>,
    fs: Option<Fs>,
    /// The user must type the drive's name here to arm the Erase button.
    confirm: String,
    status: Option<Result<String, String>>,
}

/// One row in the browser.
#[derive(Clone)]
struct Item {
    path: PathBuf,
    is_dir: bool,
    /// Modification time, unix seconds; 0 if unknown.
    mtime: i64,
    /// File size in bytes; 0 for folders (a folder's true size would mean walking it).
    size: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Name,
    Size,
    Modified,
}

struct Browser {
    open_for: Option<Field>,
    cwd: PathBuf,
    /// Visible entries. Files are listed only when picking a source — a destination is
    /// always a folder to write into.
    entries: Vec<Item>,
    error: Option<String>,
    sort_key: SortKey,
    /// Descending order when true. Folders always sort before files regardless.
    sort_desc: bool,
    /// When browsing the drive-locked field, the removable drive whose subtree we are in.
    /// None means "show the list of connected drives" — the locked field is restricted to
    /// external drives, so you cannot navigate up out of a drive into the internal
    /// filesystem, and with nothing plugged in there is nothing to browse but the
    /// "connect a drive" prompt.
    drive_root: Option<PathBuf>,
    /// Which field is drive-locked: Dest when copying *to* a drive (the default),
    /// Source when copying *from* one. Kept in sync with `Ui::direction` by the toggle.
    locked: Field,
    /// Names (not full paths) selected in the current folder. Ctrl-click toggles one,
    /// Shift-click extends from the anchor, a plain click replaces the set. Empty means
    /// "the folder itself", which is what the Choose button then commits.
    picked: std::collections::BTreeSet<std::ffi::OsString>,
    /// Row index the last plain/ctrl click landed on — the origin for Shift-ranges.
    anchor: Option<usize>,
    /// Dotfiles are hidden by default, as everywhere — but a backup of `~/.config` is
    /// a real thing to want, and without a toggle the GUI simply could not express it.
    show_hidden: bool,
    /// Inline "new folder" name being typed in the destination picker; None when the
    /// control is closed.
    new_folder: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Source,
    Dest,
}

/// Which way the transfer points. The drive restriction always sits on the drive side:
/// forward locks the destination to external drives, reverse locks the source. An
/// explicit, always-visible toggle — never inferred from the paths — so the locked
/// panel physically cannot point the wrong way while the user believes otherwise.
#[derive(Clone, Copy, PartialEq)]
enum Direction {
    ToDrive,
    FromDrive,
}

impl Browser {
    fn new() -> Self {
        let cwd = dirs_home();
        let mut b = Browser {
            open_for: None,
            cwd,
            entries: Vec::new(),
            error: None,
            sort_key: SortKey::Name,
            sort_desc: false,
            drive_root: None,
            locked: Field::Dest,
            picked: std::collections::BTreeSet::new(),
            anchor: None,
            show_hidden: false,
            new_folder: None,
        };
        b.reload();
        b
    }

    /// List the current directory. Folders are always shown, to navigate into; files are
    /// shown only when choosing a source, because Kevat can copy a single file as happily
    /// as a folder — and a destination is always a folder to write into, where files would
    /// just be noise. Unreadable directories report rather than vanish.
    fn reload(&mut self) {
        self.entries.clear();
        // A selection names entries in the folder we are leaving; carrying it into a
        // different listing would silently act on same-named items elsewhere.
        self.picked.clear();
        self.anchor = None;
        self.error = None;
        let show_files = self.open_for == Some(Field::Source);
        match std::fs::read_dir(&self.cwd) {
            Ok(rd) => {
                for ent in rd.flatten() {
                    let p = ent.path();
                    let hidden = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with('.'))
                        .unwrap_or(false);
                    if hidden && !self.show_hidden {
                        continue;
                    }
                    // symlink_metadata: do not follow links, and one stat gives both the
                    // type and the modification time the Date column shows.
                    let md = std::fs::symlink_metadata(&p).ok();
                    let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                    if is_dir || show_files {
                        let mtime = md.as_ref().map(scan::mtime_of).unwrap_or(0);
                        let size = if is_dir {
                            0
                        } else {
                            md.as_ref().map(|m| m.len()).unwrap_or(0)
                        };
                        self.entries.push(Item {
                            path: p,
                            is_dir,
                            mtime,
                            size,
                        });
                    }
                }
                self.resort();
            }
            Err(e) => self.error = Some(format!("cannot read this folder: {e}")),
        }
    }

    /// Order the current entries by the active column. Folders always come before files —
    /// the convention every file manager follows — and the chosen key orders within each
    /// group, reversed when `sort_desc`.
    fn resort(&mut self) {
        let key = self.sort_key;
        let desc = self.sort_desc;
        self.entries.sort_by(|a, b| {
            // Folders first, unconditionally.
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let by_name = |a: &Item, b: &Item| {
                    a.path
                        .file_name()
                        .map(|n| n.to_ascii_lowercase())
                        .cmp(&b.path.file_name().map(|n| n.to_ascii_lowercase()))
                };
                let ord = match key {
                    SortKey::Name => by_name(a, b),
                    // Ties fall back to name order — folders all have size 0, and a
                    // size-sorted folder block in read_dir order looks unsorted.
                    SortKey::Size => a.size.cmp(&b.size).then_with(|| by_name(a, b)),
                    SortKey::Modified => a.mtime.cmp(&b.mtime),
                };
                if desc {
                    ord.reverse()
                } else {
                    ord
                }
            })
        });
    }

    /// Handle a click on a column header: switch to that key, or flip direction if it is
    /// already active. Newest-first is the natural default when switching to Date.
    fn sort_by(&mut self, key: SortKey) {
        if self.sort_key == key {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort_key = key;
            // Size and Date read most naturally largest/newest first.
            self.sort_desc = matches!(key, SortKey::Modified | SortKey::Size);
        }
        self.resort();
    }

    fn go(&mut self, p: PathBuf) {
        self.cwd = p;
        self.reload();
    }

    /// Open the picker for a field and list from a sensible starting directory: the parent
    /// of a previously chosen file, or a chosen folder itself, or wherever it last was.
    fn open(&mut self, which: Field, current: Option<&Path>) {
        self.open_for = Some(which);
        // The destination browser has no SIZE header (folders have no size to show), so
        // a size sort carried over from the source browser would order its folders
        // invisibly and uncontrollably. Fall back to names.
        if which == Field::Dest && self.sort_key == SortKey::Size {
            self.sort_key = SortKey::Name;
            self.sort_desc = false;
        }
        if which == self.locked {
            // The drive-locked field is restricted to connected external drives. If a
            // drive was already chosen, drop back into that drive's subtree; otherwise
            // show the drive list (drive_root = None). Never start in the internal
            // filesystem.
            self.drive_root = current
                .map(|p| if p.is_dir() { p.to_path_buf() } else { p.parent().map(Path::to_path_buf).unwrap_or_else(|| p.to_path_buf()) })
                .and_then(|p| removable_root_of(&p));
            if let Some(root) = &self.drive_root {
                self.cwd = current
                    .map(|p| if p.is_dir() { p.to_path_buf() } else { p.parent().map(Path::to_path_buf).unwrap_or_else(|| root.clone()) })
                    .unwrap_or_else(|| root.clone());
            }
            self.reload();
            return;
        }
        match current {
            Some(p) if p.is_dir() => self.cwd = p.to_path_buf(),
            Some(p) => {
                if let Some(parent) = p.parent() {
                    self.cwd = parent.to_path_buf();
                }
            }
            None => {
                // Don't inherit a drive subtree left over from browsing the locked
                // field — the free panel starts somewhere on this computer.
                if removable_root_of(&self.cwd).is_some() {
                    self.cwd = dirs_home();
                }
            }
        }
        self.reload();
    }
}

struct Ui {
    screen: Screen,
    /// Forward ("to a drive") or reverse ("from a drive"). Toggling clears the picked
    /// paths — the two fields swap semantics, and predictability beats cleverness.
    direction: Direction,
    src: Option<PathBuf>,
    /// A multi-selection: `src` is then the folder holding these entries, and each keeps
    /// its own name at the destination. Empty for an ordinary single source.
    src_names: Vec<PathBuf>,
    /// What to do about destination files that already exist and differ. Replace is the
    /// backup default, but the count is always stated first — silently overwriting a
    /// newer file at the destination was the one way this app could lose data without
    /// saying so.
    on_exists: engine::OnExists,
    /// Cached conflict count and the (source, destination) pair it was computed for —
    /// the count needs a scan, which must not run on every repaint.
    conflicts: usize,
    conflict_key: Option<String>,
    /// Held while a transfer runs so the machine cannot sleep under it; dropped when
    /// the transfer ends, which releases the block.
    awake: Option<StayAwake>,
    dst: Option<PathBuf>,
    mode: Mode,
    verify: bool,
    /// Leave out app-cache folders (AppData, node_modules, .cache…) at scan time.
    /// Off by default: a copier that silently omits folders is worse than a slow one;
    /// this is a choice the user makes with the folder names in front of them.
    skip_caches: bool,
    /// Leave out cloud-sync folders — OneDrive/Dropbox/Google Drive/iCloud, matched by
    /// name prefix ("OneDrive - Tenant" carries the tenant in the name). Separate from
    /// caches on purpose: these are real data with a server-side copy, not rebuildable
    /// junk, so they get their own explicit choice. Names that don't exist on this
    /// platform simply never match.
    skip_cloud: bool,
    /// One-shot: relative paths to leave out of the next run — the "finish without
    /// the unreadable files" button. Consumed by `start()`, never persisted: giving
    /// up on a file must be chosen per run, not remembered silently.
    skip_rels: Vec<PathBuf>,
    /// Smoothed files-per-second over the last few seconds, and the last sample point.
    /// Small-file stretches sit at 1–3 MB/s where the byte speed looks broken; the
    /// per-file rate is the number that is actually moving.
    file_rate: f64,
    file_rate_prev: Option<(Instant, usize)>,
    browser: Browser,
    shared: Arc<Shared>,
    started: Option<Instant>,
    error: Option<String>,
    dark: bool,
    logo: Option<egui::TextureHandle>,
    /// Result of a "safely remove" the user asked for on the done screen, kept so its
    /// outcome stays on screen. None until they press the button.
    eject_status: Option<Result<String, String>>,
    /// An eject running on its worker thread. Ejecting flushes — seconds on a slow
    /// drive — and a frozen window at exactly that moment invites the user to yank the
    /// cable, the one event eject exists to prevent. The UI shows "Removing safely…"
    /// and polls this instead.
    eject_busy: Option<Arc<Mutex<Option<Result<String, String>>>>>,
    /// The About panel is a modal overlay, not a screen, so opening it never disturbs a
    /// transfer running underneath — closing it returns to exactly where you were.
    show_about: bool,
    /// Whether a Devanagari fallback font was installed — the About text shows केवट only
    /// when it can actually be drawn, never as tofu.
    deva: bool,
    erase: EraseState,
    /// A Move deletes originals, so the Start button arms first: one click asks for
    /// confirmation, the second commits. Reset whenever the mode or selection changes.
    move_armed: bool,
    /// The removable drive we last auto-filled as the destination. Recorded so that when
    /// exactly one drive is present we fill it in once, but never re-fight the user if they
    /// clear or change it — a second auto-fill only happens for a *different* drive.
    auto_dst: Option<PathBuf>,
    /// Interrupted transfers found on launch — the "continue where you left off?" card.
    /// Loaded once; entries drop off when continued or dismissed. Never deletes a
    /// journal: dismissing hides the offer, the debt stays on disk for next launch.
    pending: Vec<journal::Pending>,
    /// Transfer history, loaded when the History screen is opened (never per frame).
    history: Vec<journal::HistoryEntry>,
    /// True when `dst` came from a journal's session line and is already the *effective*
    /// destination. `start()` must then skip `effective_dst` — re-applying it would nest
    /// a second folder level, change the journal key, and silently re-copy everything.
    /// Every manual destination pick resets this.
    exact_dst: bool,
    /// Second-click arming for the resume card's risky paths (a Move, or a destination
    /// where none of the already-done files can be found).
    pending_armed: bool,
}

impl Ui {
    fn new(dark: bool, deva: bool) -> Self {
        Ui {
            screen: Screen::Pick,
            direction: Direction::ToDrive,
            src: None,
            src_names: Vec::new(),
            on_exists: engine::OnExists::Replace,
            conflicts: 0,
            conflict_key: None,
            awake: None,
            dst: None,
            mode: Mode::Copy,
            verify: false,
            skip_caches: false,
            skip_cloud: false,
            skip_rels: Vec::new(),
            file_rate: 0.0,
            file_rate_prev: None,
            browser: Browser::new(),
            shared: Arc::new(Shared::default()),
            started: None,
            error: None,
            dark,
            logo: None,
            eject_status: None,
            eject_busy: None,
            show_about: false,
            deva,
            erase: EraseState::default(),
            move_armed: false,
            auto_dst: None,
            // One read at launch. A directory of a few journals parses in microseconds,
            // and the card is the first thing a crash-survivor needs to see.
            pending: journal::pending(),
            history: Vec::new(),
            exact_dst: false,
            pending_armed: false,
        }
    }

    /// The scan filter the current checkboxes describe — one builder so the real run
    /// and the conflict pre-count can never disagree about what is in the job.
    fn scan_filter(&self) -> scan::Filter {
        let mut f =
            if self.skip_caches { scan::Filter::caches() } else { scan::Filter::none() };
        if self.skip_cloud {
            for c in scan::CLOUD_PREFIXES {
                f.add_prefix(c);
            }
        }
        f
    }

    /// The field the drive restriction sits on for the current direction.
    fn locked_field(&self) -> Field {
        match self.direction {
            Direction::ToDrive => Field::Dest,
            Direction::FromDrive => Field::Source,
        }
    }

    /// Switch direction, clearing everything the two fields' swapped semantics could
    /// poison: the picked paths (a dest has folder-nesting semantics a source does not),
    /// the auto-fill guard, an armed Move, a journal-sourced exact destination, and any
    /// stale refusal message.
    fn set_direction(&mut self, dir: Direction) {
        if self.direction == dir {
            return;
        }
        self.direction = dir;
        self.src = None;
        self.src_names.clear();
        self.dst = None;
        self.skip_rels.clear();
        self.auto_dst = None;
        self.move_armed = false;
        self.exact_dst = false;
        self.error = None;
        self.browser.open_for = None;
        self.browser.drive_root = None;
        self.browser.locked = self.locked_field();
    }

    fn palette(&self) -> Palette {
        if self.dark {
            theme::DARK
        } else {
            theme::LIGHT
        }
    }

    fn start(&mut self) {
        let (Some(src), Some(picked)) = (self.src.clone(), self.dst.clone()) else {
            return;
        };
        // The picker only offers connected external drives, but the *committed* value can
        // outlive the drive: unplug it after choosing and `picked` names a directory that
        // no longer exists. Without this check, `create_dir_all` would happily recreate
        // that path on the internal disk (on Fedora, `/run/media/<user>` is a user-owned
        // tmpfs — a multi-GB copy into RAM) and the free-space probe would measure the
        // wrong volume. Refuse instead; nothing has been written yet.
        // A resumed destination may not exist yet (power died before its directory was
        // durable) — its *drive* being present is what matters; the engine recreates the
        // directory under the same journal key.
        // Only blame a drive when a drive is actually involved — a deleted internal
        // folder gets its own words.
        let gone = |p: &Path| -> String {
            if removable_root_of(p).is_some() || dest_is_removable(p) {
                "That drive isn't connected any more — plug it back in and try again.".into()
            } else {
                "That folder no longer exists — pick it again.".into()
            }
        };
        // The parent-exists exemption (a resumed job whose destination directory did not
        // survive) is only sound when that parent is itself on a *connected* drive.
        // Otherwise it is a trapdoor: with the stick unplugged, `/run/media/<user>`
        // still exists — a user-owned tmpfs that outlives the unmount — so the check
        // passed, the engine recreated the path there, and a multi-GB transfer went into
        // RAM. In move mode it then verified against that copy and deleted the originals.
        // `/Volumes` on macOS has the same shape.
        let parent_ok = picked.parent().is_some_and(|par| {
            par.exists()
                && (self.direction == Direction::FromDrive || removable_root_of(par).is_some())
        });
        let present = picked.exists() || (self.exact_dst && parent_ok);
        if !present {
            self.error = Some(gone(&picked));
            return;
        }
        // Defence in depth against the same race: an eject worker may still be
        // unmounting the very drive this transfer would write to.
        if self.eject_busy.is_some() {
            self.error = Some("The drive is still being removed — wait a moment.".into());
            return;
        }
        // The mirror case: in reverse mode the *source* is the drive, and it can vanish
        // between picking and pressing Start just the same. Refuse with the same calm
        // message instead of the scanner's rawer "cannot read source".
        if !src.exists() {
            self.error = Some(gone(&src));
            return;
        }
        // A folder is copied *into* the destination as a named folder, the way every file
        // manager behaves — otherwise "Photos" would spill 800 loose files into the drive's
        // root. Deterministic, so re-running (Continue) lands on the same place and resumes.
        // A journal-sourced destination is already effective (A-grade trap: re-applying
        // would nest "Photos/Photos" and mint a fresh journal key = silent full re-copy).
        // A multi-selection lands each chosen item under its own name directly in the
        // destination — wrapping them in a folder named after the folder they happened
        // to be sitting in would be nobody's intent.
        let names = self.src_names.clone();
        let multi = !names.is_empty();
        let dst = if self.exact_dst || multi {
            picked.clone()
        } else {
            effective_dst(&src, &picked)
        };
        let shared = Arc::new(Shared::default());
        self.shared = shared.clone();
        self.started = Some(Instant::now());
        self.error = None;
        self.eject_status = None;
        self.awake = Some(StayAwake::new());
        self.screen = Screen::Running;
        self.file_rate = 0.0;
        self.file_rate_prev = None;
        let filter = self.scan_filter();
        // One-shot by construction: giving up on files is a per-run choice.
        let skip_rels = std::mem::take(&mut self.skip_rels);

        let opts = Options {
            mode: self.mode,
            // Verify defaults ON for move (set when the mode is chosen) but can be
            // turned off — the two-phase rule still holds either way: originals are
            // deleted only after every file is copied and one durability barrier has
            // been forced over the whole destination. A resume across a crash/power
            // cut always forces the check (ARCHITECTURE D2): the files written this
            // run are re-read, cheaply.
            on_exists: self.on_exists,
            verify: self.verify || self.exact_dst,
            // Two selections made in the same folder share that folder as their source
            // path, so the selection itself must enter the journal key or they would
            // resume each other's work.
            job_tag: if multi {
                let mut key = String::new();
                for n in &names {
                    key.push_str(&n.to_string_lossy());
                    key.push('\u{0}');
                }
                Some(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(key.as_bytes())))
            } else {
                None
            },
            paranoid: false,
            dry_run: false,
            selection: names.iter().map(|n| n.to_string_lossy().into_owned()).collect(),
            skip_caches: self.skip_caches,
            skip_cloud: self.skip_cloud,
        };

        std::thread::spawn(move || {
            let mut manifest = match if multi {
                scan::scan_selected_with(&src, &names, &filter)
            } else {
                scan::scan_with(&src, &filter)
            } {
                Ok(m) => m,
                Err(e) => {
                    *shared.outcome.lock().unwrap() = Some(Err(format!("cannot read source: {e}")));
                    shared.finished.store(true, Ordering::Release);
                    return;
                }
            };
            // "Finish without the unreadable files": the files chosen on the error
            // screen leave the job before any totals are computed, so this run can
            // genuinely complete. Reported below like every other omission.
            let mut gave_up: Vec<PathBuf> = Vec::new();
            if !skip_rels.is_empty() {
                let skip: std::collections::HashSet<&PathBuf> = skip_rels.iter().collect();
                manifest.files.retain(|e| {
                    let keep = !skip.contains(&e.rel);
                    if !keep {
                        gave_up.push(e.rel.clone());
                    }
                    keep
                });
                manifest.total_bytes = manifest.files.iter().map(|e| e.size).sum();
            }
            shared
                .files_total
                .store(manifest.file_count(), Ordering::Relaxed);
            shared.bytes_total.store(manifest.total_bytes, Ordering::Relaxed);
            // When app caches were NOT left out, count how many manifest files a cache
            // filter would have pruned — the running screen turns a big number into a
            // "stop, tick the box, start again" hint, and resume makes that flow free.
            if filter.is_empty() {
                let would_skip = scan::Filter::caches();
                // For a multi-selection, skip the first component: it is a root the
                // user chose by name, which the filter is defined never to prune —
                // counting it promised savings the checkbox cannot deliver.
                let n = manifest
                    .files
                    .iter()
                    .filter(|e| {
                        if multi {
                            would_skip
                                .matches_path(&e.rel.components().skip(1).collect::<PathBuf>())
                        } else {
                            would_skip.matches_path(&e.rel)
                        }
                    })
                    .count();
                shared.cache_files.store(n, Ordering::Relaxed);
            }
            let mut noted = manifest.skipped.clone();
            for p in &manifest.excluded {
                noted.push((p.clone(), "left out, as asked".into()));
            }
            for p in &gave_up {
                noted.push((p.clone(), "couldn't be read last time — left out, as asked".into()));
            }
            if !noted.is_empty() {
                if let Ok(mut sk) = shared.scan_skipped.lock() {
                    *sk = noted;
                }
            }
            // Everything skipped means nothing was copied — never report that as done,
            // the same refusal the CLI makes.
            if manifest.file_count() == 0 && !manifest.skipped.is_empty() {
                *shared.outcome.lock().unwrap() = Some(Err(
                    "nothing was copied — every entry was skipped (symlinks or unreadable files)"
                        .to_string(),
                ));
                shared.finished.store(true, Ordering::Release);
                return;
            }

            // Refuse a doomed transfer up front. Only when we can actually read the free
            // space *and* it is clearly short — an unreadable figure means proceed, never
            // block on a guess. A move within one filesystem needs no new space, but the
            // GUI cannot copy across into the same tree (check_paths forbids it), so the
            // straightforward size-versus-free comparison is the right one here.
            // Compare what is still to WRITE, not the manifest total: on a resume or a
            // repeat run the files already on the drive are exactly what consumed its
            // free space, so the full total refuses the transfer that is nearly done —
            // and a drive sized to the data is the headline case.
            if let Some(free) = engine::free_space(&dst) {
                let needed = engine::bytes_still_needed(&dst, &manifest);
                if needed > free {
                    *shared.outcome.lock().unwrap() = Some(Err(format!(
                        "{} still to copy — the destination has {} free.",
                        human(needed),
                        human(free)
                    )));
                    shared.finished.store(true, Ordering::Release);
                    return;
                }
            }

            let s = shared.clone();
            let result = engine::run_with(&src, &dst, &manifest, &opts, &shared.cancel, &mut |p| {
                match p {
                    Progress::Start => {}
                    Progress::Dirs { done, total } => {
                        s.dirs_done.store(done, Ordering::Relaxed);
                        s.dirs_total.store(total, Ordering::Relaxed);
                    }
                    Progress::Eta { seconds, small_left, small_secs, big_bytes, big_secs } => {
                        // 0 = unknown; a known sub-minute value stores as 1.
                        let enc = |v: Option<u64>| v.map(|s| s.max(1) as i64).unwrap_or(0);
                        s.eta_secs.store(enc(seconds), Ordering::Relaxed);
                        s.eta_small_left.store(small_left, Ordering::Relaxed);
                        s.eta_small_secs.store(enc(small_secs), Ordering::Relaxed);
                        s.eta_big_bytes.store(big_bytes, Ordering::Relaxed);
                        s.eta_big_secs.store(enc(big_secs), Ordering::Relaxed);
                    }
                    Progress::FileSkipped => {
                        s.files_skipped.fetch_add(1, Ordering::Relaxed);
                    }
                    Progress::Errors { total } => {
                        s.errors.store(total, Ordering::Relaxed);
                    }
                    Progress::File { rel } => {
                        if let Ok(mut c) = s.current.lock() {
                            *c = rel;
                        }
                        s.checking.store(0, Ordering::Relaxed);
                    }
                    Progress::Checking { rel, bytes } => {
                        // Re-verifying an already-written prefix. Shown as its own state
                        // so hours of validating a huge file reads as work, not a hang.
                        if let Ok(mut c) = s.current.lock() {
                            *c = rel;
                        }
                        s.checking.store(bytes.max(1), Ordering::Relaxed);
                    }
                    Progress::Bytes(n) => {
                        // Freshly written: counts toward both the bar and the speed.
                        s.bytes_done.fetch_add(n, Ordering::Relaxed);
                        s.bytes_fresh.fetch_add(n, Ordering::Relaxed);
                    }
                    Progress::Skipped(n) => {
                        // Carried forward: the bar, but never the speed.
                        s.bytes_done.fetch_add(n, Ordering::Relaxed);
                    }
                    Progress::FileDone => {
                        s.files_done.fetch_add(1, Ordering::Relaxed);
                    }
                    Progress::DeletePhase { total } => {
                        // Everything is copied and proven; the originals go now. Its own
                        // state, because unlinks move no bytes — reusing the byte bar
                        // would look stalled at 100%.
                        s.deleting_total.store(total, Ordering::Relaxed);
                        s.deleted.store(0, Ordering::Relaxed);
                    }
                    Progress::Deleted => {
                        s.deleted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });

            *shared.outcome.lock().unwrap() = Some(result.map_err(|e| e.to_string()));
            shared.finished.store(true, Ordering::Release);
        });
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Where a transfer actually lands. A folder source becomes a same-named folder *inside*
/// the chosen destination (file-manager convention); a single file goes to the destination
/// as given. Deterministic so a Continue re-run keys the journal to the same place.
fn effective_dst(src: &Path, picked: &Path) -> PathBuf {
    if src.is_dir() {
        match src.file_name() {
            Some(name) => picked.join(name),
            None => picked.to_path_buf(),
        }
    } else {
        picked.to_path_buf()
    }
}

/// Sizes in decimal units (kB/MB/GB), matching what a drive's label and every file manager
/// say — a "64 GB" stick, not "59.6 GiB". The engine and CLI keep binary units where they
/// belong; this is the friendly front-of-house number.
fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Transfer speed in decimal MB/s, or GB/s once it is fast enough to warrant it.
fn speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1_000_000_000.0 {
        format!("{:.1} GB/s", bytes_per_sec / 1_000_000_000.0)
    } else {
        format!("{:.0} MB/s", bytes_per_sec / 1_000_000.0)
    }
}

/// Open a path in the system file manager. A file opens its containing folder, since you
/// cannot "open" a file into a folder view. Fire-and-forget: the file manager is the
/// user's, and whether it takes focus is the desktop's business, not ours.
fn reveal(path: &Path) {
    let target = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.to_path_buf())
    };
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program).arg(target).spawn();
}

/// Hand a URL to the system's default browser. Kevat itself opens no socket — this spawns
/// the user's browser, exactly as clicking a link in any app does — so the "no network
/// code in the binary" promise is untouched.
fn open_url(url: &str) {
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program).arg(url).spawn();
}

/// The mount point of the filesystem holding `path`: walk up while the device id stays the
/// same, and stop where it changes — that boundary is where the drive is mounted.
#[cfg(unix)]
fn mount_point(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let mut cur = std::fs::canonicalize(path).ok()?;
    if !cur.is_dir() {
        cur = cur.parent()?.to_path_buf();
    }
    let dev = std::fs::metadata(&cur).ok()?.dev();
    loop {
        match cur.parent() {
            Some(parent) => match std::fs::metadata(parent) {
                Ok(m) if m.dev() == dev => cur = parent.to_path_buf(),
                _ => return Some(cur), // parent is a different device, or unreadable
            },
            None => return Some(cur), // reached the root
        }
    }
}

/// The device backing a mount point, from /proc/self/mountinfo. Each line is
/// `… <mountpoint> … - <fstype> <source> <opts>`; we match the mount point and take the
/// source after the ` - ` separator.
#[cfg(target_os = "linux")]
fn device_for_mount(mp: &Path) -> Option<String> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mps = mp.to_string_lossy();
    for line in info.lines() {
        // `?` here would abort the whole scan on one unparsable line; skip it instead.
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let fields: Vec<&str> = left.split_whitespace().collect();
        // Compare unescaped: mountinfo writes a space as \040, so the automount of a
        // drive labelled "My Passport" never matched, the Eject button silently never
        // appeared for it, and those labels are the shipping defaults on WD and Seagate.
        if fields.get(4).map(|m| unescape_mountinfo(m) == mps).unwrap_or(false) {
            return right.split_whitespace().nth(1).map(str::to_string);
        }
    }
    None
}

/// Undo mountinfo's octal escaping of characters that would break its field splitting.
/// Backslash last, so a real backslash is not re-interpreted.
#[cfg(target_os = "linux")]
fn unescape_mountinfo(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn run_ok(program: &str, args: &[&str]) -> Result<(), String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW — a GUI-subsystem app spawning a console tool would otherwise
        // flash a black console window, which is exactly what kevatw.exe exists to avoid.
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    match cmd.output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Keep the machine awake for the duration of a transfer.
///
/// A laptop that suspends fifteen minutes into a three-hour copy is the quietest way
/// for a transfer to "fail": the user comes back to a fraction done and concludes the
/// app is broken. Held as a value — dropping it releases the block — so the wake-lock
/// cannot outlive the screen that owns it.
struct StayAwake {
    #[cfg(not(target_os = "windows"))]
    child: Option<std::process::Child>,
}

impl StayAwake {
    fn new() -> Self {
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::System::Power::SetThreadExecutionState;
            // ES_CONTINUOUS | ES_SYSTEM_REQUIRED: the system may blank the screen but
            // must not sleep. Cleared in Drop.
            unsafe { SetThreadExecutionState(0x8000_0000 | 0x0000_0001) };
            StayAwake {}
        }
        #[cfg(not(target_os = "windows"))]
        {
            // Spawning the OS's own inhibitor keeps the promise of no new dependencies
            // and no D-Bus client in the binary. Failure is silent and harmless: the
            // transfer still runs, the machine may just sleep as it normally would.
            let mut cmd = if cfg!(target_os = "macos") {
                let mut c = std::process::Command::new("caffeinate");
                c.arg("-i"); // prevent idle sleep
                c
            } else {
                let mut c = std::process::Command::new("systemd-inhibit");
                c.args([
                    "--what=idle:sleep",
                    "--who=Kevat",
                    "--why=Copying files to a drive",
                    "--mode=block",
                    "sleep",
                    "infinity",
                ]);
                c
            };
            let child = cmd
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok();
            StayAwake { child }
        }
    }
}

impl Drop for StayAwake {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::System::Power::SetThreadExecutionState;
            unsafe { SetThreadExecutionState(0x8000_0000) }; // ES_CONTINUOUS alone
        }
        #[cfg(not(target_os = "windows"))]
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ── the Windows drive layer ───────────────────────────────────────────────────
//
// Windows has no mountinfo to read; drives are letters. Everything below funnels through
// `win_drives()`, one cached scan shared by the quick-jump list, the destination chooser,
// `dest_is_removable` and the erase screen — so a slow call (an unhappy card reader, a
// sleeping network drive) can cost at most one scan every couple of seconds, never one
// per frame.

/// The volume root holding `path`, e.g. `D:\` — from the path's prefix component, so it
/// works for a path that does not exist yet and for the `\\?\D:\…` verbatim form that
/// `canonicalize` produces.
#[cfg(target_os = "windows")]
fn volume_root(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    let p = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match p.components().next()? {
        Component::Prefix(pre) => match pre.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                Some(PathBuf::from(format!("{}:\\", d.to_ascii_uppercase() as char)))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Whether the volume behind `letter` sits on the USB bus. `GetDriveTypeW` calls a USB
/// hard drive or portable SSD DRIVE_FIXED — the same "medium detection lies" trap as the
/// Linux `removable` flag — so ask the storage stack for the bus type directly. Opening
/// with zero access rights needs no permissions and touches no data.
#[cfg(target_os = "windows")]
fn volume_on_usb(letter: char) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, BusTypeUsb, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceProperty, IOCTL_STORAGE_QUERY_PROPERTY,
        STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    let path: Vec<u16> = format!("\\\\.\\{letter}:").encode_utf16().chain(Some(0)).collect();
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut query: STORAGE_PROPERTY_QUERY = unsafe { std::mem::zeroed() };
    query.PropertyId = StorageDeviceProperty;
    query.QueryType = PropertyStandardQuery;
    let mut desc: STORAGE_DEVICE_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let mut ret: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const _,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            &mut desc as *mut _ as *mut _,
            std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(h) };
    ok != 0 && desc.BusType == BusTypeUsb
}

/// One scan of every drive letter: display label ("USB STICK (D:)"), root path, and
/// whether it is an external drive Kevat may write onto. External means: not the system
/// drive, and either DRIVE_REMOVABLE or a fixed disk on the USB bus.
#[cfg(target_os = "windows")]
fn scan_win_drives() -> Vec<(String, PathBuf, bool)> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    // The drive Windows itself runs from is never offered as external, whatever bus it
    // claims — the same "never the disk backing /" rule as the other platforms.
    let sys_letter = std::env::var("SystemDrive")
        .ok()
        .and_then(|s| s.chars().next())
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('C');
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u8 {
        if mask & (1u32 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i) as char;
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
        let kind = unsafe { GetDriveTypeW(wide.as_ptr()) };
        if !matches!(kind, DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_REMOTE) {
            continue; // CD-ROM, RAM disk, unknown — not a copy target or source root
        }
        // Never query a network drive's volume: a disconnected mapped share makes
        // GetVolumeInformationW block for the whole SMB timeout, and this scan runs on
        // the UI thread every couple of seconds. The bare letter is label enough.
        let vol = if kind == DRIVE_REMOTE {
            String::new()
        } else {
            let mut name = [0u16; 64];
            let ok = unsafe {
                GetVolumeInformationW(
                    wide.as_ptr(),
                    name.as_mut_ptr(),
                    name.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if ok == 0 {
                // Only ERROR_NOT_READY (21) means an empty card-reader slot. Everything
                // else — RAW after an interrupted format, an ext4 stick from a Linux
                // machine, a locked BitLocker volume — is a real drive that must stay
                // visible, not least so the Erase screen can rescue it.
                const ERROR_NOT_READY: u32 = 21;
                if kind == DRIVE_REMOVABLE
                    && unsafe { windows_sys::Win32::Foundation::GetLastError() } == ERROR_NOT_READY
                {
                    continue;
                }
                String::new()
            } else {
                let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                String::from_utf16_lossy(&name[..end])
            }
        };
        let label = if vol.is_empty() { format!("{letter}:") } else { format!("{vol} ({letter}:)") };
        let external = letter != sys_letter
            && match kind {
                DRIVE_REMOVABLE => true,
                DRIVE_FIXED => volume_on_usb(letter),
                _ => false,
            };
        out.push((label, PathBuf::from(root), external));
    }
    out
}

/// `scan_win_drives`, at most once every two seconds. The pick screen polls for a drive
/// being plugged in, and egui repaints on every mouse move — without this cache the bus
/// queries would run per frame.
#[cfg(target_os = "windows")]
fn win_drives() -> Vec<(String, PathBuf, bool)> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, Vec<(String, PathBuf, bool)>)>> = Mutex::new(None);
    // Poison-proof: a panic mid-scan must not turn every later frame into a panic.
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, drives)) = guard.as_ref() {
        if at.elapsed() < Duration::from_secs(2) {
            return drives.clone();
        }
    }
    let drives = scan_win_drives();
    *guard = Some((Instant::now(), drives.clone()));
    drives
}

/// Display name for a drive root: the volume label on Windows ("USB STICK (D:)" — `D:\`
/// has no file_name to show), the mount directory's name elsewhere.
fn drive_label(d: &Path) -> String {
    #[cfg(target_os = "windows")]
    {
        if let Some((label, _, _)) = win_drives().into_iter().find(|(_, root, _)| root == d) {
            return label;
        }
    }
    d.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| d.to_string_lossy().into_owned())
}

/// Quick-jump places for the picker: Home, then every mounted drive. On Linux these are
/// the udisks auto-mount roots (/media, /run/media) plus /mnt; on macOS, /Volumes. Returned
/// as (label, path) so a plugged-in disk is one click away rather than a climb from home.
fn drive_places() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        out.push(("Home".to_string(), PathBuf::from(home)));
    }
    let label = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string_lossy().into_owned())
    };
    #[cfg(target_os = "linux")]
    {
        if let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") {
            let mut seen = std::collections::BTreeSet::new();
            for line in info.lines() {
                let Some((left, _)) = line.split_once(" - ") else {
                    continue;
                };
                // Field 5 (index 4) is the mount point; mountinfo escapes spaces as \040.
                if let Some(raw) = left.split_whitespace().nth(4) {
                    let mp = unescape_mountinfo(raw);
                    if (mp.starts_with("/media/") || mp.starts_with("/run/media/") || mp.starts_with("/mnt/"))
                        && seen.insert(mp.clone())
                    {
                        let path = PathBuf::from(&mp);
                        out.push((label(&path), path));
                    }
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.push((label(&p), p));
                }
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = label; // drive letters have no file_name; win_drives carries the volume label
        for (name, root, _) in win_drives() {
            out.push((name, root));
        }
    }
    out
}

/// Reduce a partition device to its whole-disk name: `sda1` → `sda`, `nvme0n1p5` →
/// `nvme0n1`, `mmcblk0p2` → `mmcblk0`. That whole-disk name is the key under /sys/block.
#[cfg(target_os = "linux")]
fn block_base(dev: &str) -> String {
    let name = dev.rsplit('/').next().unwrap_or(dev);
    if name.starts_with("nvme") || name.starts_with("mmcblk") {
        // These name partitions `…pN`; strip a trailing `p<digits>`.
        if let Some(idx) = name.rfind('p') {
            if idx + 1 < name.len() && name[idx + 1..].bytes().all(|b| b.is_ascii_digit()) {
                return name[..idx].to_string();
            }
        }
        name.to_string()
    } else {
        name.trim_end_matches(|c: char| c.is_ascii_digit()).to_string()
    }
}

/// Whether it is safe and sensible to offer "Eject" for the drive holding `dst`: a
/// removable device that is not the one backing `/`. Without this the eject button would,
/// on an ordinary internal-disk destination, ask the OS to unmount the system disk — which
/// pops an admin-auth prompt and is never what the user wanted. Better to hide the button.
fn dest_is_removable(dst: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let mp = match mount_point(dst) {
            Some(m) => m,
            None => return false,
        };
        let dev = match device_for_mount(&mp) {
            Some(d) => d,
            None => return false,
        };
        let base = block_base(&dev);
        // Never the disk that carries the running system.
        if let Some(root) = device_for_mount(Path::new("/")) {
            if block_base(&root) == base {
                return false;
            }
        }
        // Two signals, either is enough. The `removable` flag catches classic USB sticks
        // and card readers, but USB hard drives and SSDs (and USB-SATA/UASP bridges)
        // routinely report removable=0 — so also treat anything sitting on the USB bus as
        // removable. `/sys/block/<base>` is a symlink into the device tree; if the resolved
        // path runs through a `usb` node, the drive is external. This is what makes an
        // ordinary portable SSD show up at all.
        let flag = std::fs::read_to_string(format!("/sys/block/{base}/removable"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        let on_usb = std::fs::canonicalize(format!("/sys/block/{base}"))
            .map(|p| p.to_string_lossy().contains("/usb"))
            .unwrap_or(false);
        flag || on_usb
    }
    #[cfg(target_os = "macos")]
    {
        // Being under /Volumes is not enough: macOS mounts *every* non-boot volume
        // there, internal ones included — a Boot Camp partition or a second internal
        // APFS volume. Treating those as removable offered them on the Erase screen,
        // whose contract is "only removable drives are ever listed". Ask diskutil.
        let Some(mp) = mount_point(dst) else { return false };
        if !mp.starts_with("/Volumes") {
            return false;
        }
        mac_volume_is_external(&mp)
    }
    #[cfg(target_os = "windows")]
    {
        // The scan already decided which letters are external (removable, or fixed on
        // the USB bus, never the system drive) — just resolve dst to its root and look
        // it up.
        match volume_root(dst) {
            Some(root) => win_drives().into_iter().any(|(_, r, ext)| ext && r == root),
            None => false,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = dst;
        false
    }
}

/// Whether the volume mounted at `mp` is genuinely external, per `diskutil`. Cached for
/// two seconds per mount point: this is called from `dest_is_removable`, which the pick
/// screen runs twice a frame per drive, and egui repaints on every mouse move — without
/// the cache that is one subprocess spawn per volume per frame, i.e. a frozen window.
/// The Windows scan makes exactly the same trade for exactly the same reason.
#[cfg(target_os = "macos")]
fn mac_volume_is_external(mp: &Path) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: Mutex<Option<HashMap<PathBuf, (Instant, bool)>>> = Mutex::new(None);
    // Poison-proof: a panic while probing must not turn every later frame into a panic.
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some((at, v)) = map.get(mp) {
        if at.elapsed() < Duration::from_secs(2) {
            return *v;
        }
    }
    let probe = || -> bool {
        let Some(p) = mp.to_str() else { return false };
        let Ok(out) = std::process::Command::new("diskutil").args(["info", p]).output() else {
            return false; // unknown answers no — never offer a disk we cannot vouch for
        };
        if !out.status.success() {
            return false;
        }
        let info = String::from_utf8_lossy(&out.stdout);
        info.lines().any(|l| {
            let l = l.trim();
            (l.starts_with("Device Location:") && l.contains("External"))
                || (l.starts_with("Removable Media:") && l.contains("Removable"))
        })
    };
    let v = probe();
    map.insert(mp.to_path_buf(), (Instant::now(), v));
    v
}

/// The mount roots of every connected external drive, deduplicated — the only places a
/// destination may live. Built from the same mount enumeration as the quick-jump list, minus
/// Home, keeping only what `dest_is_removable` accepts (so never the system disk).
fn removable_drives() -> Vec<PathBuf> {
    drive_places()
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| dest_is_removable(path))
        .collect()
}

/// If `path` sits inside a connected external drive, the root of that drive; otherwise None.
/// Used to keep destination browsing inside the drive the user is on — "Up" stops at the
/// drive root instead of escaping into the internal filesystem.
fn removable_root_of(path: &Path) -> Option<PathBuf> {
    removable_drives()
        .into_iter()
        .find(|root| path == root || path.starts_with(root))
}

/// Best-effort "safely remove" of the drive holding `dst`. Honest by construction: it
/// reports exactly what the OS tools said, so a non-removable disk (which refuses to power
/// off) produces a clear message rather than a false "done".
fn eject(dst: &Path) -> Result<String, String> {
    // `mount_point` is unix-only, so it is referenced only inside the unix branches —
    // otherwise a Windows build fails to find it (E0425).
    #[cfg(target_os = "linux")]
    {
        let mp =
            mount_point(dst).ok_or_else(|| "could not find the drive's mount point".to_string())?;
        let dev = device_for_mount(&mp)
            .ok_or_else(|| "could not find the device for this drive".to_string())?;
        let base = block_base(&dev);
        // Unmount EVERY mounted partition of this physical drive, not just the one we
        // wrote to. `udisksctl power-off` acts on the whole device; powering it off
        // while a sibling partition is still mounted dirty would lose that partition's
        // data — the one sin this feature exists to prevent.
        let mounts = mounts_on_drive(&base);
        let mut mounts = mounts;
        if mounts.is_empty() {
            mounts.push((dev.clone(), mp.to_string_lossy().into_owned()));
        }
        for (d, mpoint) in &mounts {
            if let Err(e) = run_ok("udisksctl", &["unmount", "-b", d]) {
                // Already unmounted (a device listed under several mountinfo lines) is
                // the outcome we wanted, not a failure.
                if e.contains("NotMounted") || e.contains("not mounted") {
                    continue;
                }
                return Err(map_eject_err_linux(&e, mpoint));
            }
        }
        // Powering the drive off is the extra courtesy that makes the LED go dark; not
        // every bus supports it. But a failure can equally mean "something on this drive
        // is still mounted", and reporting *that* as flushed-and-safe is the one lie
        // this feature must never tell — so before claiming anything, re-scan and
        // require the drive to be genuinely clear.
        let powered = std::process::Command::new("udisksctl")
            .args(["power-off", "-b", &dev])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if powered {
            return Ok("Powered off — unplug it.".to_string());
        }
        let still = mounts_on_drive(&base);
        if let Some((_, mpoint)) = still.first() {
            return Err(format!(
                "Another part of this drive ({mpoint}) is still in use — it was not removed."
            ));
        }
        Ok("Unmounted and flushed — you can unplug it.".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        // diskutil eject on a mount point resolves the containing whole disk, unmounts
        // every volume on it (including the rest of an APFS container) and ejects the
        // disk — the correct whole-device semantics, so success genuinely means safe.
        let mp =
            mount_point(dst).ok_or_else(|| "could not find the drive's mount point".to_string())?;
        let p = mp.to_str().ok_or_else(|| "bad mount path".to_string())?;
        run_ok("diskutil", &["eject", p]).map_err(|e| {
            if e.contains("dissented") || e.contains("busy") {
                "Something still has the drive open — close Finder windows showing it \
                 and try again."
                    .to_string()
            } else {
                "macOS declined to release the drive. Your files are copied; eject it \
                 from Finder."
                    .to_string()
            }
        })?;
        Ok("Ejected — safe to unplug.".to_string())
    }
    #[cfg(target_os = "windows")]
    {
        // PnP eject — what the taskbar's own "Safely Remove Hardware" drives. It works
        // for standard users (the old lock→dismount→eject ioctl chain needed a
        // write-access volume open, which Windows denies unelevated for fixed-class USB
        // sticks — exactly the drives our scan admits), it flushes and dismounts every
        // volume on the device itself, and on refusal it names the vetoing program, so
        // every failure message below states a fact the PnP manager reported, not a
        // guess.
        eject_windows_pnp(dst)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = dst;
        Err("Safe-remove isn't wired up on this platform yet.".to_string())
    }
}

/// Every mount that lives on the physical drive whose whole-disk name is `base`, as
/// `(device, mountpoint)`.
///
/// Plain partitions match by name (`sdb1` → `sdb`). Encrypted and LVM volumes do not:
/// they are mounted from `/dev/mapper/<name>`, whose name says nothing about the disk
/// underneath — so those are matched by walking `/sys/block/<dm>/slaves` down to the
/// real partitions. Missing them was a data-loss bug: the sibling stayed mounted with
/// dirty pages while the drive was declared safe to unplug.
#[cfg(target_os = "linux")]
fn mounts_on_drive(base: &str) -> Vec<(String, String)> {
    // Does this device sit on `base`, directly or through any depth of dm mapping?
    fn on_disk(dev: &str, base: &str, depth: u8) -> bool {
        if block_base(dev) == base {
            return true;
        }
        if depth == 0 {
            return false;
        }
        // Resolve /dev/mapper/<name> to its dm-N kernel name, then follow its slaves.
        let name = std::fs::canonicalize(dev)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| dev.rsplit('/').next().unwrap_or(dev).to_string());
        let slaves = format!("/sys/block/{name}/slaves");
        let Ok(rd) = std::fs::read_dir(slaves) else { return false };
        rd.flatten().any(|e| {
            let child = e.file_name().to_string_lossy().into_owned();
            on_disk(&format!("/dev/{child}"), base, depth - 1)
        })
    }
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for line in info.lines() {
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let Some(mp_raw) = left.split_whitespace().nth(4) else { continue };
        let Some(d) = right.split_whitespace().nth(1) else { continue };
        if !d.starts_with("/dev/") || !on_disk(d, base, 4) {
            continue;
        }
        let mount = unescape_mountinfo(mp_raw);
        // One device can appear on several lines (bind mounts); unmounting by device
        // once is enough, and a second attempt would fail and be misreported.
        if !out.iter().any(|(dev, _)| dev == d) {
            out.push((d.to_string(), mount));
        }
    }
    out
}

/// Turn udisksctl's stderr into a sentence that names the actual cause. Raw tool output
/// never reaches the surface: it leaks internals, and the common failures have better
/// words.
#[cfg(target_os = "linux")]
fn map_eject_err_linux(e: &str, mountpoint: &str) -> String {
    if e.contains("No such file or directory") {
        // udisksctl itself is missing — servers, minimal installs.
        "This system has no removal service (udisks). Your files are copied; unmount \
         the drive from your desktop or with umount."
            .to_string()
    } else if e.contains("busy") || e.contains("Error.DeviceBusy") {
        "Something still has the drive open — maybe the folder window you just opened. \
         Close it and try again."
            .to_string()
    } else if e.contains("Not authorized") || e.contains("polkit") || e.contains("Error.NotAuthorized") {
        "The system would not let this session remove the drive.".to_string()
    } else if e.is_empty() {
        format!("could not unmount {mountpoint}")
    } else {
        // Unknown cause: say what happened, and claim nothing about why.
        format!("The drive was not removed ({mountpoint} would not unmount).")
    }
}

/// The full Windows PnP route: volume letter → its disk's device number → the matching
/// disk devnode (SetupDi enumeration) → the nearest removable ancestor (capability
/// walk, at most 3 hops so it can never climb past the device to a hub) →
/// CM_Request_Device_EjectW with a veto buffer.
#[cfg(target_os = "windows")]
fn eject_windows_pnp(dst: &Path) -> Result<String, String> {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_Registry_PropertyW, CM_Get_Parent, CM_Request_Device_EjectW,
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW, SP_DEVICE_INTERFACE_DATA,
        SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA, CM_DRP_CAPABILITIES, CR_SUCCESS,
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    // GUID_DEVINTERFACE_DISK lives in Ioctl, not DeviceAndDriverInstallation.
    use windows_sys::Win32::System::Ioctl::{
        GUID_DEVINTERFACE_DISK, IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const CM_DEVCAP_REMOVABLE: u32 = 4;
    const CR_REMOVE_VETOED: u32 = 23;
    const FILE_DEVICE_DISK: u32 = 7;

    // Device number of the disk behind the volume. A 0-access open grants only
    // attribute access and works for standard users on every drive class; the ioctl is
    // FILE_ANY_ACCESS, so that handle suffices.
    let device_number = |wpath: &[u16]| -> Option<STORAGE_DEVICE_NUMBER> {
        let h = unsafe {
            CreateFileW(
                wpath.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut sdn: STORAGE_DEVICE_NUMBER = unsafe { std::mem::zeroed() };
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                h,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                std::ptr::null(),
                0,
                &mut sdn as *mut _ as *mut _,
                std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        unsafe { CloseHandle(h) };
        if ok != 0 { Some(sdn) } else { None }
    };

    let root = volume_root(dst).ok_or_else(|| "could not find the drive".to_string())?;
    let letter = root
        .to_string_lossy()
        .chars()
        .next()
        .ok_or_else(|| "could not find the drive".to_string())?;
    // A volume can be mounted into a folder on ANOTHER volume and have no letter of its
    // own. The letter prefix would then name the host drive, and ejecting that while the
    // real destination stayed mounted would be a false "safe to unplug". Ask Windows for
    // the path's actual mount root and refuse unless it is the bare letter root.
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
        let wide: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = [0u16; 260];
        let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if ok != 0 {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
            let mount_root = String::from_utf16_lossy(&buf[..end]);
            let expected = format!("{letter}:\\");
            if !mount_root.eq_ignore_ascii_case(&expected) {
                return Err(
                    "this destination is a folder-mounted volume — eject it from Windows itself."
                        .to_string(),
                );
            }
        }
    }
    let vol_path: Vec<u16> = format!("\\\\.\\{letter}:").encode_utf16().chain(Some(0)).collect();
    let Some(vol) = device_number(&vol_path) else {
        return Err("couldn't reach the drive — it may already be unplugged.".to_string());
    };
    if vol.DeviceType != FILE_DEVICE_DISK {
        // Spanned/dynamic volumes fail here; never guess a disk to eject.
        return Err("this volume spans more than one disk — eject it from Windows itself.".to_string());
    }

    // Find the disk devnode with the same device number among all present disks.
    let hdev = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_DISK,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    // HDEVINFO is an isize in windows-sys; its invalid value is -1, not the
    // pointer-typed INVALID_HANDLE_VALUE.
    if hdev == -1 {
        return Err("Windows declined to release the drive. Your files are copied; use \
                    the Safely Remove icon in the taskbar."
            .to_string());
    }
    let mut devinst: Option<u32> = None;
    let mut index: u32 = 0;
    loop {
        let mut ifd: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
        ifd.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        let ok = unsafe {
            SetupDiEnumDeviceInterfaces(hdev, std::ptr::null(), &GUID_DEVINTERFACE_DISK, index, &mut ifd)
        };
        if ok == 0 {
            break;
        }
        index += 1;
        // Fixed header + generous path tail. cbSize must be the STRUCT's fixed size
        // (8 on x64), never the allocation size — the classic invalid-user-buffer trap.
        let mut buf = [0u8; 8 + 512 * 2];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        unsafe {
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        }
        let mut devinfo: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
        devinfo.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        let ok = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                hdev,
                &ifd,
                detail,
                buf.len() as u32,
                std::ptr::null_mut(),
                &mut devinfo,
            )
        };
        if ok == 0 {
            continue;
        }
        let path_ptr = unsafe { std::ptr::addr_of!((*detail).DevicePath) as *const u16 };
        let mut wpath: Vec<u16> = Vec::with_capacity(512);
        for i in 0..512 {
            let c = unsafe { *path_ptr.add(i) };
            wpath.push(c);
            if c == 0 {
                break;
            }
        }
        if let Some(d) = device_number(&wpath) {
            if d.DeviceNumber == vol.DeviceNumber && d.DeviceType == FILE_DEVICE_DISK {
                devinst = Some(devinfo.DevInst);
                break;
            }
        }
    }
    unsafe { SetupDiDestroyDeviceInfoList(hdev) };
    let Some(mut target) = devinst else {
        return Err("Windows declined to release the drive. Your files are copied; use \
                    the Safely Remove icon in the taskbar."
            .to_string());
    };

    // The disk devnode (USBSTOR\Disk…) usually isn't the removable node — that's its
    // parent (the USB device), and for a composite card reader it is the composite
    // parent above that. Walk up to the first node with the removable capability, at
    // most 3 hops. The hop limit alone is NOT enough to stay below a hub: for a plain
    // (non-composite) stick the third node IS the hub, and an external hub's own devnode
    // reports removable — ejecting it would surprise-remove every other device plugged
    // into it, possibly mid-write, and we would call that success. So refuse to crown
    // any node whose driver service is a USB hub.
    const CM_DRP_SERVICE: u32 = 5;
    let service_of = |node: u32| -> String {
        let mut buf = [0u16; 64];
        let mut len: u32 = (buf.len() * 2) as u32;
        let mut regtype: u32 = 0;
        let cr = unsafe {
            CM_Get_DevNode_Registry_PropertyW(
                node,
                CM_DRP_SERVICE,
                &mut regtype,
                buf.as_mut_ptr() as *mut _,
                &mut len,
                0,
            )
        };
        if cr != CR_SUCCESS {
            return String::new();
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
        String::from_utf16_lossy(&buf[..end]).to_ascii_uppercase()
    };
    let mut node = target;
    for hop in 0..3 {
        if hop > 0 {
            let svc = service_of(node);
            if svc.starts_with("USBHUB") {
                break; // a hub is never ours to eject
            }
        }
        let mut caps: u32 = 0;
        let mut len: u32 = 4;
        let mut regtype: u32 = 0;
        let cr = unsafe {
            CM_Get_DevNode_Registry_PropertyW(
                node,
                CM_DRP_CAPABILITIES,
                &mut regtype,
                &mut caps as *mut u32 as *mut _,
                &mut len,
                0,
            )
        };
        if cr == CR_SUCCESS && caps & CM_DEVCAP_REMOVABLE != 0 {
            target = node;
            break;
        }
        let mut parent: u32 = 0;
        if unsafe { CM_Get_Parent(&mut parent, node, 0) } != CR_SUCCESS {
            break;
        }
        node = parent;
    }

    // The eject. PnP query-remove flushes and dismounts every volume on the device
    // itself; a veto is a fact with a name, not a guess. One retry covers a close that
    // was mid-flight (the shell UI does the same).
    let request = || -> (u32, i32, String) {
        let mut veto_type: i32 = 0;
        let mut veto_name = [0u16; 260];
        let cr = unsafe {
            CM_Request_Device_EjectW(target, &mut veto_type, veto_name.as_mut_ptr(), 260, 0)
        };
        // A full 260-char buffer may carry no NUL; keep the truncated name rather
        // than falling back to the vaguer message that names nobody.
        let end = veto_name.iter().position(|&c| c == 0).unwrap_or(veto_name.len());
        (cr, veto_type, String::from_utf16_lossy(&veto_name[..end]))
    };
    let (mut cr, mut veto_type, mut veto_name) = request();
    const PNP_VETO_PENDING_CLOSE: i32 = 2;
    if cr == CR_REMOVE_VETOED && veto_type == PNP_VETO_PENDING_CLOSE {
        std::thread::sleep(Duration::from_millis(400));
        (cr, veto_type, veto_name) = request();
    }

    const PNP_VETO_WINDOWS_APP: i32 = 3;
    const PNP_VETO_WINDOWS_SERVICE: i32 = 4;
    const PNP_VETO_OUTSTANDING_OPEN: i32 = 5;
    const PNP_VETO_INSUFFICIENT_RIGHTS: i32 = 12;
    const PNP_VETO_ALREADY_REMOVED: i32 = 13;
    if cr == CR_SUCCESS {
        return Ok("Ejected — safe to unplug.".to_string());
    }
    if cr == CR_REMOVE_VETOED {
        return match veto_type {
            PNP_VETO_ALREADY_REMOVED => Ok("The drive is already ejected — safe to unplug.".to_string()),
            PNP_VETO_OUTSTANDING_OPEN | PNP_VETO_WINDOWS_APP | PNP_VETO_WINDOWS_SERVICE => {
                Err(if veto_name.is_empty() {
                    "Windows says a program still has the drive open. Close windows \
                     showing the drive, then try again."
                        .to_string()
                } else {
                    format!(
                        "Windows says {veto_name} still has the drive open — close it \
                         and try again."
                    )
                })
            }
            PNP_VETO_INSUFFICIENT_RIGHTS => Err(
                "Windows would not let Kevat request the removal. Use the Safely Remove \
                 icon in the taskbar."
                    .to_string(),
            ),
            _ => Err(
                "Windows declined to release the drive. Your files are copied; use the \
                 Safely Remove icon in the taskbar."
                    .to_string(),
            ),
        };
    }
    Err("Windows declined to release the drive. Your files are copied; use the Safely \
         Remove icon in the taskbar."
        .to_string())
}

// ── formatting an external drive (heavily guarded) ───────────────────────────
//
// This erases a drive, which is the most destructive thing the app can do — the exact
// opposite of Kevat's usual promise. Every guard here exists to make an accident hard:
// only removable drives are ever listed, never the disk backing the running system; the
// user must type the drive's name to arm the button; and the format runs through the OS's
// own privileged tool (pkexec/diskutil/format), so it goes through the system's auth.

/// A filesystem the user can format to. The set offered is filtered per platform to what
/// can actually be created there — ext4 on Linux only, and only if its mkfs tool is present.
#[derive(Clone, Copy, PartialEq)]
enum Fs {
    ExFat,
    Fat32,
    // Only `available()` on Linux ever constructs this — the other platforms cannot
    // create ext4, so there it exists purely as a match arm.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Ext4,
    Ntfs,
}

impl Fs {
    fn label(self) -> &'static str {
        match self {
            Fs::ExFat => "exFAT — best for USB drives, any OS, no size limit",
            Fs::Fat32 => "FAT32 — universal, but no single file over 4 GB",
            Fs::Ext4 => "ext4 — Linux native",
            Fs::Ntfs => "NTFS — Windows native",
        }
    }
    fn short(self) -> &'static str {
        match self {
            Fs::ExFat => "exFAT",
            Fs::Fat32 => "FAT32",
            Fs::Ext4 => "ext4",
            Fs::Ntfs => "NTFS",
        }
    }

    /// The filesystems offerable on this platform, in a sensible default order. On Linux
    /// each is included only if its `mkfs` tool is actually installed, so the menu never
    /// promises a format it cannot perform.
    fn available() -> Vec<Fs> {
        #[cfg(target_os = "linux")]
        {
            let has = |tool: &str| {
                std::env::var_os("PATH").is_some_and(|paths| {
                    std::env::split_paths(&paths).any(|d| {
                        d.join(tool).is_file()
                            || Path::new("/usr/sbin").join(tool).is_file()
                            || Path::new("/sbin").join(tool).is_file()
                    })
                })
            };
            let mut v = Vec::new();
            if has("mkfs.exfat") {
                v.push(Fs::ExFat);
            }
            if has("mkfs.vfat") {
                v.push(Fs::Fat32);
            }
            if has("mkfs.ext4") {
                v.push(Fs::Ext4);
            }
            if has("mkfs.ntfs") {
                v.push(Fs::Ntfs);
            }
            v
        }
        #[cfg(target_os = "macos")]
        {
            // diskutil speaks these; it has no ext4.
            vec![Fs::ExFat, Fs::Fat32, Fs::Ntfs]
        }
        #[cfg(target_os = "windows")]
        {
            // The `format` command; no ext4.
            vec![Fs::ExFat, Fs::Fat32, Fs::Ntfs]
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            Vec::new()
        }
    }
}

/// A removable drive the user could erase.
#[derive(Clone)]
struct DriveInfo {
    /// Used by the macOS formatter (diskutil takes the mount path); on Linux the device
    /// node is what mkfs needs, so the field is read only under cfg(macos).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    mount: PathBuf,
    device: String,
    label: String,
    size: u64,
}

/// Total size of the filesystem mounted at `mount`.
#[cfg(unix)]
fn drive_total(mount: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(mount.as_os_str().as_bytes()) else {
        return 0;
    };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return 0;
    }
    s.f_blocks as u64 * s.f_frsize as u64
}
#[cfg(windows)]
fn drive_total(mount: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = mount.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut total: u64 = 0;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            &mut total,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { 0 } else { total }
}
#[cfg(not(any(unix, windows)))]
fn drive_total(_mount: &Path) -> u64 {
    0
}

/// Every mounted removable drive that is safe to offer for erasing — removable, and never
/// the disk backing `/`. The same conservative test the Eject button uses.
fn list_removable_drives() -> Vec<DriveInfo> {
    let mut out: Vec<DriveInfo> = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return out;
        };
        let mut seen = std::collections::BTreeSet::new();
        for line in info.lines() {
            let Some((left, right)) = line.split_once(" - ") else {
                continue;
            };
            let fields: Vec<&str> = left.split_whitespace().collect();
            let Some(mp_raw) = fields.get(4) else { continue };
            let mp = unescape_mountinfo(mp_raw);
            if !(mp.starts_with("/media/") || mp.starts_with("/run/media/") || mp.starts_with("/mnt/")) {
                continue;
            }
            if !seen.insert(mp.clone()) {
                continue;
            }
            let mount = PathBuf::from(&mp);
            if !dest_is_removable(&mount) {
                continue;
            }
            let device = right
                .split_whitespace()
                .nth(1)
                .map(str::to_string)
                .unwrap_or_default();
            let label = mount
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| mp.clone());
            out.push(DriveInfo {
                size: drive_total(&mount),
                mount,
                device,
                label,
            });
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            for e in rd.flatten() {
                let mount = e.path();
                if mount.is_dir() && dest_is_removable(&mount) {
                    let label = mount
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    out.push(DriveInfo {
                        size: drive_total(&mount),
                        device: String::new(),
                        mount,
                        label,
                    });
                }
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        for (display, root, external) in win_drives() {
            if !external {
                continue;
            }
            // `device` carries the bare letter ("D:") for format.com; `label` the plain
            // volume name without the "(D:)" suffix, so it can become the new label.
            let device = root.to_string_lossy().trim_end_matches('\\').to_string();
            let label = display
                .rsplit_once(" (")
                .map(|(name, _)| name.to_string())
                .unwrap_or(display);
            out.push(DriveInfo {
                size: drive_total(&root),
                mount: root,
                device,
                label,
            });
        }
    }
    out
}

/// A FAT label must be uppercase and at most 11 characters; keep it tame everywhere.
/// (macOS formats through diskutil, which sanitises labels itself — hence unused there.)
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn fat_label(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(11)
        .collect::<String>()
        .to_uppercase()
}

/// Erase `drive` and lay down a fresh `fs`. Runs the OS's own privileged formatter, so the
/// destructive step goes through the system's authorization (a polkit/UAC prompt), never a
/// silent super-user action from inside the app.
fn format_drive(drive: &DriveInfo, fs: Fs) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        if drive.device.is_empty() {
            return Err("could not identify the drive's device".to_string());
        }
        // Unmount first; a mounted filesystem cannot be reformatted.
        let _ = std::process::Command::new("udisksctl")
            .args(["unmount", "-b", &drive.device])
            .output();
        let dev = drive.device.as_str();
        let name = fat_label(&drive.label);
        let (tool, args): (&str, Vec<String>) = match fs {
            Fs::ExFat => ("mkfs.exfat", vec!["-L".into(), name, dev.into()]),
            Fs::Fat32 => (
                "mkfs.vfat",
                vec!["-F".into(), "32".into(), "-n".into(), name, dev.into()],
            ),
            Fs::Ext4 => (
                "mkfs.ext4",
                vec!["-F".into(), "-L".into(), drive.label.clone(), dev.into()],
            ),
            Fs::Ntfs => (
                "mkfs.ntfs",
                vec!["-Q".into(), "-L".into(), drive.label.clone(), dev.into()],
            ),
        };
        // pkexec raises the single mkfs call to root through polkit — the user sees the
        // system's own auth dialog, and nothing is erased without it.
        let mut full = vec![tool.to_string()];
        full.extend(args);
        let refs: Vec<&str> = full.iter().map(String::as_str).collect();
        run_ok("pkexec", &refs)
            .map_err(|e| if e.is_empty() { "format failed".into() } else { e })?;
        Ok(format!("Formatted as {} — the drive is ready.", fs.short()))
    }
    #[cfg(target_os = "macos")]
    {
        let mount = drive.mount.to_str().ok_or_else(|| "bad mount path".to_string())?;
        let fmt = match fs {
            Fs::ExFat => "ExFAT",
            Fs::Fat32 => "MS-DOS FAT32",
            Fs::Ntfs => "NTFS",
            Fs::Ext4 => return Err("macOS cannot create ext4".to_string()),
        };
        let name = if drive.label.is_empty() { "KEVAT" } else { &drive.label };
        run_ok("diskutil", &["eraseVolume", fmt, name, mount])?;
        Ok(format!("Formatted as {} — the drive is ready.", fs.short()))
    }
    #[cfg(target_os = "windows")]
    {
        // Format-Volume through an elevated PowerShell — Start-Process -Verb RunAs
        // raises the UAC prompt, so the destructive step goes through the system's own
        // authorization, exactly like pkexec/diskutil on the other platforms.
        // Format-Volume, not format.com: format.com prompts interactively on removable
        // media ("press ENTER when ready") even with /Y, in a console this process
        // cannot reach. Two honesty guards, learned the hard way in review: a UAC
        // *cancel* makes Start-Process raise a non-terminating error, leaving $p null
        // and `exit $p.ExitCode` exiting 0 — a cancelled format must never be reported
        // as done, hence the explicit null checks.
        let fmt = match fs {
            Fs::ExFat => "exFAT",
            Fs::Fat32 => "FAT32",
            Fs::Ntfs => "NTFS",
            Fs::Ext4 => return Err("Windows cannot create ext4".to_string()),
        };
        let letter = drive
            .device
            .chars()
            .next()
            .filter(|c| c.is_ascii_alphabetic())
            .ok_or_else(|| "could not identify the drive letter".to_string())?;
        // Windows reassigns freed letters to the next drive plugged in. The snapshot
        // this screen armed against may be minutes old; formatting by letter without
        // re-checking could erase a drive that was swapped in since. Same label on the
        // same root, or no format.
        let still_there = scan_win_drives().into_iter().any(|(display, root, external)| {
            external
                && root == drive.mount
                && display
                    .rsplit_once(" (")
                    .map(|(name, _)| name)
                    .unwrap_or(&display)
                    == drive.label
        });
        if !still_there {
            return Err("the drive changed since this screen was opened — go back and reselect it".to_string());
        }
        let mut name = fat_label(&drive.label);
        // An unlabelled drive's display name is its bare letter — don't enshrine "D"
        // as a volume label; give it the same default the macOS branch uses.
        if name.is_empty() || drive.label == format!("{letter}:") {
            name = "KEVAT".to_string();
        }
        let script = format!(
            "$p = Start-Process -Verb RunAs -Wait -PassThru -FilePath powershell.exe \
             -ArgumentList '-NoProfile','-Command','Format-Volume -DriveLetter {letter} \
             -FileSystem {fmt} -NewFileSystemLabel {name} -Force -Confirm:$false; \
             exit (1 - [int]$?)'; \
             if ($null -eq $p) {{ exit 1 }}; \
             if ($null -eq $p.ExitCode) {{ exit 1 }}; \
             exit $p.ExitCode"
        );
        run_ok("powershell", &["-NoProfile", "-Command", &script]).map_err(|e| {
            if e.is_empty() {
                "format failed or was cancelled — the drive was not changed".into()
            } else {
                e
            }
        })?;
        Ok(format!("Formatted as {} — the drive is ready.", fs.short()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (drive, fs);
        Err("Formatting isn't wired up on this platform yet.".to_string())
    }
}

/// Format a modification time (unix seconds) as `YYYY-MM-DD HH:MM` in local time for the
/// browser's Date column. Empty for an unknown time.
fn fmt_mtime(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    #[cfg(unix)]
    {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // localtime_r for the user's own clock, not UTC — the date they expect to see.
        if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return String::new();
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        )
    }
    #[cfg(not(unix))]
    {
        // Civil date from days since the epoch (Howard Hinnant's algorithm), in UTC —
        // adequate until the Windows platform layer wires up local time properly.
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, rem / 3600, (rem % 3600) / 60)
    }
}

/// Human time-remaining from bytes left and the current rate. Deliberately coarse — a
/// copy's rate wobbles, so a false-precision "4 m 37 s" would just flicker. Rounded to the
/// unit that matters at that scale, and honest about not knowing until there is a rate.
fn eta(remaining: u64, rate: f64) -> Option<String> {
    if rate <= 0.0 || remaining == 0 {
        return None;
    }
    Some(format!("{} left", eta_words((remaining as f64 / rate).round() as u64)))
}

/// 214000 → "214,000". Counts in the hundreds of thousands are unreadable without the
/// separators, and the small-file breakdown is exactly where such counts appear.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The duration words alone — "less than a minute", "about 4 minutes", "about 2 h 10 m"
/// — so callers can end the sentence their own way ("left", "of small files", …).
fn eta_words(secs: u64) -> String {
    if secs < 60 {
        "less than a minute".to_string()
    } else if secs < 3600 {
        let m = (secs + 30) / 60;
        format!("about {m} minute{}", if m == 1 { "" } else { "s" })
    } else {
        let h = secs / 3600;
        let m = (secs % 3600 + 30) / 60;
        format!("about {h} h {m} m")
    }
}

fn shorten(p: &Path, max: usize) -> String {
    let s = p.to_string_lossy();
    if s.chars().count() <= max {
        return s.into_owned();
    }
    let tail: String = s.chars().rev().take(max - 1).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

// ── the screens ──────────────────────────────────────────────────────────────

fn draw(ctx: &egui::Context, ui_state: &mut Ui) {
    let p = ui_state.palette();

    // Keyboard shortcuts a desktop user expects. Esc backs out of whatever is open;
    // Enter starts a ready transfer from the pick screen.
    let (esc, enter, select_all, back) = ctx.input(|i| {
        (
            i.key_pressed(egui::Key::Escape),
            i.key_pressed(egui::Key::Enter),
            i.modifiers.command && i.key_pressed(egui::Key::A),
            i.key_pressed(egui::Key::Backspace),
        )
    });
    // Ctrl+A selects everything listed; Backspace goes up a level — both reflexes from
    // every file manager, and both only meaningful while the picker is open.
    if let Some(which) = ui_state.browser.open_for {
        if select_all && which == Field::Source {
            ui_state.browser.picked = ui_state
                .browser
                .entries
                .iter()
                .filter_map(|it| it.path.file_name().map(|n| n.to_os_string()))
                .collect();
        }
        if back && ui_state.browser.new_folder.is_none() {
            let locked = ui_state.browser.locked;
            let at_drive_root =
                ui_state.browser.drive_root.as_deref() == Some(ui_state.browser.cwd.as_path());
            if which == locked && at_drive_root {
                ui_state.browser.drive_root = None;
            } else if let Some(parent) = ui_state.browser.cwd.parent().map(|q| q.to_path_buf()) {
                ui_state.browser.go(parent);
            }
        }
    }
    if esc {
        if ui_state.show_about {
            ui_state.show_about = false;
        } else if ui_state.browser.new_folder.is_some() {
            ui_state.browser.new_folder = None;
        } else if !ui_state.browser.picked.is_empty() {
            // Clear the selection before closing the picker — Escape undoes one step,
            // not two.
            ui_state.browser.picked.clear();
            ui_state.browser.anchor = None;
        } else if ui_state.browser.open_for.is_some() {
            ui_state.browser.open_for = None;
        } else if matches!(ui_state.screen, Screen::Erase) {
            ui_state.screen = Screen::Pick;
        } else if ui_state.move_armed {
            ui_state.move_armed = false;
        }
    }
    if enter
        && !ui_state.show_about
        && matches!(ui_state.screen, Screen::Pick)
        && ui_state.browser.open_for.is_none()
        && ui_state.mode == Mode::Copy
        && ui_state.src.is_some()
        && ui_state.dst.is_some()
        && ui_state.src != ui_state.dst
    {
        // Enter only auto-starts a Copy — a Move must go through its explicit confirm.
        ui_state.start();
    }

    // A folder or file dragged onto the window becomes the source — the gesture people try
    // first. It sets the source and leaves the destination to them.
    let dropped: Vec<PathBuf> = ctx.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .filter_map(|f| f.path.clone())
            .collect()
    });
    if let Some(path) = dropped.into_iter().next() {
        if matches!(ui_state.screen, Screen::Pick) {
            // In "From a drive" mode the source is locked to external drives; a dropped
            // internal folder can only be meant as a forward copy's source, so switch
            // (visibly — the toggle moves) rather than accept a source the locked
            // picker itself would refuse.
            if ui_state.direction == Direction::FromDrive && removable_root_of(&path).is_none() {
                ui_state.set_direction(Direction::ToDrive);
            }
            ui_state.src = Some(path);
            ui_state.src_names.clear();
            ui_state.browser.open_for = None;
            ui_state.move_armed = false;
            // A journal-sourced exact destination belonged to the old source; keeping it
            // would spill this new source into the old job's folder.
            ui_state.exact_dst = false;
        }
    }

    egui::CentralPanel::default()
        .frame(egui::Frame::none().fill(p.ground).inner_margin(28.0))
        .show(ctx, |ui| {
            header(ui, ui_state, &p);
            ui.add_space(18.0);
            if ui_state.show_about {
                about(ui, ui_state, &p);
            } else {
                match ui_state.screen {
                    Screen::Pick => pick(ui, ui_state, &p),
                    Screen::Running => running(ui, ui_state, &p),
                    Screen::Done => done(ui, ui_state, &p),
                    Screen::Erase => erase(ui, ui_state, &p),
                    Screen::History => history(ui, ui_state, &p),
                }
            }
        });
}

fn header(ui: &mut egui::Ui, st: &mut Ui, p: &Palette) {
    ui.horizontal(|ui| {
        // Loaded once and cached on the Ui; egui uploads it to the painter as a texture
        // on first use, and the rasterizer samples it like any other.
        if st.logo.is_none() {
            if let Some(img) = icon::header() {
                let colour = egui::ColorImage::from_rgba_unmultiplied(
                    [img.width as usize, img.height as usize],
                    &img.rgba,
                );
                st.logo = Some(ui.ctx().load_texture("kevat-logo", colour, egui::TextureOptions::LINEAR));
            }
        }
        if let Some(tex) = &st.logo {
            ui.add(
                egui::Image::new(egui::load::SizedTexture::new(
                    tex.id(),
                    Vec2::new(26.0, 26.0),
                ))
                // The mark is a filled tile with no alpha, so rounding it here is what
                // stops it reading as a hard block dropped onto the header.
                .rounding(Rounding::same(6.0)),
            );
            ui.add_space(3.0);
        }
        ui.label(
            RichText::new("Kevat")
                .font(FontId::new(21.0, FontFamily::Proportional))
                .strong()
                .color(p.ink),
        );
        // The version sits right on the wordmark, always visible — "which build am I
        // running?" must never require opening a menu, least of all when reporting a bug.
        ui.label(
            RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                .font(FontId::new(12.0, FontFamily::Monospace))
                .color(p.ink_3),
        );
        ui.label(
            RichText::new("every byte reaches the far shore")
                .font(FontId::new(13.0, FontFamily::Proportional))
                .color(p.ink_3),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Sun/moon drawn with the painter, not a glyph — egui's bundled font has no
            // reliable weather symbols, and a hand-drawn mark matches the website's toggle.
            // The icon shows the CURRENT theme: a sun on the light ground, a moon on dark.
            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(26.0, 26.0), egui::Sense::click());
            let hovered = resp.hovered();
            let stroke_col = if hovered { p.teal } else { p.ink_2 };
            let c = rect.center();
            let painter = ui.painter();
            if st.dark {
                // Crescent: a disc, then the ground colour carved out from the side.
                painter.circle_filled(c, 7.0, stroke_col);
                painter.circle_filled(c + Vec2::new(3.2, -2.2), 6.0, p.ground);
            } else {
                painter.circle_filled(c, 4.2, stroke_col);
                for k in 0..8 {
                    let a = std::f32::consts::TAU * (k as f32) / 8.0;
                    let d = Vec2::new(a.cos(), a.sin());
                    painter.line_segment(
                        [c + d * 6.5, c + d * 9.0],
                        Stroke::new(1.6, stroke_col),
                    );
                }
            }
            if resp.clicked() {
                st.dark = !st.dark;
            }

            ui.add_space(12.0);
            // A subtle outlined chip, so these read as controls rather than blending into
            // the tagline text beside them.
            let chip = |ui: &mut egui::Ui, text: &str| {
                ui.add(
                    egui::Button::new(RichText::new(text).size(12.0).color(p.ink_2))
                        .fill(p.surface)
                        .stroke(Stroke::new(1.0, p.line))
                        .rounding(Rounding::same(7.0)),
                )
                .clicked()
            };
            // Toggle: the same control opens and closes the About overlay.
            let about_label = if st.show_about { "Close" } else { "About" };
            if chip(ui, about_label) {
                st.show_about = !st.show_about;
            }
            // Erase sits just left of About. Hidden mid-transfer, where leaving the
            // progress screen for a destructive errand makes no sense — and hidden
            // while an eject worker runs, which the Done screen's own buttons already
            // respect; these chips were the one way around that guard.
            if !matches!(st.screen, Screen::Running) && !st.show_about && st.eject_busy.is_none() {
                ui.add_space(8.0);
                if chip(ui, "Erase drive") {
                    st.erase = EraseState {
                        drives: list_removable_drives(),
                        ..EraseState::default()
                    };
                    st.screen = Screen::Erase;
                }
                ui.add_space(8.0);
                let history_label =
                    if matches!(st.screen, Screen::History) { "Back" } else { "History" };
                if chip(ui, history_label) {
                    if matches!(st.screen, Screen::History) {
                        st.screen = Screen::Pick;
                    } else {
                        st.history = journal::history();
                        st.screen = Screen::History;
                    }
                }
            }
        });
    });
    ui.add_space(14.0);
    let line = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(line, 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, Rounding::ZERO, p.line_soft);
}

/// The About overlay — version and credits, in the spirit of Diskhoji's about box. URLs
/// are shown as plain text, not opened: the binary has no network code and no browser
/// dependency, and printing the address keeps that promise visible.
fn about(ui: &mut egui::Ui, st: &mut Ui, p: &Palette) {
    ui.label(
        RichText::new("Kevat")
            .font(FontId::new(30.0, FontFamily::Proportional))
            .strong()
            .color(p.ink),
    );
    ui.label(
        RichText::new(format!("version {}", env!("CARGO_PKG_VERSION")))
            .font(FontId::new(13.0, FontFamily::Monospace))
            .color(p.ink_3),
    );
    ui.add_space(4.0);
    ui.label(
        RichText::new("A fast, resumable copier and mover for external drives.")
            .font(FontId::new(14.5, FontFamily::Proportional))
            .color(p.ink_2),
    );

    ui.add_space(20.0);
    let key = |ui: &mut egui::Ui, k: &str| {
        ui.label(
            RichText::new(k)
                .font(FontId::new(11.0, FontFamily::Monospace))
                .color(p.ink_3)
                .strong(),
        );
        ui.add_space(4.0);
    };
    // Plain text rows.
    // Plain text row.
    ui.horizontal(|ui| {
        key(ui, "LICENSE");
        ui.label(
            RichText::new("MIT — no network, no telemetry")
                .font(FontId::new(14.0, FontFamily::Proportional))
                .color(p.ink),
        );
    });
    ui.add_space(6.0);
    // Clickable rows — the value opens in the browser, like Diskhoji's about box. The
    // trailing text after a link (the Diskhoji blurb) stays plain.
    for (k, link, url, tail) in [
        ("CREATED BY", "Prateek Singh", "https://theaivibe.org/about", ""),
        ("HOME", "kevat.app", "https://kevat.app", ""),
        (
            "SOURCE",
            "github.com/singhpratech/kevatapp",
            "https://github.com/singhpratech/kevatapp",
            "",
        ),
        (
            "SIBLING",
            "diskhoji.org",
            "https://diskhoji.org",
            " — finds what is eating your disk",
        ),
    ] {
        ui.horizontal(|ui| {
            key(ui, k);
            let hovered = ui
                .add(
                    egui::Button::new(
                        RichText::new(link)
                            .font(FontId::new(14.0, FontFamily::Proportional))
                            .color(p.teal)
                            .underline(),
                    )
                    .frame(false),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if hovered.clicked() {
                open_url(url);
            }
            if !tail.is_empty() {
                ui.label(
                    RichText::new(tail)
                        .font(FontId::new(14.0, FontFamily::Proportional))
                        .color(p.ink),
                );
            }
        });
        ui.add_space(6.0);
    }

    ui.add_space(16.0);
    // With a Devanagari fallback font installed, show the name in its own script and the
    // macron in Rām, matching the website; otherwise degrade to plain Latin rather than
    // draw boxes.
    let lore = if st.deva {
        "Kevat (केवट) is the ferryman who carried Rām (राम) across the river. He does not own the \
         water and he does not hurry it — he simply gets everything to the other side, and nothing \
         is lost on the way."
    } else {
        "Kevat is the ferryman who carried Rām across the river. He does not own the water and he \
         does not hurry it — he simply gets everything to the other side, and nothing is lost on \
         the way."
    };
    ui.label(
        RichText::new(lore)
            .font(FontId::new(13.5, FontFamily::Proportional))
            .color(p.ink_3),
    );

    ui.add_space(22.0);
    if ui
        .add_sized(
            [110.0, 38.0],
            egui::Button::new(RichText::new("Back").size(14.0).color(p.ink))
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.line)),
        )
        .clicked()
    {
        st.show_about = false;
    }
}

/// One labelled folder slot. The label says what it is, the value says what is chosen,
/// and the button says what will happen — the design system's rule that a control says
/// what happens.
fn folder_field(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    value: &Option<PathBuf>,
    which: Field,
    browser: &mut Browser,
    // Non-empty when `value` is the folder holding a multi-selection rather than the
    // thing being copied — the field must then name the items, not their container.
    names: &[PathBuf],
) {
    ui.label(
        RichText::new(label.to_uppercase())
            .font(FontId::new(11.0, FontFamily::Monospace))
            .color(p.ink_3)
            .strong(),
    );
    ui.add_space(5.0);
    ui.horizontal(|ui| {
        let text = match value {
            Some(v) if !names.is_empty() => {
                format!("{} items in {}", names.len(), shorten(v, 34))
            }
            Some(v) => shorten(v, 46),
            None => "nothing chosen yet".to_string(),
        };
        let colour = if value.is_some() { p.ink } else { p.ink_3 };
        egui::Frame::none()
            .fill(p.sunk)
            .stroke(Stroke::new(1.0, p.line))
            .rounding(Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(12.0, 9.0))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width() - 108.0);
                ui.label(
                    RichText::new(text)
                        .font(FontId::new(14.0, FontFamily::Monospace))
                        .color(colour),
                );
            });
        if ui
            .add_sized(
                [96.0, 36.0],
                egui::Button::new(RichText::new("Choose…").size(14.0).color(p.ink))
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.line)),
            )
            .clicked()
        {
            browser.open(which, value.as_deref());
        }
    });
}

/// The "continue where you left off?" card — the visible face of the resume promise.
/// A power cut, a yanked cable and a crash all leave the same thing behind: a journal.
/// Without this card the machinery exists but is invisible, and an interrupted transfer
/// *looks* lost. Continue re-runs the same (src, dst, mode) pair; the engine then
/// re-validates every file the journal claims is done before copying anything new.
fn resume_card(ui: &mut egui::Ui, st: &mut Ui, p: &Palette) {
    let Some(job) = st.pending.first().cloned() else {
        return;
    };
    // Keep watching for the drive: the natural first move after "plug it back in" is
    // plugging it back in, and the button should arm itself when that happens.
    ui.ctx().request_repaint_after(Duration::from_secs(2));
    let src_ok = job.src.exists();
    // The recorded destination may not exist even though the drive is back (power died
    // before the directory itself was durable) — the drive being present is the real
    // requirement; the engine recreates the directory under the same journal key.
    // Same trapdoor as `start()`: an unplugged drive leaves its `/run/media/<user>` or
    // `/Volumes` parent behind, and accepting that would arm Continue for a destination
    // that is really the internal disk. A reverse job's destination is internal by
    // design, so there the plain parent check is correct.
    let dst_ok = job.dst.exists()
        || job.dst.parent().is_some_and(|par| {
            par.exists() && (dest_is_removable(&job.src) || removable_root_of(par).is_some())
        });
    // Same-drive check: journals key on paths, and the OS hands a returning drive's old
    // letter/mount to whatever is plugged in next. The swappable side is whichever one
    // sits on a removable drive.
    //
    // Forward job (drive destination): if NONE of the files the journal says are done
    // can be found at the destination, a different drive is probably wearing the old
    // path — continuing blind would copy everything onto the wrong stick, and a move
    // would then delete the sources.
    //
    // Reverse job (drive source): in copy mode the source keeps its files, so the
    // recorded done rels must still exist there for this to be the same drive. In move
    // mode their absence is *expected* (they were deliberately removed), so identity
    // cannot be proven from here — the two-click arm and the warning text carry that
    // risk instead, and a wrong drive's files simply won't match the journal.
    let same_drive = if dest_is_removable(&job.src) {
        match job.mode {
            Mode::Copy => {
                job.files_done == 0
                    || !job.src.exists()
                    || job.sample.iter().any(|rel| job.src.join(rel).exists())
            }
            Mode::Move => true,
        }
    } else {
        job.files_done == 0
            || !job.dst.exists()
            || job.sample.iter().any(|rel| job.dst.join(rel).exists())
    };
    egui::Frame::none()
        .fill(p.teal_wash)
        .stroke(Stroke::new(1.0, p.teal_bright))
        .rounding(Rounding::same(10.0))
        .inner_margin(egui::Margin::symmetric(16.0, 12.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(match job.mode {
                    Mode::Copy => "Unfinished copy — nothing is lost",
                    Mode::Move => "Unfinished move — nothing is lost",
                })
                .font(FontId::new(15.0, FontFamily::Proportional))
                .strong()
                .color(p.ink),
            );
            ui.add_space(2.0);
            ui.label(
                RichText::new(format!(
                    "{}  →  {}   ·   {} of {} files finished ({} of {})",
                    job.src.display(),
                    job.dst.display(),
                    job.files_done,
                    job.files_total,
                    human(job.bytes_done),
                    human(job.bytes_total),
                ))
                .font(FontId::new(12.5, FontFamily::Proportional))
                .color(p.ink_2),
            );
            ui.add_space(2.0);
            ui.label(
                RichText::new(
                    "Continue checks everything already on the drive before copying the rest.",
                )
                .font(FontId::new(12.0, FontFamily::Proportional))
                .color(p.ink_3),
            );
            if !dst_ok || !src_ok {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(if !dst_ok {
                        format!(
                            "Plug in the drive this was going to ({}) and this will light up. \
                             If it comes back under a different letter or name, pick the source \
                             and drive again — the copy restarts, but nothing is lost.",
                            job.dst.display()
                        )
                    } else {
                        "The source folder isn't reachable right now.".to_string()
                    })
                    .font(FontId::new(12.5, FontFamily::Proportional))
                    .color(p.amber),
                );
            }
            // The letter is present but holds none of the recorded files: a different
            // drive is probably wearing the old path.
            let wrong_drive_move = !same_drive && job.mode == Mode::Move;
            if !same_drive && dst_ok {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(if wrong_drive_move {
                        "None of the files already moved were found on this drive — it looks \
                         like a different drive at the same location. For a move, pick the \
                         destination again yourself."
                    } else {
                        "None of the files already copied were found on this drive — it may be \
                         a different drive at the same location."
                    })
                    .font(FontId::new(12.5, FontFamily::Proportional))
                    .color(p.amber),
                );
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let ready = src_ok && dst_ok && !wrong_drive_move;
                // A move deletes originals, and so does continuing one — it stays a
                // two-click action here exactly as on the pick screen. A copy onto a
                // suspect drive also asks twice.
                let needs_arm = job.mode == Mode::Move || !same_drive;
                let label = if st.pending_armed {
                    match job.mode {
                        Mode::Copy => "Press again to continue".to_string(),
                        Mode::Move => "Confirm — originals are deleted after copying".to_string(),
                    }
                } else {
                    match (job.mode, same_drive) {
                        (Mode::Copy, true) => "Continue copying".to_string(),
                        (Mode::Copy, false) => "Copy everything again".to_string(),
                        (Mode::Move, _) => "Continue the move".to_string(),
                    }
                };
                let fill = if st.pending_armed && job.mode == Mode::Move { p.amber } else { p.teal_bright };
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::new(
                            RichText::new(label)
                                .size(13.5)
                                .strong()
                                .color(if st.pending_armed && job.mode == Mode::Move {
                                    Color32::WHITE
                                } else {
                                    theme::on_accent(p)
                                }),
                        )
                        .fill(fill)
                        .rounding(Rounding::same(8.0)),
                    )
                    .clicked()
                {
                    if needs_arm && !st.pending_armed {
                        st.pending_armed = true;
                    } else {
                        st.pending_armed = false;
                        // The toggle follows the job, never the other way round — a
                        // resumed transfer's direction is a recorded fact, and the
                        // control on screen must agree with what is actually running.
                        st.direction = if !dest_is_removable(&job.dst)
                            && dest_is_removable(&job.src)
                        {
                            Direction::FromDrive
                        } else {
                            Direction::ToDrive
                        };
                        st.browser.locked = st.locked_field();
                        st.src = Some(job.src.clone());
                        // The recorded selection and filters, not defaults: without
                        // them Continue silently widened a three-folder selection into
                        // a whole-parent job and rescanned everything the user chose
                        // to leave out. Found by adversarial review.
                        st.src_names = job.names.clone();
                        st.skip_caches = job.skip_caches;
                        st.skip_cloud = job.skip_cloud;
                        st.dst = Some(job.dst.clone());
                        // The session line records the *effective* destination — start()
                        // must not derive it a second time.
                        st.exact_dst = true;
                        st.mode = job.mode;
                        // Setting the mode directly bypasses the toggle's click handler,
                        // which is where the verify default is normally established — a
                        // resumed Move left verify=false behind, and the user's NEXT
                        // manual Move then ran unverified without one click asking for
                        // it. (This run itself is safe: exact_dst forces verify on.)
                        st.verify = job.mode == Mode::Move;
                        st.move_armed = false;
                        st.auto_dst = None;
                        st.pending.remove(0);
                        st.start();
                        return;
                    }
                }
                if ui
                    .add(
                        egui::Button::new(RichText::new("Not now").size(13.0).color(p.ink))
                            .fill(p.surface)
                            .stroke(Stroke::new(1.0, p.line)),
                    )
                    .clicked()
                {
                    // Hide the offer for this session only. The journal stays: the debt
                    // is real and next launch offers it again.
                    st.pending_armed = false;
                    st.pending.remove(0);
                }
            });
            if st.pending.len() > 1 {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("…and {} more unfinished", st.pending.len() - 1))
                        .font(FontId::new(11.5, FontFamily::Proportional))
                        .color(p.ink_3),
                );
            }
        });
    ui.add_space(14.0);
}

fn pick(ui: &mut egui::Ui, st: &mut Ui, p: &Palette) {
    resume_card(ui, st, p);
    ui.label(
        RichText::new(match st.direction {
            Direction::ToDrive => "Copy to another drive",
            Direction::FromDrive => "Copy back from a drive",
        })
        .font(FontId::new(27.0, FontFamily::Proportional))
        .strong()
        .color(p.ink),
    );
    ui.add_space(6.0);
    ui.label(
        RichText::new(
            "Unplug the drive, close this window — even lose power. Nothing already \
             written is lost; open Kevat again and it offers to continue from where it \
             stopped.",
        )
        .font(FontId::new(14.0, FontFamily::Proportional))
        .color(p.ink_2),
    );
    ui.add_space(14.0);

    // The direction control — one visible bit of state. The drive restriction always
    // sits on the drive side, so the locked panel can never point the wrong way.
    ui.horizontal(|ui| {
        let fwd = st.direction == Direction::ToDrive;
        if ui
            .add(egui::SelectableLabel::new(fwd, RichText::new("  To a drive  ").size(13.5)))
            .clicked()
        {
            st.set_direction(Direction::ToDrive);
        }
        if ui
            .add(egui::SelectableLabel::new(!fwd, RichText::new("  From a drive  ").size(13.5)))
            .clicked()
        {
            st.set_direction(Direction::FromDrive);
        }
    });
    ui.add_space(16.0);

    if st.browser.open_for.is_some() {
        browser_panel(ui, st, p);
        return;
    }

    {
        // Belt-and-braces: the browser's notion of the locked field must always match
        // the direction, whatever path led here.
        st.browser.locked = st.locked_field();
        let br = &mut st.browser;
        folder_field(ui, p, "Copy from", &st.src, Field::Source, br, &st.src_names);
        ui.add_space(16.0);
        folder_field(ui, p, "Copy to", &st.dst, Field::Dest, br, &[]);
    }

    // Offer plugged-in removable drives for the drive-locked field — the destination in
    // forward mode, the source in reverse. Kept live: a drive plugged in while this
    // screen is open should appear without the user having to click.
    let locked = st.locked_field();
    let locked_value =
        if locked == Field::Dest { st.dst.clone() } else { st.src.clone() };
    if locked_value.is_none() {
        ui.ctx().request_repaint_after(Duration::from_secs(2));
        let drives = removable_drives();

        // Auto-detect: one external drive plugged in, nothing chosen yet → fill it in for
        // them. Filling either field writes nothing (the copy still needs an explicit
        // press and, for Move, a confirm), so pre-filling is safe and saves a click. The
        // auto_dst guard means clearing it will not immediately snap the same drive back —
        // only a *different* drive appearing re-triggers the fill.
        if drives.len() == 1 && st.auto_dst.as_ref() != Some(&drives[0]) {
            match locked {
                Field::Dest => {
                    st.dst = Some(drives[0].clone());
                    st.exact_dst = false;
                }
                Field::Source => st.src = Some(drives[0].clone()),
            }
            st.auto_dst = Some(drives[0].clone());
            st.move_armed = false;
        }
    }
    let locked_value =
        if locked == Field::Dest { st.dst.clone() } else { st.src.clone() };
    if locked_value.is_none() {
        let drives = removable_drives();
        ui.add_space(10.0);
        if drives.is_empty() {
            ui.label(
                RichText::new(match st.direction {
                    Direction::ToDrive => "Please connect a USB drive — it will appear here.",
                    Direction::FromDrive => {
                        "Please connect the drive to copy from — it will appear here."
                    }
                })
                .font(FontId::new(13.0, FontFamily::Proportional))
                .color(p.ink_3),
            );
        } else {
            ui.label(
                RichText::new(if drives.len() == 1 { "External drive" } else { "External drives" })
                    .font(FontId::new(12.0, FontFamily::Proportional))
                    .strong()
                    .color(p.ink_2),
            );
            ui.add_space(6.0);
            for d in drives {
                let name = drive_label(&d);
                let free = engine::free_space(&d)
                    .map(|b| format!(" — {} free", human(b)))
                    .unwrap_or_default();
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!("Use “{name}”{free}")).size(13.0).color(p.teal),
                        )
                        .fill(p.teal_wash)
                        .stroke(Stroke::new(1.0, p.teal_bright))
                        .rounding(Rounding::same(8.0)),
                    )
                    .clicked()
                {
                    match locked {
                        Field::Dest => {
                            st.dst = Some(d);
                            st.exact_dst = false;
                        }
                        Field::Source => st.src = Some(d),
                    }
                    st.move_armed = false;
                }
            }
        }
    }

    // When we picked the drive for them, say so plainly — an auto-filled path the user
    // didn't notice is exactly the "acted somewhere unexpected" failure we guard against.
    let auto_filled = st.auto_dst.is_some()
        && st.auto_dst == if locked == Field::Dest { st.dst.clone() } else { st.src.clone() };
    if auto_filled {
        ui.add_space(6.0);
        ui.label(
            RichText::new("External drive detected automatically — press “Choose…” to change it.")
                .font(FontId::new(12.5, FontFamily::Proportional))
                .color(p.teal),
        );
    }

    // Direction hint, never a block: picking a source that is itself on an external
    // drive while pointed "to a drive" usually means the user wants the other
    // direction — but drive→drive is legitimate, so offer the switch, don't force it.
    if st.direction == Direction::ToDrive {
        if let Some(src) = &st.src {
            // Show the hint past an auto-filled destination too: with exactly one drive
            // connected, auto-fill has already set dst, and that is precisely the case
            // where a drive-resident source most likely means "copy it back".
            if removable_root_of(src).is_some() && (st.dst.is_none() || st.dst == st.auto_dst) {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(
                            "That folder is already on an external drive. Copying it back \
                             to this computer?",
                        )
                        .font(FontId::new(12.5, FontFamily::Proportional))
                        .color(p.ink_2),
                    );
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Switch to “From a drive”").size(12.5).color(p.teal),
                            )
                            .fill(p.teal_wash)
                            .stroke(Stroke::new(1.0, p.teal_bright))
                            .rounding(Rounding::same(7.0)),
                        )
                        .clicked()
                    {
                        let keep = st.src.clone();
                        st.set_direction(Direction::FromDrive);
                        // The picked drive folder survives the switch — it is exactly
                        // what the user meant, now on the correct (source) side.
                        st.src = keep;
                    }
                });
            }
        }
    }

    // A folder becomes a same-named folder inside the destination — say so, so the drive's
    // root doesn't fill with loose files unexpectedly.
    if let (Some(src), Some(_dst)) = (&st.src, &st.dst) {
        if src.is_dir() && st.src_names.is_empty() {
            if let Some(name) = src.file_name() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "A folder “{}” will be created inside the destination.",
                        name.to_string_lossy()
                    ))
                    .font(FontId::new(12.5, FontFamily::Proportional))
                    .color(p.ink_3),
                );
            }
        }
    }

    // Conflicts: destination files that already exist and hold something different.
    // Counted here, before a byte moves — enumerate-first makes it cheap, and silently
    // replacing a newer file at the destination was the one way this app could lose
    // data without saying so. Cached by (src, dst) so it is not a filesystem walk on
    // every repaint.
    if let (Some(src), Some(dst)) = (st.src.clone(), st.dst.clone()) {
        let landed = if st.exact_dst || !st.src_names.is_empty() {
            dst.clone()
        } else {
            effective_dst(&src, &dst)
        };
        // The key carries everything the count depends on: the pair, the selection
        // (a changed multi-selection must not show the previous selection's number),
        // and both filter toggles.
        let mut key = format!(
            "{}\u{0}{}\u{0}{}{}",
            src.display(),
            landed.display(),
            st.skip_caches as u8,
            st.skip_cloud as u8
        );
        for n in &st.src_names {
            key.push('\u{0}');
            key.push_str(&n.to_string_lossy());
        }
        if st.conflict_key.as_deref() != Some(key.as_str()) {
            st.conflict_key = Some(key);
            st.conflicts = if landed.exists() {
                // Same filter as the real run, or the count would name conflicts inside
                // folders the transfer will never touch.
                let f = st.scan_filter();
                let manifest = if st.src_names.is_empty() {
                    scan::scan_with(&src, &f).ok()
                } else {
                    scan::scan_selected_with(&src, &st.src_names, &f).ok()
                };
                manifest.map(|m| engine::conflicts(&landed, &m).len()).unwrap_or(0)
            } else {
                0
            };
        }
        if st.conflicts > 0 {
            ui.add_space(10.0);
            ui.label(
                RichText::new(format!(
                    "{} already on the drive and different.",
                    plural(st.conflicts, "file")
                ))
                .font(FontId::new(13.0, FontFamily::Proportional))
                .color(p.amber),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let replace = st.on_exists == engine::OnExists::Replace;
                if ui
                    .add(egui::SelectableLabel::new(
                        replace,
                        RichText::new("  Replace them  ").size(13.0),
                    ))
                    .clicked()
                {
                    st.on_exists = engine::OnExists::Replace;
                    st.move_armed = false;
                }
                if ui
                    .add(egui::SelectableLabel::new(
                        !replace,
                        RichText::new("  Keep what's there  ").size(13.0),
                    ))
                    .clicked()
                {
                    st.on_exists = engine::OnExists::Keep;
                    st.move_armed = false;
                }
            });
        }
    } else {
        st.conflict_key = None;
        st.conflicts = 0;
    }

    ui.add_space(22.0);

    ui.horizontal(|ui| {
        let copy_on = st.mode == Mode::Copy;
        if ui
            .add(egui::SelectableLabel::new(copy_on, RichText::new("  Copy  ").size(14.0)))
            .clicked()
        {
            st.mode = Mode::Copy;
            st.verify = false;
            st.move_armed = false;
        }
        if ui
            .add(egui::SelectableLabel::new(!copy_on, RichText::new("  Move  ").size(14.0)))
            .clicked()
        {
            st.mode = Mode::Move;
            // The safe default; unticking it is allowed and warned about below.
            st.verify = true;
            st.move_armed = false;
        }
        ui.add_space(14.0);
        let mut v = st.verify;
        if ui.checkbox(&mut v, RichText::new("Check every file after writing").size(13.5)).changed() {
            st.verify = v;
            st.move_armed = false;
        }
    });

    ui.add_space(8.0);
    let mut sc = st.skip_caches;
    if ui
        .checkbox(
            &mut sc,
            RichText::new("Leave out app caches — AppData, node_modules, .cache and similar")
                .size(13.5),
        )
        .changed()
    {
        st.skip_caches = sc;
        st.move_armed = false;
        // The manifest changes, so the cached conflict count is stale.
        st.conflict_key = None;
    }
    if st.skip_caches {
        ui.label(
            RichText::new(
                "Apps rebuild these. On a big folder they are most of the files and most \
                 of the time, and almost none of the value.",
            )
            .font(FontId::new(12.5, FontFamily::Proportional))
            .color(p.ink_2),
        );
    }

    ui.add_space(6.0);
    let mut so = st.skip_cloud;
    if ui
        .checkbox(
            &mut so,
            RichText::new("Leave out cloud folders — OneDrive, Dropbox, Google Drive, iCloud")
                .size(13.5),
        )
        .changed()
    {
        st.skip_cloud = so;
        st.move_armed = false;
        st.conflict_key = None;
    }
    if st.skip_cloud {
        ui.label(
            RichText::new(
                "These already live on their service's servers, and files kept only in \
                 the cloud can't be copied from outside their own account anyway.",
            )
            .font(FontId::new(12.5, FontFamily::Proportional))
            .color(p.ink_2),
        );
    }

    if st.mode == Mode::Move {
        ui.add_space(8.0);
        ui.label(
            RichText::new(match (st.direction, st.verify) {
                (Direction::ToDrive, true) => {
                    "Moving deletes the originals — each one only after its copy is safely \
                     on the drive and has been checked."
                }
                // The check is off: still two-phase — nothing is deleted until every \
                // copy is finished and flushed — but the files are not read back first.
                (Direction::ToDrive, false) => {
                    "Moving deletes the originals — only at the very end, after every copy \
                     is finished and flushed to the drive. With the check off they are not \
                     read back first: faster, a little less certain."
                }
                // State the consequence in place: after a reverse move, the drive no
                // longer holds a copy — the backup ceases to exist. That is what move
                // means, but it must be said before the two-click confirm, not after.
                (Direction::FromDrive, true) => {
                    "Moving deletes the originals from the drive — each one only after its \
                     copy is safely on this computer and has been checked. When it \
                     finishes, the drive no longer has a copy."
                }
                (Direction::FromDrive, false) => {
                    "Moving deletes the originals from the drive — only at the very end, \
                     after every copy is finished and flushed. With the check off they are \
                     not read back first, and when it finishes the drive no longer has a \
                     copy."
                }
            })
            .font(FontId::new(13.0, FontFamily::Proportional))
            .color(p.amber),
        );
    }

    ui.add_space(24.0);
    let ready = st.src.is_some() && st.dst.is_some() && st.src != st.dst;
    ui.add_enabled_ui(ready, |ui| {
        if st.mode == Mode::Move && st.move_armed {
            // Second step: the originals-will-be-deleted confirmation, amber like Erase.
            ui.horizontal(|ui| {
                if ui
                    .add_sized(
                        [260.0, 44.0],
                        egui::Button::new(
                            RichText::new("Move — delete the originals")
                                .size(15.0)
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(p.amber)
                        .rounding(Rounding::same(10.0)),
                    )
                    .clicked()
                {
                    st.move_armed = false;
                    st.start();
                }
                if ui
                    .add_sized(
                        [96.0, 44.0],
                        egui::Button::new(RichText::new("Cancel").size(14.0).color(p.ink))
                            .fill(p.surface)
                            .stroke(Stroke::new(1.0, p.line)),
                    )
                    .clicked()
                {
                    st.move_armed = false;
                }
            });
        } else {
            let label = if st.mode == Mode::Copy { "Start copying" } else { "Start moving" };
            if ui
                .add_sized(
                    [168.0, 44.0],
                    egui::Button::new(
                        RichText::new(label).size(15.0).strong().color(theme::on_accent(p)),
                    )
                    .fill(p.teal_bright)
                    .rounding(Rounding::same(10.0)),
                )
                .clicked()
            {
                if st.mode == Mode::Move {
                    st.move_armed = true; // arm; the second click commits
                } else {
                    st.start();
                }
            }
        }
    });

    if st.src.is_some() && st.src == st.dst {
        ui.add_space(10.0);
        ui.label(
            RichText::new("Those are the same folder.")
                .font(FontId::new(13.0, FontFamily::Proportional))
                .color(p.amber),
        );
    }

    // A refused start (e.g. the chosen drive was unplugged) must say why, right here
    // where the button is — a button that silently does nothing reads as a broken app.
    if let Some(err) = &st.error {
        ui.add_space(10.0);
        ui.label(
            RichText::new(err.clone())
                .font(FontId::new(13.0, FontFamily::Proportional))
                .color(p.amber),
        );
    }
}

/// The History screen: every finished run, newest first, from the local
/// `history.jsonl`. Strictly this machine's file — nothing is sent anywhere, which is
/// why the header says so. A resume shows the files *that run* actually copied, so a
/// transfer interrupted twice reads as three honest rows, not one inflated one.
fn history(ui: &mut egui::Ui, st: &mut Ui, p: &Palette) {
    ui.label(
        RichText::new("History")
            .font(FontId::new(27.0, FontFamily::Proportional))
            .strong()
            .color(p.ink),
    );
    ui.add_space(4.0);
    ui.label(
        RichText::new("Every transfer this machine has run. Stored only on this computer.")
            .font(FontId::new(13.0, FontFamily::Proportional))
            .color(p.ink_3),
    );
    ui.add_space(14.0);

    if st.history.is_empty() {
        ui.label(
            RichText::new("No transfers recorded yet.")
                .font(FontId::new(14.0, FontFamily::Proportional))
                .color(p.ink_2),
        );
        return;
    }

    let list_h = (ui.available_height() - 24.0).max(120.0);
    egui::ScrollArea::vertical()
        .max_height(list_h)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for e in st.history.iter().rev() {
                egui::Frame::none()
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.line_soft))
                    .rounding(Rounding::same(8.0))
                    .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(fmt_mtime(e.at))
                                    .font(FontId::new(12.0, FontFamily::Monospace))
                                    .color(p.ink_3),
                            );
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(match e.mode {
                                    Mode::Copy => "Copy",
                                    Mode::Move => "Move",
                                })
                                .font(FontId::new(12.0, FontFamily::Monospace))
                                .strong()
                                .color(if e.mode == Mode::Move { p.amber } else { p.teal }),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let (status, colour) = if e.done {
                                    ("finished".to_string(), p.teal)
                                } else if e.errors > 0 {
                                    (format!("{} error(s)", e.errors), p.amber)
                                } else {
                                    ("stopped — resumable".to_string(), p.amber)
                                };
                                ui.label(
                                    RichText::new(status)
                                        .font(FontId::new(12.0, FontFamily::Monospace))
                                        .color(colour),
                                );
                            });
                        });
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(format!("{}  →  {}", e.src, e.dst))
                                .font(FontId::new(13.0, FontFamily::Proportional))
                                .color(p.ink),
                        );
                        ui.add_space(2.0);
                        let skipped = if e.skipped > 0 {
                            format!(" · {} already there", e.skipped)
                        } else {
                            String::new()
                        };
                        ui.label(
                            RichText::new(format!(
                                "{} file(s), {}{} · {}",
                                e.copied,
                                human(e.bytes),
                                skipped,
                                if e.secs >= 60.0 {
                                    format!("{:.0} min", e.secs / 60.0)
                                } else {
                                    format!("{:.0} s", e.secs.max(1.0))
                                },
                            ))
                            .font(FontId::new(12.5, FontFamily::Proportional))
                            .color(p.ink_2),
                        );
                    });
                ui.add_space(8.0);
            }
        });
}

fn erase(ui: &mut egui::Ui, st: &mut Ui, p: &Palette) {
    ui.label(
        RichText::new("Erase a drive")
            .font(FontId::new(27.0, FontFamily::Proportional))
            .strong()
            .color(p.ink),
    );
    ui.add_space(6.0);
    ui.label(
        RichText::new(
            "Formatting erases everything on the drive, permanently — there is no undo. Only \
             removable drives are shown; your system disk can never appear here.",
        )
        .font(FontId::new(14.0, FontFamily::Proportional))
        .color(p.amber),
    );
    ui.add_space(20.0);

    if st.erase.drives.is_empty() {
        ui.label(
            RichText::new("No removable drive is plugged in. Connect one and reopen this screen.")
                .font(FontId::new(14.0, FontFamily::Proportional))
                .color(p.ink_2),
        );
    } else {
        ui.label(
            RichText::new("DRIVE")
                .font(FontId::new(11.0, FontFamily::Monospace))
                .color(p.ink_3)
                .strong(),
        );
        ui.add_space(6.0);
        let drives = st.erase.drives.clone();
        for (i, d) in drives.iter().enumerate() {
            let selected = st.erase.selected == Some(i);
            let text = format!("{}   ({}, {})", d.label, human(d.size), d.device);
            if ui
                .add(egui::SelectableLabel::new(
                    selected,
                    RichText::new(text).font(FontId::new(14.0, FontFamily::Monospace)),
                ))
                .clicked()
            {
                st.erase.selected = Some(i);
                st.erase.confirm.clear();
                st.erase.status = None;
                if st.erase.fs.is_none() {
                    // FAT32 by default: the format every device on earth reads — TVs,
                    // cameras, car stereos, decade-old machines. The 4 GB per-file limit
                    // is stated beside the choice, so picking exFAT for big files is one
                    // informed click away.
                    let formats = Fs::available();
                    st.erase.fs = formats
                        .iter()
                        .copied()
                        .find(|f| *f == Fs::Fat32)
                        .or_else(|| formats.first().copied());
                }
            }
        }
    }

    // Everything below only appears once a drive is chosen.
    if let Some(idx) = st.erase.selected {
        let drive = st.erase.drives[idx].clone();
        let formats = Fs::available();

        ui.add_space(18.0);
        ui.label(
            RichText::new("FORMAT AS")
                .font(FontId::new(11.0, FontFamily::Monospace))
                .color(p.ink_3)
                .strong(),
        );
        ui.add_space(6.0);
        if formats.is_empty() {
            ui.label(
                RichText::new("No formatting tools are installed on this system.")
                    .font(FontId::new(13.5, FontFamily::Proportional))
                    .color(p.amber),
            );
        }
        for f in &formats {
            let on = st.erase.fs == Some(*f);
            if ui
                .add(egui::SelectableLabel::new(
                    on,
                    RichText::new(f.label()).size(13.5),
                ))
                .clicked()
            {
                st.erase.fs = Some(*f);
            }
        }
        // The one fact that changes the choice: FAT32 cannot hold a file over 4 GB.
        // Stated where the decision is made, not discovered mid-copy.
        if st.erase.fs == Some(Fs::Fat32) {
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "FAT32 works in everything, but cannot hold a single file over 4 GB — \
                     pick exFAT if you copy large files.",
                )
                .font(FontId::new(12.5, FontFamily::Proportional))
                .color(p.ink_3),
            );
        }

        ui.add_space(18.0);
        egui::Frame::none()
            .fill(p.teal_wash)
            .stroke(Stroke::new(1.0, p.amber))
            .rounding(Rounding::same(10.0))
            .inner_margin(14.0)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!(
                        "This will erase everything on “{}” ({}). This cannot be undone.",
                        drive.label,
                        human(drive.size)
                    ))
                    .font(FontId::new(14.0, FontFamily::Proportional))
                    .strong()
                    .color(p.ink),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("Type the drive's name  ({})  to confirm:", drive.label))
                        .font(FontId::new(13.0, FontFamily::Proportional))
                        .color(p.ink_2),
                );
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::singleline(&mut st.erase.confirm)
                        .hint_text(&drive.label)
                        .desired_width(240.0),
                );
            });

        ui.add_space(16.0);
        let armed = st.erase.fs.is_some() && st.erase.confirm == drive.label;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(armed, |ui| {
                if ui
                    .add_sized(
                        [150.0, 40.0],
                        egui::Button::new(
                            RichText::new("Erase drive").size(14.0).strong().color(Color32::WHITE),
                        )
                        .fill(p.amber)
                        .rounding(Rounding::same(9.0)),
                    )
                    .clicked()
                {
                    if let Some(fs) = st.erase.fs {
                        st.erase.status = Some(format_drive(&drive, fs));
                        // Rescan; the drive's identity may have changed after formatting.
                        st.erase.drives = list_removable_drives();
                        st.erase.selected = None;
                        st.erase.confirm.clear();
                    }
                }
            });
            if ui
                .add_sized(
                    [96.0, 40.0],
                    egui::Button::new(RichText::new("Cancel").size(14.0).color(p.ink))
                        .fill(p.surface)
                        .stroke(Stroke::new(1.0, p.line)),
                )
                .clicked()
            {
                st.erase.selected = None;
                st.erase.confirm.clear();
            }
        });
    }

    if let Some(result) = &st.erase.status {
        ui.add_space(14.0);
        match result {
            Ok(msg) => ui.label(
                RichText::new(msg)
                    .font(FontId::new(14.0, FontFamily::Proportional))
                    .color(p.teal),
            ),
            Err(msg) => ui.label(
                RichText::new(format!("Could not erase it: {msg}"))
                    .font(FontId::new(14.0, FontFamily::Proportional))
                    .color(p.amber),
            ),
        };
    }

    ui.add_space(24.0);
    if ui
        .add_sized(
            [110.0, 38.0],
            egui::Button::new(RichText::new("Back").size(14.0).color(p.ink))
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.line)),
        )
        .clicked()
    {
        st.screen = Screen::Pick;
    }
}

/// Commit one path as the source and close the picker. A changed selection voids an
/// armed Move (the confirm was for the old pair) and any journal-sourced exact
/// destination (recorded for the old source; keeping it would spill the new source into
/// the old job's folder).
fn commit_single(st: &mut Ui, path: PathBuf) {
    st.src = Some(path);
    st.src_names.clear();
    st.move_armed = false;
    st.exact_dst = false;
    st.browser.open_for = None;
}

fn browser_panel(ui: &mut egui::Ui, st: &mut Ui, p: &Palette) {
    let which = st.browser.open_for.unwrap();
    let locked = st.browser.locked;
    let title = match (which, which == locked) {
        (Field::Source, false) => "Choose a folder or a file to copy from",
        (Field::Source, true) => "Choose what to copy from the drive",
        (Field::Dest, true) => "Choose an external drive to copy into",
        (Field::Dest, false) => "Choose a folder on this computer to copy into",
    };
    ui.label(
        RichText::new(title)
            .font(FontId::new(18.0, FontFamily::Proportional))
            .strong()
            .color(p.ink),
    );
    ui.add_space(10.0);

    // The drive-locked field is restricted to connected external drives. Until one is
    // entered, show the drive chooser (or the "connect a drive" prompt) instead of the
    // filesystem browser — that field never points at the internal disk.
    if which == locked && st.browser.drive_root.is_none() {
        drive_chooser(ui, st, p, which);
        return;
    }

    // ── Up + current path ────────────────────────────────────────────────────
    let mut back_to_drives = false;
    ui.horizontal(|ui| {
        if ui
            .add(
                egui::Button::new(RichText::new("Up").size(13.0).color(p.ink))
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.line)),
            )
            .clicked()
        {
            // The locked field's browsing is bounded to the drive: at the drive root
            // "Up" goes back to the drive list, never into the internal filesystem
            // above it. The free field climbs normally.
            if which == locked {
                let at_root = st.browser.drive_root.as_deref() == Some(st.browser.cwd.as_path());
                if at_root {
                    back_to_drives = true;
                } else if let Some(parent) = st.browser.cwd.parent().map(|q| q.to_path_buf()) {
                    st.browser.go(parent);
                }
            } else if let Some(parent) = st.browser.cwd.parent().map(|q| q.to_path_buf()) {
                st.browser.go(parent);
            }
        }
        ui.label(
            RichText::new(shorten(&st.browser.cwd, 54))
                .font(FontId::new(13.0, FontFamily::Monospace))
                .color(p.ink_2),
        );
    });
    if back_to_drives {
        // Drop back to the drive list next frame rather than drawing a stale listing.
        st.browser.drive_root = None;
        ui.ctx().request_repaint();
        return;
    }

    // Quick jumps. For the free field, Home plus every mounted drive. For the locked
    // field, only the connected external drives — switching between two plugged-in
    // disks is one click, and there is nowhere else that field may go.
    if which != locked {
        let places = drive_places();
        if !places.is_empty() {
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                for (name, path) in places {
                    if ui
                        .add(
                            egui::Button::new(RichText::new(name).size(12.5).color(p.ink))
                                .fill(p.sunk)
                                .stroke(Stroke::new(1.0, p.line)),
                        )
                        .clicked()
                    {
                        st.browser.go(path);
                    }
                }
            });
        }
    } else {
        let drives = removable_drives();
        if drives.len() > 1 {
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                for d in drives {
                    let name = drive_label(&d);
                    let on = st.browser.drive_root.as_deref() == Some(d.as_path());
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(name)
                                    .size(12.5)
                                    .color(if on { p.teal } else { p.ink }),
                            )
                            .fill(if on { p.teal_wash } else { p.sunk })
                            .stroke(Stroke::new(1.0, if on { p.teal_bright } else { p.line })),
                        )
                        .clicked()
                    {
                        st.browser.drive_root = Some(d.clone());
                        st.browser.go(d);
                    }
                }
            });
        }
    }
    ui.add_space(8.0);

    // Picker controls: hidden files, select-all, and (for a destination) a new folder —
    // the things a file dialog has had since forever, absent here until now.
    ui.horizontal(|ui| {
        let mut hidden = st.browser.show_hidden;
        if ui
            .checkbox(&mut hidden, RichText::new("Show hidden files").size(12.5))
            .changed()
        {
            st.browser.show_hidden = hidden;
            st.browser.reload();
        }
        if which == Field::Source {
            ui.add_space(12.0);
            if ui
                .add(
                    egui::Button::new(RichText::new("Select all").size(12.5).color(p.ink))
                        .fill(p.sunk)
                        .stroke(Stroke::new(1.0, p.line)),
                )
                .on_hover_text("Ctrl+A")
                .clicked()
            {
                st.browser.picked = st
                    .browser
                    .entries
                    .iter()
                    .filter_map(|it| it.path.file_name().map(|n| n.to_os_string()))
                    .collect();
            }
            if !st.browser.picked.is_empty() {
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new(RichText::new("Clear").size(12.5).color(p.ink))
                            .fill(p.sunk)
                            .stroke(Stroke::new(1.0, p.line)),
                    )
                    .clicked()
                {
                    st.browser.picked.clear();
                    st.browser.anchor = None;
                }
            }
        }
        if which == Field::Dest && st.browser.new_folder.is_none() {
            ui.add_space(12.0);
            if ui
                .add(
                    egui::Button::new(RichText::new("New folder").size(12.5).color(p.ink))
                        .fill(p.sunk)
                        .stroke(Stroke::new(1.0, p.line)),
                )
                .clicked()
            {
                st.browser.new_folder = Some(String::new());
            }
        }
    });

    // The inline new-folder row: type a name, press Create, land inside it.
    if let Some(mut name) = st.browser.new_folder.clone() {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(&mut name)
                    .hint_text("folder name")
                    .desired_width(220.0),
            );
            edit.request_focus();
            st.browser.new_folder = Some(name.clone());
            let submit = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui
                .add(
                    egui::Button::new(
                        RichText::new("Create").size(12.5).color(theme::on_accent(p)),
                    )
                    .fill(p.teal_bright),
                )
                .clicked();
            if (submit || clicked) && !name.trim().is_empty() {
                let target = st.browser.cwd.join(name.trim());
                match std::fs::create_dir(&target) {
                    Ok(()) => {
                        st.browser.new_folder = None;
                        st.browser.go(target);
                    }
                    Err(e) => {
                        st.browser.error = Some(format!("could not create the folder: {e}"));
                        st.browser.new_folder = None;
                    }
                }
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("Cancel").size(12.5).color(p.ink))
                        .fill(p.surface)
                        .stroke(Stroke::new(1.0, p.line)),
                )
                .clicked()
            {
                st.browser.new_folder = None;
            }
        });
    }
    ui.add_space(8.0);

    if let Some(err) = st.browser.error.clone() {
        ui.label(
            RichText::new(err)
                .font(FontId::new(13.0, FontFamily::Proportional))
                .color(p.amber),
        );
        ui.add_space(6.0);
    }

    // Column headers, clickable to sort — the active one is teal with its direction shown.
    // Size is a source-only column: a destination lists folders, which have no size to show.
    let key = st.browser.sort_key;
    let desc = st.browser.sort_desc;
    let show_size = which == Field::Source;
    let hdr = |ui: &mut egui::Ui, text: &str, this: SortKey| {
        let active = key == this;
        let mut label = text.to_string();
        if active {
            label.push_str(if desc { "  (desc)" } else { "  (asc)" });
        }
        ui.add(
            egui::Button::new(
                RichText::new(label)
                    .font(FontId::new(11.0, FontFamily::Monospace))
                    .strong()
                    .color(if active { p.teal } else { p.ink_3 }),
            )
            .frame(false),
        )
        .clicked()
    };
    let date_col = 140.0;
    let size_col = if show_size { 92.0 } else { 0.0 };
    ui.horizontal(|ui| {
        if hdr(ui, "NAME", SortKey::Name) {
            st.browser.sort_by(SortKey::Name);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(6.0);
            // Reserve each column's width so the header sits over the values below it. In a
            // right-to-left layout the first allocation is the rightmost, so Date comes first
            // and Size lands to its left.
            ui.allocate_ui_with_layout(
                Vec2::new(date_col, 18.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    if hdr(ui, "DATE MODIFIED", SortKey::Modified) {
                        st.browser.sort_by(SortKey::Modified);
                    }
                },
            );
            if show_size {
                ui.allocate_ui_with_layout(
                    Vec2::new(size_col, 18.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        if hdr(ui, "SIZE", SortKey::Size) {
                            st.browser.sort_by(SortKey::Size);
                        }
                    },
                );
            }
        });
    });
    ui.add_space(4.0);

    // Size the list to what is left after reserving room for the action row below it, so
    // the "Choose…"/"Cancel" buttons are never pushed off the bottom of the window. The
    // list scrolls internally; the buttons stay put.
    let list_h = (ui.available_height() - 64.0).clamp(120.0, 360.0);
    egui::Frame::none()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.line_soft))
        .rounding(Rounding::same(10.0))
        .inner_margin(6.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(list_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let entries = st.browser.entries.clone();
                    if entries.is_empty() {
                        ui.add_space(8.0);
                        let empty = if which == Field::Source {
                            "Nothing in here."
                        } else {
                            "No folders on this drive — “Copy into” will use the drive itself."
                        };
                        ui.label(
                            RichText::new(empty)
                                .font(FontId::new(13.0, FontFamily::Proportional))
                                .color(p.ink_3),
                        );
                        ui.add_space(8.0);
                    }
                    // Each row is drawn by hand rather than as a Button, so the name sits
                    // left-aligned under its column (a stretched Button centres its text),
                    // the whole strip lights up on hover, and a long name truncates with an
                    // ellipsis instead of shoving the size or date off the edge.
                    let row_w = ui.available_width();
                    let name_font = FontId::new(14.0, FontFamily::Proportional);
                    let meta_font = FontId::new(12.0, FontFamily::Monospace);
                    let pad = 8.0;
                    for (idx, item) in entries.iter().enumerate() {
                        let os_name = item
                            .path
                            .file_name()
                            .map(|n| n.to_os_string())
                            .unwrap_or_default();
                        let selected = st.browser.picked.contains(&os_name);
                        let name = os_name.to_string_lossy().into_owned();
                        // A folder keeps its trailing slash and the ink colour, since
                        // clicking it navigates; a file is tinted so it reads as the thing
                        // you land on. Only sources ever list files.
                        let (label, colour) = if item.is_dir {
                            (format!("{name}/"), p.ink)
                        } else {
                            (name, p.ink_2)
                        };

                        let (rect, resp) =
                            ui.allocate_exact_size(Vec2::new(row_w, 28.0), egui::Sense::click());
                        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        // A selected row keeps its tint whether or not the pointer is on
                        // it, so a multi-selection stays visible while you reach for the
                        // button.
                        if selected {
                            ui.painter().rect_filled(rect, Rounding::same(5.0), p.teal_wash);
                            ui.painter().rect_stroke(
                                rect,
                                Rounding::same(5.0),
                                Stroke::new(1.0, p.teal_bright),
                            );
                        } else if resp.hovered() {
                            ui.painter().rect_filled(rect, Rounding::same(5.0), p.teal_wash);
                        }

                        // Name, left-aligned and truncated to leave room for size and date.
                        let name_max = (row_w - date_col - size_col - pad * 2.0).max(40.0);
                        let mut job = egui::text::LayoutJob::single_section(
                            label,
                            egui::TextFormat {
                                font_id: name_font.clone(),
                                color: colour,
                                ..Default::default()
                            },
                        );
                        job.wrap = egui::text::TextWrapping {
                            max_width: name_max,
                            max_rows: 1,
                            break_anywhere: true,
                            overflow_character: Some('…'),
                        };
                        let galley = ui.fonts(|f| f.layout_job(job));
                        let ny = rect.center().y - galley.size().y / 2.0;
                        ui.painter()
                            .galley(egui::pos2(rect.left() + pad, ny), galley, colour);

                        // Size, right-aligned in its column (source only; folders show a dash).
                        if show_size {
                            let text = if item.is_dir {
                                "—".to_string()
                            } else {
                                human(item.size)
                            };
                            ui.painter().text(
                                egui::pos2(rect.right() - date_col - pad, rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                text,
                                meta_font.clone(),
                                p.ink_3,
                            );
                        }

                        // Date, right-aligned in its column.
                        ui.painter().text(
                            egui::pos2(rect.right() - pad, rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            fmt_mtime(item.mtime),
                            meta_font.clone(),
                            p.ink_3,
                        );

                        // Double-click opens a folder — the gesture every file manager
                        // trained people to use. A single click selects, so folders can
                        // join a multi-selection instead of being only navigable.
                        if resp.double_clicked() {
                            if item.is_dir {
                                st.browser.go(item.path.clone());
                            } else {
                                commit_single(st, item.path.clone());
                            }
                        } else if resp.clicked() {
                            let mods = ui.input(|i| i.modifiers);
                            if mods.shift_only() || (mods.shift && mods.command) {
                                // Extend from the anchor: the whole run between the two
                                // rows, inclusive, exactly like a list view.
                                let from = st.browser.anchor.unwrap_or(idx);
                                let (lo, hi) = if from <= idx { (from, idx) } else { (idx, from) };
                                if !mods.command {
                                    st.browser.picked.clear();
                                }
                                for row in entries.iter().take(hi + 1).skip(lo) {
                                    if let Some(n) = row.path.file_name() {
                                        st.browser.picked.insert(n.to_os_string());
                                    }
                                }
                            } else if mods.command {
                                // Ctrl (Cmd on macOS) toggles one row.
                                if selected {
                                    st.browser.picked.remove(&os_name);
                                } else {
                                    st.browser.picked.insert(os_name.clone());
                                }
                                st.browser.anchor = Some(idx);
                            } else {
                                // Plain click: this row alone.
                                st.browser.picked.clear();
                                st.browser.picked.insert(os_name.clone());
                                st.browser.anchor = Some(idx);
                            }
                        }
                    }
                });
        });

    ui.add_space(14.0);
    ui.horizontal(|ui| {
        // Name the folder in the button, so "which folder did I choose?" is answered by
        // the control itself — the current folder is the selection. Picking a single file
        // instead happens by clicking it in the list above.
        // A drive root has no file_name — "Copy into “USB STICK (D:)”" beats
        // "Copy into “/”", which names a root that doesn't even exist on Windows.
        let here = st
            .browser
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| drive_label(&st.browser.cwd));
        // The button says what it will actually take, so a selection can never be
        // silently ignored in favour of the folder around it.
        let n = st.browser.picked.len();
        let action = match which {
            Field::Source if n == 1 => {
                let only = st.browser.picked.iter().next().cloned().unwrap_or_default();
                format!("Choose “{}”", only.to_string_lossy())
            }
            Field::Source if n > 1 => format!("Choose {n} items"),
            Field::Source => format!("Choose “{here}”"),
            Field::Dest => format!("Copy into “{here}”"),
        };
        let commit = ui
            .add_sized(
                [230.0, 38.0],
                egui::Button::new(
                    RichText::new(action)
                        .size(14.0)
                        .strong()
                        .color(theme::on_accent(p)),
                )
                .fill(p.teal_bright)
                .rounding(Rounding::same(9.0)),
            )
            .clicked();
        // What the selection actually amounts to — files sized, folders counted. A
        // multi-selection with no total is a decision made blind.
        if n > 0 {
            let bytes: u64 = st
                .browser
                .entries
                .iter()
                .filter(|it| {
                    it.path
                        .file_name()
                        .map(|f| st.browser.picked.contains(f))
                        .unwrap_or(false)
                })
                .map(|it| it.size)
                .sum();
            let folders = st
                .browser
                .entries
                .iter()
                .filter(|it| {
                    it.is_dir
                        && it
                            .path
                            .file_name()
                            .map(|f| st.browser.picked.contains(f))
                            .unwrap_or(false)
                })
                .count();
            let mut text = format!("{n} selected");
            if bytes > 0 {
                text.push_str(&format!(" · {}", human(bytes)));
            }
            if folders > 0 {
                text.push_str(&format!(
                    " · {} (contents counted when it starts)",
                    plural(folders, "folder")
                ));
            }
            ui.add_space(10.0);
            ui.label(
                RichText::new(text)
                    .font(FontId::new(12.5, FontFamily::Proportional))
                    .color(p.ink_3),
            );
        }
        if commit {
            let chosen = st.browser.cwd.clone();
            match which {
                Field::Source => {
                    // Selected rows win over the folder itself: picking three items and
                    // pressing the button must take those three, not everything around
                    // them. One selected item behaves exactly like the old single pick.
                    let names: Vec<PathBuf> =
                        st.browser.picked.iter().map(PathBuf::from).collect();
                    match names.len() {
                        0 => {
                            st.src = Some(chosen);
                            st.src_names.clear();
                        }
                        1 => {
                            st.src = Some(chosen.join(&names[0]));
                            st.src_names.clear();
                        }
                        _ => {
                            st.src = Some(chosen);
                            st.src_names = names;
                        }
                    }
                    // A journal-sourced exact destination belonged to the old source.
                    st.exact_dst = false;
                }
                Field::Dest => {
                    st.dst = Some(chosen);
                    st.exact_dst = false;
                    // A drive picked by hand overrides any earlier auto-fill, so the
                    // auto-detect logic won't try to "helpfully" replace it.
                    st.auto_dst = None;
                }
            }
            // A changed selection voids an armed Move: the delete-the-originals confirm
            // the user gave was for the old pair, not this one.
            st.move_armed = false;
            st.browser.open_for = None;
        }
        if ui
            .add_sized(
                [96.0, 38.0],
                egui::Button::new(RichText::new("Cancel").size(14.0).color(p.ink))
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.line)),
            )
            .clicked()
        {
            st.browser.open_for = None;
        }
    });
}

/// The destination drive chooser: the entry list when picking a copy target. One button per
/// connected external drive, or a "please connect a USB drive" prompt that refreshes live so a
/// drive appears the moment it is mounted. Picking a drive drops into its subtree; from there
/// the browser is bounded to that drive.
/// The chooser for the drive-locked field — a destination in forward mode, a source in
/// reverse. Same list, same liveness; only the words change with the direction.
fn drive_chooser(ui: &mut egui::Ui, st: &mut Ui, p: &Palette, which: Field) {
    ui.ctx().request_repaint_after(Duration::from_secs(2));
    let _ = which;
    let drives = removable_drives();
    if drives.is_empty() {
        egui::Frame::none()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.line_soft))
            .rounding(Rounding::same(10.0))
            .inner_margin(22.0)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(match st.direction {
                        Direction::ToDrive => "Please connect a USB drive",
                        Direction::FromDrive => "Please connect the drive",
                    })
                    .font(FontId::new(17.0, FontFamily::Proportional))
                    .strong()
                    .color(p.ink),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(match st.direction {
                        Direction::ToDrive => {
                            "Kevat copies onto external drives. Plug one in and it will appear here."
                        }
                        Direction::FromDrive => {
                            "Kevat copies from an external drive back to this computer. \
                             Plug it in and it will appear here."
                        }
                    })
                    .font(FontId::new(13.0, FontFamily::Proportional))
                    .color(p.ink_3),
                );
            });
    } else {
        ui.label(
            RichText::new(if drives.len() == 1 {
                "Connected drive"
            } else {
                "Connected drives"
            })
            .font(FontId::new(11.0, FontFamily::Monospace))
            .strong()
            .color(p.ink_3),
        );
        ui.add_space(8.0);
        for d in drives {
            let name = drive_label(&d);
            let free = engine::free_space(&d)
                .map(|b| format!("   ·   {} free", human(b)))
                .unwrap_or_default();
            let w = ui.available_width().min(540.0);
            if ui
                .add_sized(
                    [w, 46.0],
                    egui::Button::new(
                        RichText::new(format!("{name}{free}")).size(15.0).color(p.teal),
                    )
                    .fill(p.teal_wash)
                    .stroke(Stroke::new(1.0, p.teal_bright))
                    .rounding(Rounding::same(9.0)),
                )
                .clicked()
            {
                st.browser.drive_root = Some(d.clone());
                st.browser.go(d);
            }
            ui.add_space(8.0);
        }
    }

    ui.add_space(14.0);
    if ui
        .add_sized(
            [96.0, 38.0],
            egui::Button::new(RichText::new("Cancel").size(14.0).color(p.ink))
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.line)),
        )
        .clicked()
    {
        st.browser.open_for = None;
    }
}

fn running(ui: &mut egui::Ui, st: &mut Ui, p: &Palette) {
    let s = &st.shared;
    let total = s.bytes_total.load(Ordering::Relaxed);
    let done = s.bytes_done.load(Ordering::Relaxed);
    let fresh = s.bytes_fresh.load(Ordering::Relaxed);
    let files_total = s.files_total.load(Ordering::Relaxed);
    let files_done = s.files_done.load(Ordering::Relaxed);
    let skipped = s.files_skipped.load(Ordering::Relaxed);
    let elapsed = st.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    // Speed and ETA from freshly-written bytes only — carried-forward bytes are already
    // on the disk and must not inflate the rate or shorten the estimate falsely.
    let rate = if elapsed > 0.0 { fresh as f64 / elapsed } else { 0.0 };
    let finished = s.finished.load(Ordering::Acquire);

    let verb = if st.mode == Mode::Copy { "Copying" } else { "Moving" };
    ui.label(
        RichText::new(verb)
            .font(FontId::new(27.0, FontFamily::Proportional))
            .strong()
            .color(p.ink),
    );

    // Before the first file is known, the manifest is still being built. Four zeros and a
    // dead bar read as "crashed", so show an explicit, moving-looking pre-flight instead.
    if files_total == 0 && !finished {
        ui.add_space(16.0);
        ui.label(
            RichText::new("Looking through your files…")
                .font(FontId::new(15.0, FontFamily::Proportional))
                .color(p.ink),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new("Large folders can take a moment before the copy begins.")
                .font(FontId::new(13.0, FontFamily::Proportional))
                .color(p.ink_2),
        );
        return;
    }

    ui.add_space(4.0);
    let cur = s.current.lock().map(|c| c.clone()).unwrap_or_default();
    let dirs_done = s.dirs_done.load(Ordering::Relaxed);
    let dirs_total = s.dirs_total.load(Ordering::Relaxed);
    ui.label(
        RichText::new(if !cur.is_empty() {
            cur
        } else if dirs_total > 0 {
            // Folder creation is real drive work that moves no file bytes; without its
            // own line, tens of thousands of folders at 0 MB/s read as a hang.
            format!("making folders on the drive — {dirs_done} of {dirs_total}")
        } else {
            "starting…".into()
        })
        .font(FontId::new(13.0, FontFamily::Monospace))
        .color(p.ink_3),
    );
    ui.add_space(20.0);

    // Progress bar, drawn by hand so the fill is exactly the brand teal.
    let frac = if total > 0 { (done as f32 / total as f32).clamp(0.0, 1.0) } else { 0.0 };
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 12.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, Rounding::same(6.0), p.sunk);
    if frac > 0.0 {
        let mut fill = rect;
        fill.set_width(rect.width() * frac.max(0.01));
        ui.painter().rect_filled(fill, Rounding::same(6.0), p.teal_bright);
    }
    ui.add_space(10.0);

    // A resume re-reads the bytes already on the drive before writing a single new one.
    // On a huge file that is minutes to hours, during which speed and ETA are honestly
    // zero — so say what is actually happening instead of showing a frozen-looking bar.
    let checking = s.checking.load(Ordering::Relaxed);
    let deleting_total = s.deleting_total.load(Ordering::Relaxed);
    let deleted = s.deleted.load(Ordering::Relaxed);
    // Prefer the engine's two-population estimate: it knows that 200k small files and
    // 70 GB of large ones move at unrelated speeds. Fall back to the plain byte
    // extrapolation until it has sampled enough to speak.
    let eng_secs = s.eta_secs.load(Ordering::Relaxed);
    let left = if eng_secs > 0 {
        Some(format!("{} left", eta_words(eng_secs as u64)))
    } else {
        eta(total.saturating_sub(done), rate)
    };
    ui.label(
        if deleting_total > 0 {
            // Named explicitly, or "it says copied but my originals are still there"
            // reads as a stall.
            RichText::new(format!(
                "everything is copied and checked — removing the originals, {deleted} of {deleting_total}"
            ))
            .font(FontId::new(13.0, FontFamily::Monospace))
            .color(p.teal)
        } else if checking > 0 {
            RichText::new(format!(
                "checking what's already on the drive — {} verified",
                human(checking)
            ))
            .font(FontId::new(13.0, FontFamily::Monospace))
            .color(p.teal)
        } else {
            RichText::new(left.unwrap_or_default())
                .font(FontId::new(13.0, FontFamily::Monospace))
                .color(p.ink_3)
        },
    );

    // The itemised estimate: the two populations move at unrelated speeds, and saying
    // so is the difference between "why does 100 GB say 5 hours at 2 MB/s" and an
    // answer. Only when both parts exist and the engine actually knows.
    let small_left = s.eta_small_left.load(Ordering::Relaxed);
    let small_secs = s.eta_small_secs.load(Ordering::Relaxed);
    let big_bytes = s.eta_big_bytes.load(Ordering::Relaxed);
    let big_secs = s.eta_big_secs.load(Ordering::Relaxed);
    if !finished
        && deleting_total == 0
        && checking == 0
        && small_left > 0
        && big_bytes > 0
        && small_secs > 0
        && big_secs > 0
    {
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "{} small files ({}), then {} in larger files ({})",
                thousands(small_left),
                eta_words(small_secs as u64),
                human(big_bytes),
                eta_words(big_secs as u64),
            ))
            .font(FontId::new(12.5, FontFamily::Monospace))
            .color(p.ink_3),
        );
    }
    ui.add_space(12.0);

    // Live figures are monospace so the digits do not jitter as they update.
    let mono = |t: String, size: f32, c: Color32| {
        RichText::new(t).font(FontId::new(size, FontFamily::Monospace)).color(c)
    };
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(mono(speed(rate), 30.0, p.ink).strong());
            ui.label(
                RichText::new("speed")
                    .font(FontId::new(11.0, FontFamily::Monospace))
                    .color(p.ink_3),
            );
        });
        ui.add_space(34.0);
        ui.vertical(|ui| {
            ui.label(mono(format!("{} / {}", human(done), human(total)), 30.0, p.ink).strong());
            ui.label(
                RichText::new("copied")
                    .font(FontId::new(11.0, FontFamily::Monospace))
                    .color(p.ink_3),
            );
        });
    });
    ui.add_space(14.0);
    // Smoothed files-per-second. During a small-file stretch the MB/s sits at 1–5 and
    // looks like a fault; files-per-second is the number that is actually moving.
    match st.file_rate_prev {
        Some((t0, n0)) => {
            let dt = t0.elapsed().as_secs_f64();
            if dt >= 1.0 {
                let sample = files_done.saturating_sub(n0) as f64 / dt;
                st.file_rate =
                    if st.file_rate > 0.0 { 0.7 * st.file_rate + 0.3 * sample } else { sample };
                st.file_rate_prev = Some((Instant::now(), files_done));
            }
        }
        None => st.file_rate_prev = Some((Instant::now(), files_done)),
    }
    let fps = if !finished && st.file_rate >= 1.0 {
        format!(" · {:.0} files a second", st.file_rate)
    } else {
        String::new()
    };
    ui.label(mono(
        // One accounting rule for the whole screen: like the byte bar, the file count
        // includes what earlier runs already proved. "768 of 3039" on a job that is
        // three-quarters done read as bad news; "2333 of 3039" is the truth.
        if skipped > 0 {
            format!(
                "{} of {files_total} files · {skipped} from earlier runs, checked{fps}",
                files_done + skipped
            )
        } else {
            format!("{files_done} of {files_total} files{fps}")
        },
        13.0,
        p.ink_2,
    ));

    // Failures build up in view, not as an ambush on the final screen. The copy is
    // still going — the wording must say so, or a growing count reads as "it's dying"
    // and invites a needless Stop.
    let errs = s.errors.load(Ordering::Relaxed);
    if errs > 0 && !finished {
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "{} couldn't be read so far — the copy keeps going; the full list comes \
                 at the end",
                plural(errs, "file")
            ))
            .font(FontId::new(13.0, FontFamily::Monospace))
            .color(p.amber),
        );
    }

    // The creative way out of the small-file swamp: when a big share of the manifest is
    // app caches and they were not left out, say so mid-copy — Stop, tick the box,
    // start again. Resume keeps everything already copied, so the advice costs nothing.
    let cache_files = s.cache_files.load(Ordering::Relaxed);
    let remaining = files_total.saturating_sub(files_done + skipped);
    if !finished && cache_files > 10_000 && remaining > 20_000 && cache_files * 3 > files_total {
        ui.add_space(12.0);
        ui.label(
            RichText::new(format!(
                "This can finish much sooner: {} of these files are app caches — browser \
                 data, AppData and similar, which apps rebuild on their own. Stop, tick \
                 \"Leave out app caches\", and start again; everything already copied \
                 is kept.",
                thousands(cache_files as u64)
            ))
            .font(FontId::new(13.0, FontFamily::Proportional))
            .color(p.amber),
        );
    }

    ui.add_space(24.0);
    let stopping = s.stopping.load(Ordering::Relaxed);
    if stopping {
        // The click was heard; cancel is polled between files, so acknowledge it rather
        // than leave a live-looking button the user hammers.
        ui.add_sized(
            [150.0, 40.0],
            egui::Button::new(RichText::new("Stopping…").size(14.0).color(p.ink_3))
                .fill(p.sunk)
                .stroke(Stroke::new(1.0, p.line)),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new("Finishing the current file safely, then stopping. Nothing is lost.")
                .font(FontId::new(13.0, FontFamily::Proportional))
                .color(p.ink_2),
        );
    } else {
        if ui
            .add_sized(
                [128.0, 40.0],
                egui::Button::new(RichText::new("Stop").size(14.0).color(p.ink))
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.line)),
            )
            .clicked()
        {
            s.cancel.store(true, Ordering::Relaxed);
            s.stopping.store(true, Ordering::Relaxed);
        }
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Stopping is safe — so is unplugging, and so is the computer shutting off. \
                 Nothing already written is ever lost; Kevat offers to continue next time.",
            )
            .font(FontId::new(13.0, FontFamily::Proportional))
            .color(p.ink_2),
        );
    }

    if finished {
        st.screen = Screen::Done;
        // Release the wake-lock the moment the work is over, not when the window closes.
        st.awake = None;
    }
}

fn done(ui: &mut egui::Ui, st: &mut Ui, p: &Palette) {
    let outcome = st.shared.outcome.lock().ok().and_then(|o| o.clone_summary());
    let have_pair = st.src.is_some() && st.dst.is_some();
    // Offering "Continue" only makes sense when the run left something unfinished and the
    // same source/destination are still on hand to resume from.
    let mut can_continue = false;
    // Copy mode with per-file failures: offer to re-run with exactly those files left
    // out, so a wall of unreadable cloud placeholders doesn't hold the job hostage.
    // Copy only — in a move this would delete the originals of everything that DID
    // copy, a decision that deserves more ceremony than one button.
    let mut finish_without: Vec<PathBuf> = Vec::new();

    match &outcome {
        Some(Ok(sum)) => {
            // Whether the loop actually broke early — not whether Stop was ever pressed.
            // Reading the flag here called a transfer that finished its last file after
            // the press "Stopped", offered Continue, and that Continue found no journal
            // (the run completed, so it was removed) and silently re-copied everything.
            let stopped = sum.stopped;
            let has_errors = !sum.errors.is_empty();
            can_continue = have_pair && (stopped || has_errors);
            if has_errors && !stopped && st.mode == Mode::Copy && have_pair {
                finish_without = sum.errors.iter().map(|(rel, _)| rel.clone()).collect();
            }

            // The heading must not say "Copied" in 27pt when files failed — the big word
            // is all a hurried person reads before ejecting.
            let (heading, head_color) = if has_errors {
                (
                    format!("{} files couldn't be copied", sum.errors.len()),
                    p.amber,
                )
            } else if stopped {
                ("Stopped".to_string(), p.ink)
            } else if st.mode == Mode::Copy {
                ("Copied".to_string(), p.ink)
            } else {
                ("Moved".to_string(), p.ink)
            };
            ui.label(
                RichText::new(heading)
                    .font(FontId::new(27.0, FontFamily::Proportional))
                    .strong()
                    .color(head_color),
            );
            ui.add_space(10.0);
            let rate = if sum.elapsed_secs > 0.0 {
                sum.bytes_written as f64 / sum.elapsed_secs
            } else {
                0.0
            };
            let line = |ui: &mut egui::Ui, t: String| {
                ui.label(
                    RichText::new(t)
                        .font(FontId::new(14.0, FontFamily::Monospace))
                        .color(p.ink_2),
                );
            };
            line(ui, format!("{} written", plural(sum.files_copied, "file")));
            if sum.files_skipped > 0 {
                line(ui, format!("{} already there", plural(sum.files_skipped, "file")));
            }
            if sum.files_verified > 0 {
                line(ui, format!("{} checked after writing", plural(sum.files_verified, "file")));
            }
            if sum.kept_existing > 0 {
                line(ui, format!("{} left as they were on the drive", plural(sum.kept_existing, "file")));
            }
            // Entries the scan could not take are part of the outcome, not a footnote —
            // a clean "Copied" over silently omitted files is the kind of quiet lie the
            // rest of this app refuses to tell.
            let scan_skipped = st.shared.scan_skipped.lock().map(|s| s.clone()).unwrap_or_default();
            if !scan_skipped.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("{} left out", plural(scan_skipped.len(), "item")))
                    .font(FontId::new(14.0, FontFamily::Proportional))
                    .color(p.amber),
                );
                for (path, why) in scan_skipped.iter().take(5) {
                    ui.label(
                        RichText::new(format!("  {}: {why}", shorten(path, 52)))
                            .font(FontId::new(12.0, FontFamily::Monospace))
                            .color(p.ink_3),
                    );
                }
                if scan_skipped.len() > 5 {
                    ui.label(
                        RichText::new(format!("  …and {} more", scan_skipped.len() - 5))
                            .font(FontId::new(12.0, FontFamily::Monospace))
                            .color(p.ink_3),
                    );
                }
            }
            if sum.sources_deleted > 0 {
                line(ui, format!("{} removed", plural(sum.sources_deleted, "original")));
            }
            line(
                ui,
                format!("{} in {:.1}s ({})", human(sum.bytes_written), sum.elapsed_secs, speed(rate)),
            );

            // Where it went — the actual folder, which for a folder copy is the one Kevat
            // created inside the destination.
            if let (Some(src), Some(dst)) = (&st.src, &st.dst) {
                // A resumed job's dst is already effective — deriving again would
                // display a doubly-nested path that doesn't exist.
                let landed = if st.exact_dst { dst.clone() } else { effective_dst(src, dst) };
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("saved to {}", shorten(&landed, 58)))
                        .font(FontId::new(13.0, FontFamily::Monospace))
                        .color(p.ink_3),
                );
            }

            if has_errors {
                ui.add_space(12.0);
                for (path, err) in sum.errors.iter().take(6) {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    ui.label(
                        RichText::new(format!("  {name} — {}", humanize_error(err)))
                            .font(FontId::new(12.5, FontFamily::Proportional))
                            .color(p.ink_2),
                    );
                }
                if sum.errors.len() > 6 {
                    ui.label(
                        RichText::new(format!("  …and {} more", sum.errors.len() - 6))
                            .font(FontId::new(12.5, FontFamily::Proportional))
                            .color(p.ink_3),
                    );
                }
            }

            // The two things a person reaches for once a copy to a USB drive finishes:
            // look at what landed, and unplug it safely — only when it succeeded cleanly.
            if !can_continue {
                if let (Some(src), Some(dst)) = (st.src.clone(), st.dst.clone()) {
                    let landed =
                        if st.exact_dst { dst.clone() } else { effective_dst(&src, &dst) };
                    // The drive to safely remove is whichever side is removable: the
                    // destination after a forward copy — the SOURCE after a reverse one,
                    // where the user just read (or moved) off the drive and wants it out.
                    let eject_target = if dest_is_removable(&dst) {
                        Some(dst.clone())
                    } else if dest_is_removable(&src) {
                        Some(src.clone())
                    } else {
                        None
                    };
                    ui.add_space(20.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_sized(
                                [130.0, 38.0],
                                egui::Button::new(RichText::new("Open folder").size(14.0).color(p.ink))
                                    .fill(p.surface)
                                    .stroke(Stroke::new(1.0, p.line)),
                            )
                            .clicked()
                        {
                            reveal(&landed);
                        }
                        // Collect a finished worker's verdict before drawing anything.
                        if let Some(busy) = &st.eject_busy {
                            let done = busy.lock().ok().and_then(|mut r| r.take());
                            if let Some(result) = done {
                                st.eject_status = Some(result);
                                st.eject_busy = None;
                            } else {
                                ui.ctx().request_repaint_after(Duration::from_millis(300));
                            }
                        }
                        // Only for a genuine removable drive — never the system disk.
                        // Hidden while the worker runs and after a success: a second
                        // click on an ejected drive could only produce a confusing
                        // error over a true "safe to unplug".
                        let ejected_ok = matches!(st.eject_status, Some(Ok(_)));
                        if let Some(eject_path) = eject_target.clone().filter(|_| {
                            st.eject_busy.is_none() && !ejected_ok
                        }) {
                            if ui
                                .add_sized(
                                    [130.0, 38.0],
                                    egui::Button::new(RichText::new("Eject drive").size(14.0).color(p.ink))
                                        .fill(p.surface)
                                        .stroke(Stroke::new(1.0, p.line)),
                                )
                                .clicked()
                        {
                            // Off the UI thread: ejecting flushes, and a frozen window
                            // mid-flush is an invitation to yank the cable.
                            let slot: Arc<Mutex<Option<Result<String, String>>>> =
                                Arc::new(Mutex::new(None));
                            let worker = slot.clone();
                            let target = eject_path;
                            std::thread::spawn(move || {
                                // A panic in the platform layer must not leave the UI
                                // stuck on "Removing the drive safely…" forever with
                                // every button disabled.
                                let result = std::panic::catch_unwind(
                                    std::panic::AssertUnwindSafe(|| eject(&target)),
                                )
                                .unwrap_or_else(|_| {
                                    Err("something went wrong while removing the drive".to_string())
                                });
                                if let Ok(mut r) = worker.lock() {
                                    *r = Some(result);
                                }
                            });
                            st.eject_busy = Some(slot);
                            st.eject_status = None;
                            ui.ctx().request_repaint_after(Duration::from_millis(300));
                        }
                        }
                    });
                    if st.eject_busy.is_some() {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("Removing the drive safely…")
                                .font(FontId::new(13.0, FontFamily::Proportional))
                                .color(p.ink_2),
                        );
                    }
                    match &st.eject_status {
                        Some(Ok(msg)) => {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(msg)
                                    .font(FontId::new(13.0, FontFamily::Proportional))
                                    .color(p.teal),
                            );
                        }
                        Some(Err(msg)) => {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(format!("Could not remove it: {msg}"))
                                    .font(FontId::new(13.0, FontFamily::Proportional))
                                    .color(p.amber),
                            );
                        }
                        None => {}
                    }
                }
            }
        }
        Some(Err(e)) => {
            can_continue = have_pair;
            ui.label(
                RichText::new("It stopped early")
                    .font(FontId::new(27.0, FontFamily::Proportional))
                    .strong()
                    .color(p.ink),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(humanize_error(e))
                    .font(FontId::new(14.0, FontFamily::Proportional))
                    .color(p.amber),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("Nothing already written was lost — Continue picks up from the last proven point.")
                    .font(FontId::new(13.5, FontFamily::Proportional))
                    .color(p.ink_2),
            );
        }
        None => {
            ui.label(RichText::new("Finishing…").color(p.ink_2));
        }
    }

    ui.add_space(22.0);
    ui.horizontal(|ui| {
        // Continue is the headline action after an interruption: it re-runs the same
        // transfer, which resumes from the journal, skipping what is already done.
        if can_continue
            && ui
                .add_sized(
                    [140.0, 40.0],
                    egui::Button::new(
                        RichText::new("Continue")
                            .size(14.0)
                            .strong()
                            .color(theme::on_accent(p)),
                    )
                    .fill(p.teal_bright)
                    .rounding(Rounding::same(9.0)),
                )
                .clicked()
        {
            st.start();
            return;
        }
        // The way out when the failures are permanent (cloud placeholders from another
        // account, locked system files): re-run with exactly those files left out, so
        // the job can genuinely finish and say so. They are reported on the next Done
        // screen as "left out, as asked" — given up on, never hidden.
        if !finish_without.is_empty()
            && ui
                .add_sized(
                    [200.0, 40.0],
                    egui::Button::new(
                        RichText::new("Finish without those files").size(14.0).color(p.ink),
                    )
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.line))
                    .rounding(Rounding::same(9.0)),
                )
                .clicked()
        {
            st.skip_rels = finish_without.clone();
            st.start();
            return;
        }
        let (label, fill, text) = if can_continue {
            ("Copy something else", p.surface, p.ink)
        } else {
            ("Copy something else", p.teal_bright, theme::on_accent(p))
        };
        let mut btn = egui::Button::new(RichText::new(label).size(14.0).strong().color(text))
            .rounding(Rounding::same(9.0));
        btn = if can_continue {
            btn.fill(fill).stroke(Stroke::new(1.0, p.line))
        } else {
            btn.fill(fill)
        };
        // Never walk away from a running eject: leaving would orphan a worker that is
        // still unmounting and powering off, free to race a transfer started on the very
        // same drive moments later.
        let ejecting = st.eject_busy.is_some();
        if ui.add_enabled(!ejecting, btn.min_size(Vec2::new(176.0, 40.0))).clicked() {
            st.screen = Screen::Pick;
            st.shared = Arc::new(Shared::default());
            st.started = None;
            st.eject_status = None;
            st.eject_busy = None;
            st.skip_rels.clear();
            // A fresh pick starts from clean semantics: the resumed job's exact
            // destination must not leak into the next transfer's folder derivation.
            st.exact_dst = false;
        }
    });
}

/// Turn an engine error string into something a non-technical person can act on.
fn humanize_error(e: &str) -> String {
    let low = e.to_lowercase();
    if low.contains("verify failed") {
        "didn't copy correctly — Continue will redo it".to_string()
    } else if low.contains("permission denied") {
        "permission denied".to_string()
    } else if low.contains("no space") || low.contains("won't fit") {
        e.to_string()
    } else if low.contains("no such file") {
        "the file went away before it could be copied".to_string()
    } else if low.contains("source not removed") {
        "copied, but the original couldn't be deleted".to_string()
    } else {
        e.to_string()
    }
}

/// "1 file" / "3 files" — proper singular/plural for the done-screen summary.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// `Summary` is not `Clone`, and the UI reads it every frame while the worker still
/// owns the mutex — so copy the fields out rather than holding the lock or cloning.
trait CloneSummary {
    fn clone_summary(&self) -> Option<Result<Summary, String>>;
}

impl CloneSummary for Option<Result<Summary, String>> {
    fn clone_summary(&self) -> Option<Result<Summary, String>> {
        match self {
            None => None,
            Some(Err(e)) => Some(Err(e.clone())),
            Some(Ok(s)) => Some(Ok(Summary {
                stopped: s.stopped,
                files_copied: s.files_copied,
                files_skipped: s.files_skipped,
                kept_existing: s.kept_existing,
                files_verified: s.files_verified,
                bytes_written: s.bytes_written,
                sources_deleted: s.sources_deleted,
                elapsed_secs: s.elapsed_secs,
                errors: s.errors.clone(),
            })),
        }
    }
}

// ── winit plumbing ───────────────────────────────────────────────────────────

struct App {
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    painter: raster::Painter,
    ui: Ui,
    applied_dark: Option<bool>,
    /// When a frame asked to be repainted *after a delay* (the pick screen polls every
    /// couple of seconds for a hot-plugged drive), the deadline lands here and
    /// `about_to_wait` turns it into a `ControlFlow::WaitUntil`. Without this the delay
    /// was silently dropped and "plug it in and it will appear" needed a mouse wiggle.
    next_repaint: Option<Instant>,
}

impl App {
    fn new() -> Self {
        let egui_ctx = egui::Context::default();
        // egui bundles only a Latin face and, unlike a browser, does no system-font
        // fallback — so Devanagari renders as empty boxes. Register a system Devanagari
        // font (Noto, Lohit, Mangal, …) as a fallback if one is present; `deva` records
        // whether it worked, so the About text only shows केवट when it can be drawn.
        let deva = install_devanagari(&egui_ctx);
        App {
            window: None,
            surface: None,
            egui_ctx,
            egui_state: None,
            painter: raster::Painter::new(),
            ui: Ui::new(prefers_dark(), deva),
            applied_dark: None,
            next_repaint: None,
        }
    }

    fn redraw(&mut self) {
        let (Some(window), Some(state), Some(surface)) = (
            self.window.clone(),
            self.egui_state.as_mut(),
            self.surface.as_mut(),
        ) else {
            return;
        };

        // Re-apply the palette only when it actually changed; rebuilding Visuals every
        // frame would discard egui's style cache for no reason.
        if self.applied_dark != Some(self.ui.dark) {
            self.egui_ctx.set_visuals(theme::visuals(&self.ui.palette()));
            self.applied_dark = Some(self.ui.dark);
        }

        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return; // minimised
        };

        let raw_input = state.take_egui_input(&window);
        let screen_before = self.ui.screen;
        let ui_ref = &mut self.ui;
        let output = self.egui_ctx.run(raw_input, |ctx| draw(ctx, ui_ref));
        state.handle_platform_output(&window, output.platform_output);

        self.painter.update_textures(&output.textures_delta);
        let ppp = output.pixels_per_point;
        let primitives = self.egui_ctx.tessellate(output.shapes, ppp);

        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };

        let ground = self.ui.palette().ground;
        let clear = ((ground.r() as u32) << 16) | ((ground.g() as u32) << 8) | ground.b() as u32;
        buffer.fill(clear);
        self.painter.paint(
            &mut buffer,
            size.width as usize,
            size.height as usize,
            ppp,
            &primitives,
        );
        let _ = buffer.present();

        // While a transfer runs, tick at 10 fps. Otherwise repaint when egui asks —
        // immediately for a zero delay, and via a scheduled wake-up for a timed one
        // (the pick screen's drive polling). A screen change discovered *during* this
        // frame (Running flips to Done inside draw()) also needs one more frame, or the
        // just-painted buffer — still showing "Copying" — would sit until the next
        // input event and read as a hang.
        self.next_repaint = None;
        if matches!(self.ui.screen, Screen::Running) {
            window.request_redraw();
            std::thread::sleep(TICK);
        } else if self.ui.screen != screen_before
            || output.viewport_output.values().any(|v| v.repaint_delay.is_zero())
        {
            window.request_redraw();
        } else if let Some(delay) =
            output.viewport_output.values().map(|v| v.repaint_delay).min()
        {
            if delay != Duration::MAX {
                self.next_repaint = Instant::now().checked_add(delay);
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window_icon = icon::window().and_then(|img| {
            winit::window::Icon::from_rgba(img.rgba, img.width, img.height).ok()
        });
        let attrs = Window::default_attributes()
            .with_title(concat!("Kevat ", env!("CARGO_PKG_VERSION")))
            .with_window_icon(window_icon)
            // Tall enough that the picker — header, columns, list, and its action row —
            // fits without the bottom buttons being clipped, which the old 560 did.
            .with_inner_size(winit::dpi::LogicalSize::new(820.0, 720.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(640.0, 600.0));
        // Pin the X11 WM_CLASS and Wayland app_id to "kevat" explicitly. The desktop
        // entry installed by install.sh binds the running window to its launcher via
        // StartupWMClass (X11) and via the .desktop file name (Wayland); winit's
        // default is derived from the executable name, which would break the binding
        // if the binary were ever renamed or run through a wrapper. Both ext traits
        // define with_name, so the calls are fully qualified to disambiguate.
        #[cfg(all(unix, not(target_os = "macos")))]
        let attrs = {
            use winit::platform::{
                wayland::WindowAttributesExtWayland, x11::WindowAttributesExtX11,
            };
            let attrs = WindowAttributesExtX11::with_name(attrs, "kevat", "kevat");
            WindowAttributesExtWayland::with_name(attrs, "kevat", "kevat")
        };
        let Ok(window) = event_loop.create_window(attrs) else {
            event_loop.exit();
            return;
        };
        let window = Rc::new(window);

        let Ok(context) = softbuffer::Context::new(window.clone()) else {
            event_loop.exit();
            return;
        };
        let Ok(surface) = softbuffer::Surface::new(&context, window.clone()) else {
            event_loop.exit();
            return;
        };

        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            None,
            None,
            None,
        ));
        self.surface = Some(surface);
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if let Some(state) = self.egui_state.as_mut() {
            let response = state.on_window_event(&window, &event);
            if response.repaint {
                window.request_redraw();
            }
        }

        match event {
            WindowEvent::CloseRequested => {
                // A transfer in flight is told to stop rather than being killed with the
                // process: it will finish the file it is on and leave a clean journal.
                self.ui.shared.cancel.store(true, Ordering::Relaxed);
                event_loop.exit();
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    // Runs after each batch of events. This is where a frame's "repaint me in N
    // seconds" becomes a real OS timer: sleep until the deadline (WaitUntil), and once
    // it passes, request the redraw and drop back to plain Wait.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.next_repaint {
            Some(at) if Instant::now() >= at => {
                self.next_repaint = None;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// A Devanagari font, bundled so केवट and राम render identically on every platform rather
/// than depending on whatever the system happens to ship — macOS in particular only offers
/// its Devanagari faces as `.ttc` collections egui cannot load. Registered as a fallback
/// (appended to each family), so it is consulted only for glyphs the Latin face lacks;
/// Latin text is untouched. Licensed separately under OFL 1.1 — see assets/fonts/OFL.txt.
const DEVANAGARI_FONT: &[u8] = include_bytes!("../../assets/fonts/NotoSansDevanagari-Regular.ttf");

/// Install the bundled Devanagari fallback. Always succeeds (the font ships in the binary),
/// so the About panel can show केवट/राम everywhere. Returns true for symmetry with callers.
fn install_devanagari(ctx: &egui::Context) -> bool {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "devanagari".to_owned(),
        egui::FontData::from_static(DEVANAGARI_FONT),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("devanagari".to_owned());
    }
    ctx.set_fonts(fonts);
    true
}

fn prefers_dark() -> bool {
    // No portal round-trip and no zbus: honour an explicit override if the user
    // set one, otherwise default to light, which is the design system's primary theme.
    match std::env::var("KEVAT_THEME").as_deref() {
        Ok("dark") => true,
        Ok("light") => false,
        _ => false,
    }
}

pub fn launch() -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|e| format!("cannot start the window system: {e}"))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new();
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("window loop failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_is_coarse_and_honest() {
        // No rate yet, or nothing left → nothing to promise.
        assert_eq!(eta(1_000_000, 0.0), None);
        assert_eq!(eta(0, 5_000_000.0), None);
        // Under a minute is never given false precision.
        assert_eq!(eta(1_000_000, 10_000_000.0).unwrap(), "less than a minute left");
        // Singular vs plural minutes, rounded.
        assert_eq!(eta(60_000_000, 1_000_000.0).unwrap(), "about 1 minute left");
        assert_eq!(eta(150_000_000, 1_000_000.0).unwrap(), "about 3 minutes left"); // 150s → 2.5 → 3
        // Hours for the long haul.
        assert!(eta(9_000_000_000, 1_000_000.0).unwrap().starts_with("about 2 h"));
    }

    #[test]
    fn human_uses_decimal_drive_units() {
        // Decimal, to match what a drive's label says — a "64 GB" stick, not 59.6 GiB.
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1500), "1.5 kB");
        assert_eq!(human(5_000_000), "5.0 MB");
        assert_eq!(human(64_000_000_000), "64.0 GB");
    }

    #[test]
    fn speed_switches_to_gb_when_fast() {
        assert_eq!(speed(120_000_000.0), "120 MB/s");
        assert_eq!(speed(2_500_000_000.0), "2.5 GB/s");
    }

    #[test]
    fn plural_is_grammatical() {
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(3, "file"), "3 files");
    }
}
