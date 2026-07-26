//! The resume journal. Append-only JSONL in the OS config directory — never on the
//! destination drive it exists to survive the loss of.
//!
//! Two ordering rules are load-bearing:
//!   * **J-after-D** — a record is only made durable after the data it asserts is durable.
//!     Callers must `fsync` the destination before handing a record here for commit.
//!   * A torn final line (power loss mid-write) is discarded on load, never parsed.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_128;

/// Group-commit thresholds. One fsync per ~512 files is roughly a 60 KB write, so the
/// tighter bound costs nothing measurable and halves the redo window
/// (these supersede the looser 1000 files / 128 MiB figures from an earlier draft).
const COMMIT_FILES: usize = 512;
const COMMIT_BYTES: u64 = 64 * 1024 * 1024;
const COMMIT_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Copy,
    Move,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Record {
    /// Written once, first line. `v` lets a future Kevat refuse or migrate an old journal.
    #[serde(rename = "session")]
    Session {
        v: u32,
        src: String,
        dst: String,
        mode: Mode,
        files: usize,
        bytes: u64,
        /// Multi-selection: the chosen names under `src`, in selection order (the
        /// job-tag hash is order-sensitive). Empty = the whole folder. Without this,
        /// the resume card could only offer `src` itself — Continue silently widened
        /// a three-folder selection into a whole-parent copy, and in move mode a
        /// whole-parent *delete*. Serde default keeps old journals readable.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        names: Vec<String>,
        /// The scan-filter choices the job ran with, so Continue re-applies them
        /// instead of silently rescanning everything the user chose to leave out.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        skip_caches: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        skip_cloud: bool,
    },
    /// One per completed file. `hash` is xxh3-128 of the source plaintext, hex.
    #[serde(rename = "file-done")]
    FileDone {
        rel: String,
        size: u64,
        mtime: i64,
        /// Nanosecond mtime of the *source* when it was copied. The seconds field
        /// alone let a same-size edit landing in the same second pass as unchanged —
        /// and in move mode the skip branch unlinks sources, so that mistake deleted
        /// the edited file while the destination kept the stale bytes. Defaulted so
        /// an older journal parses; 0 is treated as unproven, like the checkpoint's.
        #[serde(default)]
        mtime_ns: i64,
        hash: String,
    },
    /// Progress inside the file currently in flight.
    #[serde(rename = "checkpoint")]
    Checkpoint {
        rel: String,
        /// Bytes durably written to the destination.
        off: u64,
        /// Length of the trailing span covered by `span_hash`.
        span: u64,
        /// xxh3-128 of destination bytes `[off - span, off)`, hex. Validated against the
        /// medium on resume — a yanked drive corrupts the tail, so an offset alone proves
        /// nothing (invariant 4).
        span_hash: String,
        /// Size and mtime of the *source* when this checkpoint was taken.
        ///
        /// Without these, resume splices whatever the source is now onto bytes copied
        /// from what it used to be, producing a destination that matches no version of
        /// the file that ever existed — and reports success. Defaulted so a journal
        /// written by an older build still parses; a checkpoint claiming size 0 is
        /// treated as unproven rather than trusted.
        ///
        /// `src_mtime` is in **nanoseconds**. Whole seconds let a same-size source
        /// regenerated within the same second pass as unchanged; a seconds-valued
        /// checkpoint from an older journal fails the match and the file restarts —
        /// bounded redo, never a splice.
        #[serde(default)]
        src_size: u64,
        #[serde(default)]
        src_mtime: i64,
    },
    /// Move mode only: every file is copied, verified and durable on the destination —
    /// the point at which deleting sources becomes permissible. Written once, after a
    /// destination-wide durability barrier, before the first unlink. Its presence is
    /// what tells a resume it is in the delete phase rather than still copying.
    #[serde(rename = "all-copied")]
    AllCopied,
    /// Move mode only, after the destination copy is verified and durable.
    #[serde(rename = "source-deleted")]
    SourceDeleted { rel: String },
    /// Job finished cleanly; the journal may be removed.
    #[serde(rename = "complete")]
    Complete,
}

pub const JOURNAL_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct DoneFile {
    pub size: u64,
    pub mtime: i64,
    /// Nanosecond source mtime; 0 when the journal predates the field (unproven).
    pub mtime_ns: i64,
    pub hash: String,
}

/// One checkpoint of the file in flight, as loaded back from the journal.
#[derive(Debug, Clone)]
pub struct Ckpt {
    pub off: u64,
    pub span: u64,
    pub span_hash: String,
    pub src_size: u64,
    pub src_mtime: i64,
}

/// The in-flight file's checkpoint *chain*, oldest first: each span starts where the
/// previous one ended, so together they cover the destination from the first span's
/// start to the last offset. Keeping only the newest checkpoint — which is what used
/// to happen — left everything below its span trusted blind on resume, a region that
/// grew 64 MiB per checkpoint.
#[derive(Debug)]
pub struct Partial {
    pub rel: String,
    pub chain: Vec<Ckpt>,
}

/// What a previous run left behind.
#[derive(Debug, Default)]
pub struct ResumeState {
    pub done: HashMap<String, DoneFile>,
    pub deleted: Vec<String>,
    /// Checkpoint chain for the file that was in flight, if any.
    pub partial: Option<Partial>,
    pub complete: bool,
    /// Phase 2 was reached: every file is proven on the destination and the run owes
    /// only source deletions. Absent in journals from before two-phase move, which
    /// therefore resume as ordinary copies — the safe direction.
    pub all_copied: bool,
    /// True if the final line was truncated — expected after a hard kill, not an error.
    pub torn_tail: bool,
    pub session: Option<(String, String, Mode)>,
    /// Totals from the session line, for "N of M files" in the resume offer.
    pub session_files: usize,
    pub session_bytes: u64,
    /// Selection and filter choices from the session line — what Continue must
    /// reproduce for the resumed run to be the same job.
    pub session_names: Vec<String>,
    pub session_skip_caches: bool,
    pub session_skip_cloud: bool,
}

pub struct Journal {
    path: PathBuf,
    file: File,
    /// Buffered records not yet fsync'd.
    pending: Vec<u8>,
    files_since_commit: usize,
    bytes_since_commit: u64,
    last_commit: Instant,
}

/// Read an environment variable, accepting it only if it names an **absolute** path.
///
/// Emptiness and relativity are the same hazard wearing two hats. `HOME=""` makes
/// `PathBuf::from("").join(".config/kevat")` the *relative* path `.config/kevat`, and a
/// relative journal directory resolves against the working directory — which, for
/// someone who ran `cd /media/usb && kevat ~/data .`, is the destination drive. That
/// puts the journal on the very medium it exists to survive the loss of.
fn absolute_env(key: &str) -> Option<PathBuf> {
    let raw = std::env::var_os(key)?;
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        Some(p)
    } else {
        None
    }
}

fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = absolute_env("APPDATA") {
            return appdata.join("kevat");
        }
    }
    if let Some(x) = absolute_env("XDG_CONFIG_HOME") {
        return x.join("kevat");
    }
    if let Some(home) = absolute_env("HOME") {
        #[cfg(target_os = "macos")]
        return home.join("Library/Application Support/kevat");
        #[cfg(not(target_os = "macos"))]
        return home.join(".config/kevat");
    }
    // Last resort, and deliberately absolute. A journal in the temp directory may be
    // swept between reboots, costing a resume; a journal written relative to the
    // working directory can land on the destination drive, costing the invariant.
    std::env::temp_dir().join("kevat")
}

/// Absolute, lexically normalised form of `p` — `.` dropped, `..` folded, relative
/// paths joined onto the working directory. Purely textual on purpose; the symlink
/// resolution happens in `stable_key`, which can only do it for parts that exist.
pub fn lexical_abs(p: &Path) -> PathBuf {
    use std::path::Component;
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A key for `p` that does not change when `p` comes into existence.
///
/// `fs::canonicalize` fails on a path that is not there yet, and falling back to the
/// path as written is what made this unstable: the first run keys the destination on
/// `dst`, the resume keys it on `/abs/dst` once the directory exists, no journal is
/// found under the new key, and the whole transfer is silently copied again. Same
/// trap for any path spelled through a symlink.
///
/// So: canonicalise the deepest ancestor that does exist, then re-attach the names
/// below it. Before and after creation, that yields the same string.
pub fn stable_key(p: &Path) -> String {
    let abs = lexical_abs(p);
    let mut probe = abs.as_path();
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(c) = fs::canonicalize(probe) {
            let mut out = c;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out.to_string_lossy().into_owned();
        }
        match (probe.parent(), probe.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                probe = parent;
            }
            // Reached a root that will not canonicalise. The lexical form is already
            // absolute and normalised, so it is a stable answer on its own.
            _ => return abs.to_string_lossy().into_owned(),
        }
    }
}

/// Key on source + destination + mode.
///
/// Note what this does *not* do: it does not key on the volume's identity. Remount the
/// destination at a different path — a new drive letter on Windows, a different
/// `/media` mount point on Linux — and the key changes, no journal is found, and the
/// transfer starts from zero. Keying by volume UUID so that cannot happen is designed
/// but not implemented here.
pub fn journal_path(src: &Path, dst: &Path, mode: Mode, tag: Option<&str>) -> PathBuf {
    // `tag` distinguishes two different multi-selections made in the SAME folder: both
    // have that folder as their source path, so without it they would share a journal
    // and each would try to resume the other's work.
    let key = format!(
        "{}\u{0}{}\u{0}{:?}\u{0}{}",
        stable_key(src),
        stable_key(dst),
        mode,
        tag.unwrap_or("")
    );
    let h = xxh3_128(key.as_bytes());
    config_dir()
        .join("journals")
        .join(format!("{:032x}.jsonl", h))
}

// ── transfer history ─────────────────────────────────────────────────────────
//
// One JSON line per finished run, appended to <config>/history.jsonl. Strictly local —
// this file never leaves the machine, so the no-network/no-telemetry claim is
// untouched. Best-effort by design: history failing to write must never fail a
// transfer that succeeded.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unix seconds when the run ended.
    pub at: i64,
    pub src: String,
    pub dst: String,
    pub mode: Mode,
    /// Files copied by THIS run (a resume that skipped most of the tree shows the
    /// small remainder it actually did, which is the honest number).
    pub copied: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub secs: f64,
    pub errors: usize,
    /// True when the whole job finished (journal removed); false for a run that ended
    /// with errors or a Stop — the pending card carries those forward.
    pub done: bool,
}

pub fn history_path() -> PathBuf {
    config_dir().join("history.jsonl")
}

/// Append one line; caps the file at ~1000 entries by rewriting the newest 500 when it
/// grows past that, so a heavy user's history stays a few hundred KB forever.
pub fn history_append(e: &HistoryEntry) {
    let path = history_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let Ok(line) = serde_json::to_string(e) else { return };
    let entries = history();
    if entries.len() >= 1000 {
        let keep: Vec<String> = entries
            .iter()
            .rev()
            .take(499)
            .rev()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect();
        let _ = fs::write(&path, format!("{}\n{line}\n", keep.join("\n")));
        return;
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        use io::Write;
        let _ = writeln!(f, "{line}");
    }
}

/// All recorded runs, oldest first (file order). Unparseable lines are skipped.
pub fn history() -> Vec<HistoryEntry> {
    let Ok(text) = fs::read_to_string(history_path()) else {
        return Vec::new();
    };
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// An interrupted transfer: its journal still exists (a completed run removes its
/// journal), and the session line names the pair. This is what lets the GUI greet a
/// user after a crash or power cut with "continue where you left off?" instead of a
/// blank picker — without it, the resume machinery exists but is invisible, and an
/// interrupted transfer *looks* lost even though nothing is.
// Only the GUI surfaces pending transfers; scoped rather than blanket-allowed so a
// genuinely unused field in a GUI build still warns.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct Pending {
    pub src: PathBuf,
    /// The *effective* destination exactly as the engine ran it — already includes the
    /// named subfolder. Resume must feed this back untouched; re-deriving it would
    /// change the journal key and silently restart from zero.
    pub dst: PathBuf,
    pub mode: Mode,
    pub files_done: usize,
    pub files_total: usize,
    /// Bytes already proven done — "1 of 2 files" can mean 99% by bytes.
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Up to 20 relative paths the journal claims are done, for the same-drive check:
    /// if none of them exist under `dst`, the letter probably now names a different
    /// drive, and continuing blind would copy everything onto the wrong stick.
    pub sample: Vec<String>,
    /// The multi-selection and filter choices the job was started with — Continue must
    /// reproduce them exactly or it runs a different (wider) job under the same name.
    pub names: Vec<PathBuf>,
    pub skip_caches: bool,
    pub skip_cloud: bool,
}

/// Every interrupted transfer recorded on this machine, most recent first.
///
/// Read-only: this never deletes or repairs a journal. A journal that fails to parse
/// entirely is skipped here — the engine's own load() deals with torn tails when the
/// transfer is actually resumed.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn pending() -> Vec<Pending> {
    let dir = config_dir().join("journals");
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, Pending)> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(st) = load(&path) else { continue };
        // `complete` with the file still present means a remove() lost a race — not a
        // debt to surface.
        if st.complete {
            continue;
        }
        let Some((src, dst, mode)) = st.session else { continue };
        let at = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let bytes_done: u64 = st.done.values().map(|d| d.size).sum();
        let sample: Vec<String> = st.done.keys().take(20).cloned().collect();
        found.push((
            at,
            Pending {
                src: PathBuf::from(src),
                dst: PathBuf::from(dst),
                mode,
                files_done: st.done.len(),
                files_total: st.session_files,
                bytes_done,
                bytes_total: st.session_bytes,
                sample,
                names: st.session_names.iter().map(PathBuf::from).collect(),
                skip_caches: st.session_skip_caches,
                skip_cloud: st.session_skip_cloud,
            },
        ));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

pub fn load(path: &Path) -> io::Result<ResumeState> {
    let mut st = ResumeState::default();
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(st),
        Err(e) => return Err(e),
    };
    let rdr = BufReader::new(f);
    // Collect raw lines first: only a line terminated by \n was fully written.
    let mut lines: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    let mut rdr = rdr;
    loop {
        buf.clear();
        let n = rdr.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if buf.last() != Some(&b'\n') {
            // Torn tail from a hard kill mid-write. Discard it, do not parse.
            st.torn_tail = true;
            break;
        }
        lines.push(String::from_utf8_lossy(&buf[..buf.len() - 1]).into_owned());
    }

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let rec: Record = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => {
                st.torn_tail = true;
                continue;
            }
        };
        match rec {
            Record::Session { src, dst, mode, files, bytes, names, skip_caches, skip_cloud, .. } => {
                st.session = Some((src, dst, mode));
                st.session_files = files;
                st.session_bytes = bytes;
                st.session_names = names;
                st.session_skip_caches = skip_caches;
                st.session_skip_cloud = skip_cloud;
            }
            Record::FileDone {
                rel,
                size,
                mtime,
                mtime_ns,
                hash,
            } => {
                if st.partial.as_ref().map_or(false, |p| p.rel == rel) {
                    st.partial = None;
                }
                st.done.insert(rel, DoneFile { size, mtime, mtime_ns, hash });
            }
            Record::Checkpoint {
                rel,
                off,
                span,
                span_hash,
                src_size,
                src_mtime,
            } => {
                let ck = Ckpt { off, span, span_hash, src_size, src_mtime };
                match &mut st.partial {
                    // Contiguous with the chain's tip: the same in-flight copy has
                    // advanced another span. Keep every link — resume must be able to
                    // re-hash the whole covered prefix, not just the newest span.
                    Some(p)
                        if p.rel == rel
                            && p.chain
                                .last()
                                .map_or(false, |last| ck.off >= ck.span && ck.off - ck.span == last.off) =>
                    {
                        p.chain.push(ck)
                    }
                    // A different file, or the same file restarted from zero after a
                    // failed validation: the old chain describes bytes that are gone.
                    _ => st.partial = Some(Partial { rel, chain: vec![ck] }),
                }
            }
            Record::SourceDeleted { rel } => st.deleted.push(rel),
            Record::AllCopied => st.all_copied = true,
            Record::Complete => st.complete = true,
        }
    }
    Ok(st)
}

/// Take an exclusive, non-blocking lock on the journal.
///
/// Two runs of the same command share a journal *and* a `.kpart`. The second reads the
/// first's committed checkpoint, opens the file the first is still writing, and
/// `set_len`s it back down to that offset — punching a hole underneath the first
/// process's file position. Both then report success and the destination is silently
/// wrong. The journal is the natural place to arbitrate: one journal, one run.
#[cfg(unix)]
fn lock_exclusive(file: &File, busy: &str) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // Safety: the fd is owned by `file` and outlives this call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, busy));
    }
    Err(err)
}

/// The Windows equivalent of flock: `LockFileEx`, exclusive and fail-immediately.
/// The lock covers one byte at offset 0 — enough for arbitration, and it releases
/// with the handle, so a killed process never leaves a stale lock behind. Without
/// this, double-clicking the shortcut twice ran two engines against one journal and
/// one `.kpart` — the exact set_len-under-the-other's-position corruption above.
#[cfg(windows)]
fn lock_exclusive(file: &File, busy: &str) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    let mut ov: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut ov,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    // ERROR_LOCK_VIOLATION (33) is "someone already holds it" — the busy case.
    if err.raw_os_error() == Some(33) {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, busy));
    }
    Err(err)
}

#[cfg(not(any(unix, windows)))]
fn lock_exclusive(_file: &File, _busy: &str) -> io::Result<()> {
    Ok(())
}

/// Take the per-destination lock: one transfer per destination at a time.
///
/// The journal's own flock arbitrates one *journal* — but the journal is keyed on
/// (src, dst, mode), so two runs differing in any of the three shared the destination
/// unguarded: the same pair in different modes held the same `.kpart` open for write
/// twice, and two sources with a colliding relative path interleaved into one output
/// file, each run reporting success. The contended resource is the destination, so
/// this lock keys on the destination alone and is held for the whole run.
///
/// The lock file lives in the config dir, never on the destination drive
/// (invariant 3). It is deliberately never unlinked: removing it on exit races a
/// second process already holding the fd — a third could then create and lock a
/// fresh file at the same path and two runs would proceed. A few empty lock files
/// in the config dir are the cheap end of that trade.
pub fn lock_destination(dst: &Path) -> io::Result<File> {
    let dir = config_dir().join("locks");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{:032x}.lock", xxh3_128(stable_key(dst).as_bytes())));
    let file = OpenOptions::new().create(true).write(true).open(&path)?;
    lock_exclusive(&file, "another transfer is already writing to this destination")?;
    Ok(file)
}

/// Cut the file back to its last complete line.
///
/// `load` stops reading at a torn final line, but the file itself still ends mid-record.
/// Re-opening `O_APPEND` and writing would concatenate the next record onto those torn
/// bytes, destroying a record that *was* fsynced and is owed. Truncating first is what
/// makes "discard the torn tail" true on disk and not just in memory.
fn truncate_to_last_record(file: &File) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    let mut f = file;
    let mut buf = vec![0u8; len as usize];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut buf)?;
    match buf.iter().rposition(|b| *b == b'\n') {
        Some(i) => {
            let good = i as u64 + 1;
            if good < len {
                file.set_len(good)?;
            }
        }
        // Not one complete record in the file: nothing worth keeping.
        None => file.set_len(0)?,
    }
    Ok(())
}

impl Journal {
    pub fn create(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // read+write, not append: `truncate_to_last_record` below calls `set_len`, and on
        // Windows an append-mode handle lacks FILE_WRITE_DATA, so that call fails with
        // access-denied. The consequence was severe and Windows-only — a kill or power
        // cut mid-journal-write leaves a torn tail, and every later run of that transfer
        // then died here before copying a byte, bricking resume until the user deleted a
        // journal file they had no way to know about. (Linux's ftruncate accepts an
        // O_APPEND fd, which is why the suite never saw it.) The exclusive lock is held
        // for the whole run, so there is no concurrent writer and seek-to-end before each
        // write is exactly equivalent to append.
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        lock_exclusive(
            &file,
            "another Kevat run is already copying this source to this destination",
        )?;
        truncate_to_last_record(&file)?;
        Ok(Journal {
            path: path.to_path_buf(),
            file,
            pending: Vec::with_capacity(8 * 1024),
            files_since_commit: 0,
            bytes_since_commit: 0,
            last_commit: Instant::now(),
        })
    }

    /// Buffer a record. Not durable until `commit` — see J-after-D.
    pub fn append(&mut self, rec: &Record) -> io::Result<()> {
        serde_json::to_writer(&mut self.pending, rec)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        self.pending.push(b'\n');
        if let Record::FileDone { size, .. } = rec {
            self.files_since_commit += 1;
            self.bytes_since_commit += size;
        }
        Ok(())
    }

    /// True once any group-commit threshold is reached.
    pub fn should_commit(&self) -> bool {
        self.files_since_commit >= COMMIT_FILES
            || self.bytes_since_commit >= COMMIT_BYTES
            || self.last_commit.elapsed() >= COMMIT_INTERVAL
    }

    /// Flush and fsync. The caller must already have made the described data durable.
    pub fn commit(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            self.last_commit = Instant::now();
            return Ok(());
        }
        // Explicit seek: the handle is not in append mode (see `create`), and
        // `truncate_to_last_record` can leave the position past the truncated end.
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&self.pending)?;
        self.file.sync_data()?;
        self.pending.clear();
        self.files_since_commit = 0;
        self.bytes_since_commit = 0;
        self.last_commit = Instant::now();
        Ok(())
    }

    pub fn append_and_commit(&mut self, rec: &Record) -> io::Result<()> {
        self.append(rec)?;
        self.commit()
    }

    pub fn remove(self) -> io::Result<()> {
        let p = self.path.clone();
        drop(self.file);
        match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this file exists to prevent: the key must not depend on whether
    /// the destination has been created yet, or a resume silently re-copies everything.
    #[test]
    fn key_is_stable_across_creation() {
        let tmp = std::env::temp_dir().join(format!("kevat-key-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let dst = tmp.join("out");
        let before = stable_key(&dst);
        fs::create_dir_all(&dst).unwrap();
        let after = stable_key(&dst);
        assert_eq!(before, after, "key changed once the destination existed");

        let _ = fs::remove_dir_all(&tmp);
    }

    /// The journal must never resolve relative to the working directory: someone who
    /// runs `cd /media/usb && kevat ~/data .` under a service manager with no HOME
    /// would otherwise get the journal written onto the destination drive it exists to
    /// survive the loss of.
    #[test]
    fn config_dir_is_always_absolute() {
        assert!(config_dir().is_absolute(), "default environment");
        // Every degraded case must still land somewhere absolute.
        for (k, v) in [("HOME", ""), ("XDG_CONFIG_HOME", ""), ("HOME", "relative/path")] {
            let saved = std::env::var_os(k);
            std::env::set_var(k, v);
            let got = config_dir();
            match saved {
                Some(old) => std::env::set_var(k, old),
                None => std::env::remove_var(k),
            }
            assert!(got.is_absolute(), "{k}={v:?} produced {got:?}");
        }
    }

    /// A relative path and the same path spelled absolutely are the same destination.
    #[test]
    fn relative_and_absolute_agree() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(stable_key(Path::new("some/where")), stable_key(&cwd.join("some/where")));
        assert_eq!(stable_key(Path::new("./a/../b")), stable_key(&cwd.join("b")));
    }
}
