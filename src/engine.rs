//! The copy engine. v0.1 is the raw (uncompressed) path: one reader thread and one
//! writer thread joined by a bounded channel, which is the correct shape for a rotational
//! USB disk. Adaptive write queue depth and the compression workers land in v0.2.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::Instant;

use xxhash_rust::xxh3::Xxh3;

use crate::journal::{self, Journal, Mode, Record, ResumeState};
use crate::scan::{self, Manifest};

pub const CHUNK: usize = 8 * 1024 * 1024;
/// Depth of the bounded ring. Blocking on a full channel *is* the backpressure.
const RING: usize = 4;
/// Distance between checkpoints for a large file in flight.
const CHECKPOINT_EVERY: u64 = 64 * 1024 * 1024;
/// In-progress outputs carry this suffix and are renamed on completion, so a yanked
/// drive leaves unambiguous evidence of what finished.
pub const PART_SUFFIX: &str = ".kpart";

/// What to do when the destination already holds a file that differs from the source.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum OnExists {
    #[default]
    /// Write the source over it. The historical behaviour, and still the default for a
    /// backup — but never silently: both front ends state the count first.
    Replace,
    /// Leave the destination file alone and count it as skipped.
    Keep,
    /// Refuse the whole transfer before byte zero, naming the count.
    Fail,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub mode: Mode,
    /// Policy for destination files that already exist and differ from the source.
    /// Without this the answer was always "replace", with no mention anywhere — so a
    /// destination holding a *newer* edit was clobbered by an older source and the run
    /// reported success.
    pub on_exists: OnExists,
    /// Distinguishes jobs that share a source path — a multi-selection keys on the
    /// folder holding the chosen items, so the selection itself must enter the journal
    /// key or two different selections from one folder would collide. `None` for an
    /// ordinary whole-folder or single-file transfer.
    pub job_tag: Option<String>,
    pub verify: bool,
    pub paranoid: bool,
    pub dry_run: bool,
    /// Recorded into the journal's session line so a resume can reproduce the same
    /// job: the multi-selection names (empty = whole folder) and the scan-filter
    /// choices the manifest was built with. The engine itself never filters by these —
    /// the manifest it receives is already filtered; these are the resume contract.
    pub selection: Vec<String>,
    pub skip_caches: bool,
    pub skip_cloud: bool,
}

#[derive(Debug, Default)]
pub struct Summary {
    /// True only when the copy loop actually broke early on a cancel. Re-reading the
    /// cancel flag after the loop cannot tell that apart from Stop pressed during the
    /// last file, which marked a fully finished transfer "stopped — resumable".
    pub stopped: bool,
    pub files_copied: usize,
    pub files_skipped: usize,
    /// Destination files left untouched because they existed and differed.
    pub kept_existing: usize,
    pub files_verified: usize,
    pub bytes_written: u64,
    pub sources_deleted: usize,
    pub elapsed_secs: f64,
    pub errors: Vec<(PathBuf, String)>,
}

fn part_path(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_os_string();
    s.push(PART_SUFFIX);
    PathBuf::from(s)
}

/// A destination spelled with a trailing separator — `outdir/` — is an explicit
/// promise that the destination is a directory, honoured even before it exists.
/// Without this, a file source resolved `outdir/` to the *file* path `outdir/` and
/// the copy failed with a bare "No such file or directory" naming no path at all.
fn spelled_as_dir(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s.ends_with('/') || (cfg!(windows) && s.ends_with('\\'))
}

/// Copy one file. Returns (xxh3-128 of the source plaintext, bytes actually written).
///
/// `resume_from` is a destination offset already proven durable; the reader still streams
/// the whole source so the full-file hash stays a true hash of the source, while the
/// writer discards everything before the offset. Re-reading the source prefix is cheaper
/// than re-writing it, which is the point.
fn copy_file(
    src: &Path,
    dst_part: &Path,
    resume_from: u64,
    rel: &str,
    jr: &mut Journal,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> io::Result<(u128, u64, bool)> {
    let src_file = File::open(src)?;
    // Nanoseconds, not seconds: the checkpoint's identity check is the splice defense,
    // and a same-size source regenerated within the same second must still fail it.
    let (src_size, src_mtime) = match src_file.metadata() {
        Ok(md) => (md.len(), scan::mtime_ns_of(&md)),
        Err(_) => (0, 0),
    };

    let (tx, rx) = sync_channel::<io::Result<Vec<u8>>>(RING);
    let reader = thread::spawn(move || -> io::Result<u128> {
        let mut f = src_file;
        let mut h = Xxh3::new();
        loop {
            let mut buf = vec![0u8; CHUNK];
            let mut filled = 0;
            // read() may return short; fill the buffer so chunk boundaries stay aligned.
            while filled < CHUNK {
                match f.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return Err(io::Error::new(io::ErrorKind::Other, "source read failed"));
                    }
                }
            }
            if filled == 0 {
                break;
            }
            buf.truncate(filled);
            h.update(&buf);
            if tx.send(Ok(buf)).is_err() {
                break; // writer is gone; it owns the error
            }
        }
        Ok(h.digest128())
    });

    let mut out = if resume_from > 0 {
        let mut f = OpenOptions::new().write(true).open(dst_part)?;
        f.set_len(resume_from)?;
        f.seek(SeekFrom::Start(resume_from))?;
        f
    } else {
        File::create(dst_part)?
    };

    let mut pos: u64 = 0; // position in the source stream
    let mut written: u64 = 0;
    let mut last_ckpt = resume_from;
    let mut write_err: Option<io::Error> = None;
    // A checkpoint advances the trusted offset by 64 MiB, so its hash has to cover all
    // 64 MiB. Hashing only the final chunk left seven eighths of every span accepted
    // unread on resume — corruption anywhere in it was trusted forever.
    let mut span = Xxh3::new();
    let mut span_len: u64 = 0;

    let mut cancelled = false;
    for msg in rx.iter() {
        // Stop must be felt inside the file, not only between files: the workload this
        // product exists for is one 80 GB file, and polling only at boundaries meant
        // "Stopping…" sat there for the remaining 75 GB. Stopping mid-file is safe
        // because resume-from-checkpoint is exactly this situation — so stop, write a
        // checkpoint at the current position, and let resume pick it up.
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let chunk = match msg {
            Ok(c) => c,
            Err(e) => {
                write_err = Some(e);
                break;
            }
        };
        let start = pos;
        let end = pos + chunk.len() as u64;
        pos = end;

        if end <= resume_from {
            continue; // already durable on the destination
        }
        // A chunk straddling the resume point is written only from the offset onward.
        let from = resume_from.saturating_sub(start) as usize;
        let payload = &chunk[from..];

        if let Err(e) = out.write_all(payload) {
            write_err = Some(e);
            break;
        }
        written += payload.len() as u64;
        span.update(payload);
        span_len += payload.len() as u64;
        // Per-chunk rather than per-file: a multi-gigabyte file is the case this
        // product exists for, and a bar that only moves between files would sit
        // frozen for minutes on exactly the transfer the user is watching.
        on_bytes(payload.len() as u64);

        // Checkpoint: fsync the data first, then record it. J-after-D.
        if end - last_ckpt >= CHECKPOINT_EVERY {
            if let Err(e) = out.sync_data() {
                write_err = Some(e);
                break;
            }
            jr.append_and_commit(&Record::Checkpoint {
                rel: rel.to_string(),
                off: end,
                span: span_len,
                span_hash: format!("{:032x}", span.digest128()),
                src_size,
                src_mtime,
            })?;
            last_ckpt = end;
            span = Xxh3::new();
            span_len = 0;
        }
    }

    // A cancelled copy leaves the destination at a proven point: fsync what was
    // written, then record a checkpoint covering it, so resume restarts from here
    // rather than from the last 64 MiB boundary.
    if cancelled {
        out.sync_data()?;
        if span_len > 0 {
            jr.append_and_commit(&Record::Checkpoint {
                rel: rel.to_string(),
                off: resume_from + written,
                span: span_len,
                span_hash: format!("{:032x}", span.digest128()),
                src_size,
                src_mtime,
            })?;
        }
        // Drop the receiver so the reader's send fails and it returns, rather than
        // blocking forever on a full ring.
        drop(rx);
        let _ = reader.join();
        return Ok((0, written, true));
    }

    let hash = reader
        .join()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "reader thread panicked"))??;

    if let Some(e) = write_err {
        return Err(e);
    }

    out.sync_data()?;
    Ok((hash, written, false))
}

/// True when two paths name the same file on disk, however they are spelled.
///
/// String comparison of resolved paths cannot see *mount aliasing*: the same volume
/// visible at two places — `/media/user/USB` and an fstab bind at `/mnt/usb`, a Docker
/// volume, a `mount --bind` — canonicalises to two different strings that are one
/// inode. The kernel's answer is authoritative, so when both paths exist their
/// `(st_dev, st_ino)` pair decides. `fs::metadata` follows symlinks, which is wanted:
/// it is the final target's identity that matters. Either path missing means the
/// string comparison was the best available and already ran.
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

/// Windows has no equally cheap identity check — file IDs need an open handle per
/// path — so the string comparison in `check_paths` stands alone there. A bind-style
/// alias (a mounted folder seen under two drive letters) is not caught on Windows.
#[cfg(not(unix))]
fn same_file(_a: &Path, _b: &Path) -> bool {
    false
}

/// Refuse source/destination pairs that cannot be served safely.
///
/// The dangerous one is `src == dst` **in move mode**: every file is copied to
/// `name.kpart`, renamed over itself — still fine, the bytes are identical — and then
/// the "source" is unlinked. The source and the destination are the same file, so the
/// unlink deletes the only copy, and the run reports success while the data ceases to
/// exist. Comparing the resolved paths, not the strings the user typed, is what makes
/// this catch `./data`, a trailing slash, and a symlink pointing back at the same
/// place; comparing inodes (`same_file`) is what makes it catch a bind mount or any
/// other alias the path strings cannot express.
///
/// Destination-inside-source is refused too. It is not destructive, but the copy walks a
/// tree it is simultaneously writing into, and re-running the same command — which is
/// exactly how Kevat is meant to be resumed — nests the data one level deeper each time.
/// Definitive same-directory test that needs no platform file-identity API.
///
/// Windows has no cheap `(dev, ino)`: `std::os::windows::fs::MetadataExt`'s
/// `volume_serial_number` and `file_index` are still unstable, and the alternative is an
/// FFI dependency for one call. So ask the filesystem rather than the OS — drop a
/// uniquely named marker in the source and look for it in the destination. If it appears
/// there, the two are the same directory whatever the paths look like: a junction, a
/// `subst` drive, a mapped network share, a bind mount, a case-folded spelling.
///
/// Only attempted for a move, and a move must already be able to write to the source
/// because it deletes files there. A source that cannot be written is left to the path
/// and inode comparisons; the marker is removed immediately either way.
fn probe_same_dir(src: &Path, dst: &Path) -> bool {
    if !src.is_dir() || !dst.is_dir() {
        return false;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!(".kevat-probe-{}-{stamp}", std::process::id());
    let marker = src.join(&name);
    if File::create(&marker).is_err() {
        return false;
    }
    let seen = dst.join(&name).exists();
    let _ = fs::remove_file(&marker);
    seen
}

fn check_paths(src: &Path, dst: &Path, mode: Mode, dry_run: bool) -> io::Result<()> {
    let s = journal::stable_key(src);
    let d = journal::stable_key(dst);

    // "Path", not "folder": a file source arrives here as its resolved destination
    // file, and `kevat dir/f.txt dir` lands on exactly this branch.
    // The probe is the only one of the three that holds on Windows, and it is reserved
    // for a move: it writes, so a dry run must not reach it, and a copy onto itself
    // wastes effort but destroys nothing.
    let probed = mode == Mode::Move && !dry_run && probe_same_dir(src, dst);
    if s == d || same_file(src, dst) || probed {
        let what = match mode {
            Mode::Move => "source and destination are the same path — a move would delete the only copy",
            Mode::Copy => "source and destination are the same path",
        };
        return Err(io::Error::new(io::ErrorKind::InvalidInput, what));
    }

    let mut prefix = s.clone();
    prefix.push(std::path::MAIN_SEPARATOR);
    if d.starts_with(&prefix) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination is inside the source folder",
        ));
    }
    // The aliased spelling of destination-inside-source: `kevat /real /mnt/sub` where
    // `/mnt` is `/real` under another mount point. No ancestor *string* of the
    // destination matches the source, but an ancestor inode does. Walking the resolved
    // destination's ancestors costs one stat each — a handful at most.
    #[cfg(unix)]
    for anc in Path::new(&d).ancestors().skip(1) {
        if same_file(src, anc) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination is inside the source folder",
            ));
        }
    }
    Ok(())
}

/// Whether the filesystem holding `dir` treats names case-insensitively — decided by a
/// probe, never a platform guess: NTFS is usually insensitive but can be per-directory
/// sensitive, and an exFAT stick is insensitive even when mounted on Linux, which is
/// exactly the machine where this matters. The probe file lives for microseconds; a
/// kill in that window leaves one dotfile, the same accepted exposure as
/// `probe_same_dir`. An unprobeable destination answers `false` — unknown must never
/// refuse a legitimate transfer.
fn dest_case_insensitive(dir: &Path) -> bool {
    let name = format!(".kevat-case-{}", std::process::id());
    let lower = dir.join(&name);
    let upper = dir.join(name.to_uppercase());
    if fs::File::create(&lower).is_err() {
        return false;
    }
    let insensitive = upper.exists();
    let _ = fs::remove_file(&lower);
    insensitive
}

/// Refuse manifests whose names cannot land safely on this destination — before byte
/// zero, so the whole job is refused rather than half of it.
///
/// (a) Case collisions. Source filesystems are case-sensitive; FAT-family and NTFS
/// destinations are not. `README` and `readme` map to one destination file: the second
/// copy verifies against its own bytes and then renames over the first, and in move
/// mode both sources are subsequently unlinked — a file ends up existing in neither
/// location, with exit 0. Only fires when the destination actually folds case.
///
/// (b) Windows name rules, on Windows only: reserved device stems (`CON`, `NUL`,
/// `COM1`… — `con.kpart` is still the console, and reading it back for verify hangs),
/// `:` (silently redirects the bytes into an NTFS alternate data stream), and trailing
/// dots/spaces (Win32 strips them, so the rename lands on a different name than the
/// journal recorded).
fn check_manifest_names(dst: &Path, m: &Manifest) -> io::Result<()> {
    #[cfg(windows)]
    for e in &m.files {
        for comp in e.rel.components() {
            let name = comp.as_os_str().to_string_lossy();
            let stem = name.split('.').next().unwrap_or(&name);
            let reserved = matches!(
                stem.to_ascii_uppercase().as_str(),
                "CON" | "PRN" | "AUX" | "NUL"
                    | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
                    | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
            );
            if reserved || name.contains(':') || name.ends_with('.') || name.ends_with(' ') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "\"{}\" cannot be written on Windows ({}). Rename it and run again.",
                        e.rel.display(),
                        if reserved {
                            "a reserved device name"
                        } else if name.contains(':') {
                            "':' names an NTFS alternate data stream"
                        } else {
                            "Windows strips trailing dots and spaces"
                        }
                    ),
                ));
            }
        }
    }

    // The collision scan is pointless for a single file, and the probe writes to the
    // destination — skip both when there is nothing to collide.
    if m.files.len() + m.dirs.len() < 2 || !dest_case_insensitive(dst) {
        return Ok(());
    }
    let mut seen: std::collections::HashMap<String, &Path> = std::collections::HashMap::new();
    for rel in m.dirs.iter().chain(m.files.iter().map(|e| &e.rel)) {
        let folded = rel.to_string_lossy().to_lowercase();
        if let Some(first) = seen.insert(folded, rel) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "\"{}\" and \"{}\" are the same name on this drive's filesystem — \
                     copying both would leave only one. Rename one and run again.",
                    first.display(),
                    rel.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Whether the filesystem holding `dst` is FAT-family (FAT12/16/32 — not exFAT), whose
/// on-disk format stores file sizes in 32 bits: no single file of 4 GiB or more can
/// exist on it, whatever tool writes it. Best-effort; unknown answers `false` so a
/// legitimate transfer is never refused on a guess.
fn dest_is_fat_family(dst: &Path) -> bool {
    // The destination may not exist yet (an explicit file path, a folder about to be
    // created). Probe the nearest existing ancestor, as free_space does — otherwise the
    // check silently passes exactly when it is needed.
    let mut probe = dst;
    while !probe.exists() {
        match probe.parent() {
            Some(p) => probe = p,
            None => return false,
        }
    }
    let dst = probe;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(dst.as_os_str().as_bytes()) else {
            return false;
        };
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
            return false;
        }
        // MSDOS_SUPER_MAGIC covers vfat (FAT12/16/32); exFAT has its own magic.
        s.f_type as i64 == 0x4d44
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(dst.as_os_str().as_bytes()) else {
            return false;
        };
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
            return false;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(s.f_fstypename.as_ptr()) };
        name.to_string_lossy().eq_ignore_ascii_case("msdos")
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            GetVolumeInformationW, GetVolumePathNameW,
        };
        // GetVolumePathNameW, not "take the first two chars and hope for a drive
        // letter": a relative destination, a UNC share and a \\?\-verbatim path all
        // have no `D:` prefix, and the old parse silently answered "not FAT" for
        // them — bypassing the 4 GB pre-check exactly where the user typed the path
        // an unusual way. Canonicalize first so relative paths gain their root.
        let abs = dst.canonicalize().unwrap_or_else(|_| dst.to_path_buf());
        let wide: Vec<u16> = abs.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut root = [0u16; 260];
        if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0
        {
            return false;
        }
        let mut fsname = [0u16; 32];
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fsname.as_mut_ptr(),
                fsname.len() as u32,
            )
        };
        if ok == 0 {
            return false;
        }
        let end = fsname.iter().position(|&c| c == 0).unwrap_or(0);
        let name = String::from_utf16_lossy(&fsname[..end]);
        // "FAT", "FAT32" — but not "exFAT".
        name.to_ascii_uppercase().starts_with("FAT")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = dst;
        false
    }
}

/// Refuse, before byte zero, a manifest containing a file FAT32 cannot hold. The drive
/// would reject the write mid-copy anyway; knowing every size up front (enumerate
/// first) means the user hears it now, with the file named and the fix stated, instead
/// of an OS error an hour in.
fn check_fat_size_limit(dst: &Path, m: &Manifest) -> io::Result<()> {
    const FAT_MAX: u64 = u32::MAX as u64; // 4 GiB − 1, the format's hard ceiling
    if !dest_is_fat_family(dst) {
        return Ok(());
    }
    if let Some(e) = m.files.iter().find(|e| e.size > FAT_MAX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "\"{}\" is {:.1} GB, but this drive is formatted FAT32, which cannot \
                 hold any file of 4 GB or more. Reformat the drive as exFAT, or leave \
                 that file out.",
                e.rel.display(),
                e.size as f64 / 1_000_000_000.0
            ),
        ));
    }
    Ok(())
}

/// Bytes free on the filesystem that holds `path`, for the current user.
///
/// Used to refuse a transfer that cannot fit before a single byte is written, rather than
/// discovering it when the drive fills mid-copy. Walks up to the nearest existing ancestor
/// because the destination folder itself may not have been created yet. `None` when the
/// figure cannot be read — the caller then simply proceeds rather than blocking on a guess.
///
/// Only the GUI performs this pre-flight; the CLI trusts the OS to error if the disk fills.
/// Scoped to the gui build so a CLI-only compile does not warn it unused.
#[cfg(feature = "gui")]
pub fn free_space(path: &Path) -> Option<u64> {
    let mut probe = path;
    loop {
        if probe.exists() {
            break;
        }
        probe = probe.parent()?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(probe.as_os_str().as_bytes()).ok()?;
        // Safety: `stat` is written only if statvfs returns 0, and it is POD.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        // f_bavail is blocks available to a non-root user — the honest number, since a
        // filesystem can reserve blocks root alone may touch. f_frsize is their size.
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = probe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut avail: u64 = 0;
        // Bytes available *to this user* — quotas can make that smaller than the disk's
        // free total, and the caller's fit check should see the number that will bite.
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut avail,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        Some(avail)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = probe;
        None
    }
}

/// Make a directory entry itself durable.
///
/// `fsync` on a file commits its *contents*; the name it lives under is metadata in the
/// parent directory, and POSIX does not promise a rename is durable until that directory
/// is synced. Move mode is the one place this matters enough to pay for: without it,
/// power loss just after `rename` can revert the destination to its `.kpart` name while
/// the unlink of the source — on a different filesystem, with its own commit cadence —
/// survives, leaving the file under neither final name.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows: a plain `File::open` on a directory fails only because std omits
/// `FILE_FLAG_BACKUP_SEMANTICS`, not because the OS forbids it — pass the flag and the
/// handle flushes like any other. NTFS's own log makes the rename *atomic*, but not
/// *durable* at the moment `MoveFileExW` returns; the log flushes lazily, and move mode
/// unlinks the source on a different volume with its own commit cadence. Flushing the
/// directory forces the log through the rename record. Should ACLs refuse the
/// write-access directory open, the caller falls back to flushing the renamed file
/// itself, which also drags the log past the rename's LSN.
#[cfg(windows)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?
        .sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Force one durability barrier over the whole destination before any source is
/// deleted. This is the proof the per-file design could not afford: it flushes the
/// directories the transfer actually touched (deduplicated), so a bridge or disk that
/// has been holding data in a volatile cache must commit it — or fail here, with every
/// original still in place.
fn sync_destination(dst: &Path, m: &Manifest) -> io::Result<()> {
    let mut dirs: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    // `dst` is normally the destination root directory — but a single file moved to an
    // explicitly spelled file path (`kevat big.bin /mnt/usb/big.bin --move`) makes it
    // the destination FILE. Syncing that file flushes bytes copy_file already made
    // durable and proves nothing about the `.kpart` → final-name rename, which lives
    // as an entry in the PARENT directory — the very thing this barrier exists to pin
    // down before any source is unlinked. Found by adversarial review.
    if dst.is_dir() {
        dirs.insert(dst.to_path_buf());
    } else if let Some(parent) = dst.parent().filter(|p| !p.as_os_str().is_empty()) {
        dirs.insert(parent.to_path_buf());
    } else {
        dirs.insert(dst.to_path_buf());
    }
    for e in &m.files {
        if let Some(parent) = e.rel.parent() {
            if !parent.as_os_str().is_empty() {
                dirs.insert(dst.join(parent));
            }
        }
    }
    for d in dirs {
        // A directory that cannot be flushed is not fatal on its own — but it must not
        // be reported as durable either, so the error propagates and the originals stay.
        sync_dir(&d)?;
    }
    Ok(())
}

/// Validate a checkpoint against what is actually on the medium. A USB yank corrupts the
/// tail, so an offset on its own proves nothing — a checkpoint is never trusted blind.
fn checkpoint_is_intact(part: &Path, off: u64, span: u64, want_hex: &str) -> bool {
    if span == 0 || off < span {
        return false;
    }
    let Ok(mut f) = File::open(part) else {
        return false;
    };
    let Ok(md) = f.metadata() else { return false };
    if md.len() < off {
        return false; // destination is shorter than claimed
    }
    if f.seek(SeekFrom::Start(off - span)).is_err() {
        return false;
    }
    // Streamed: a span is now the full 64 MiB advance, and allocating that to validate
    // it would undo the point of a bounded-memory engine.
    let mut h = Xxh3::new();
    let mut left = span;
    let mut buf = vec![0u8; CHUNK.min(span as usize).max(1)];
    while left > 0 {
        let want = buf.len().min(left as usize);
        if f.read_exact(&mut buf[..want]).is_err() {
            return false;
        }
        h.update(&buf[..want]);
        left -= want as u64;
    }
    format!("{:032x}", h.digest128()) == want_hex
}

fn hash_file(path: &Path) -> io::Result<u128> {
    let mut f = File::open(path)?;
    let mut h = Xxh3::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.digest128())
}

#[cfg(unix)]
fn set_mtime(path: &Path, mtime: i64) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let times = [
        libc::timespec { tv_sec: mtime, tv_nsec: 0 },
        libc::timespec { tv_sec: mtime, tv_nsec: 0 },
    ];
    let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn set_mtime(path: &Path, mtime: i64) -> io::Result<()> {
    // This is not cosmetic: already_present() gates the resume skip on the
    // destination's mtime equalling the manifest's. Without the stamp every resume
    // silently re-copies the whole tree at full cost while reporting success.
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::SetFileTime;
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    // FILETIME counts 100 ns ticks since 1601-01-01; unix seconds shift by the
    // 11'644'473'600 s between the epochs.
    let ticks = (mtime + 11_644_473_600) as u64 * 10_000_000;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let ok = unsafe {
        SetFileTime(f.as_raw_handle() as _, std::ptr::null(), std::ptr::null(), &ft)
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(unix, windows)))]
fn set_mtime(_path: &Path, _mtime: i64) -> io::Result<()> {
    Ok(())
}

/// A previously completed file is re-checked cheaply by size + mtime; `--paranoid`
/// re-hashes it (per-file reconciliation, never a blanket restart).
///
/// Paranoid hashes both ends against the recorded hash, not just the destination.
/// `rec.hash` is the hash of the source *as it was copied*, so an edit that preserved
/// the source's size and mtime slips past the cheap check and still matches the
/// destination perfectly — the destination holds the old bytes. Hashing the source is
/// what catches exactly the case `--paranoid` exists for.
/// Whether the destination already holds this manifest entry, judged without any
/// journal: same size, same modification time (±2 s, since FAT stores times coarsely).
///
/// This is the ordinary-incremental test — used when no journal record exists because a
/// previous run *completed* and removed its journal. It is deliberately weaker than
/// `already_present`, which additionally checks the source's nanosecond identity against
/// what was recorded at copy time; here there is nothing recorded to check against. Good
/// enough to skip a re-copy, never good enough to delete a source.
fn matches_destination(dst: &Path, e: &scan::Entry) -> bool {
    let Ok(md) = fs::metadata(dst) else {
        return false;
    };
    if !md.is_file() || md.len() != e.size {
        return false;
    }
    (scan::mtime_of(&md) - e.mtime).abs() <= 2
}

/// `matches_destination`, plus the actual bytes under `--paranoid`.
///
/// The quick test is size + timestamp, which a same-size file modified within the same
/// two seconds can pass while holding different content. `--paranoid` promises to
/// re-hash rather than trust that — but it used to reach only journal-backed skips, so
/// after a *completed* run (journal removed) the incremental skips were never verified
/// no matter what the user asked for.
fn matches_destination_deep(src: &Path, dst: &Path, e: &scan::Entry, paranoid: bool) -> bool {
    if !matches_destination(dst, e) {
        return false;
    }
    if !paranoid {
        return true;
    }
    match (hash_file(src), hash_file(dst)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Destination files that already exist and differ from what is about to be written.
///
/// Free to compute because the manifest exists before byte zero (invariant 5), and it
/// is the fact both front ends need in order to stop being silent about overwriting.
pub fn conflicts(dst: &Path, m: &Manifest) -> Vec<PathBuf> {
    m.files
        .iter()
        .filter(|e| {
            let d = dst.join(&e.rel);
            d.exists() && !matches_destination(&d, e)
        })
        .map(|e| e.rel.clone())
        .collect()
}

/// Bytes this transfer still has to write: the manifest total minus everything the
/// destination already holds.
///
/// The plain total is the wrong number for the fit check on any resume or repeat run —
/// the files already on the drive are exactly what consumed its free space, so comparing
/// the full total against what is left refuses the transfer that is nearly finished. On
/// a drive sized to the data, which is the headline case, that dead-ends the whole
/// promise.
#[cfg(feature = "gui")]
pub fn bytes_still_needed(dst: &Path, m: &Manifest) -> u64 {
    m.files
        .iter()
        .filter(|e| !matches_destination(&dst.join(&e.rel), e))
        .map(|e| e.size)
        .sum()
}

fn already_present(
    src: &Path,
    dst: &Path,
    e: &scan::Entry,
    rec: &journal::DoneFile,
    paranoid: bool,
) -> bool {
    let Ok(md) = fs::metadata(dst) else {
        return false;
    };
    if md.len() != rec.size || rec.size != e.size || rec.mtime != e.mtime {
        return false;
    }
    // The *source* comparison is at nanosecond precision, exactly like the checkpoint
    // identity gate: whole seconds let an edit landing in the same second pass as
    // unchanged, and in move mode this branch's caller unlinks the source — the edited
    // file was deleted while the destination kept the stale bytes. A zero marks a
    // journal from a build that did not record nanoseconds: unproven, so re-copy
    // (bounded redo, invariant 6). The destination check below stays on whole seconds
    // deliberately — FAT-family drives cannot store finer, and demanding nanosecond
    // equality of the destination would break every resume onto them.
    if rec.mtime_ns == 0 || rec.mtime_ns != e.mtime_ns {
        return false;
    }
    // ±2 s, not equality: FAT32 stores modification times at 2-second resolution, so
    // the stamped mtime reads back rounded. Exact comparison failed for every odd-second
    // source on FAT32 — every resume onto the commonest cheap-stick format silently
    // re-copied the whole tree (safe, but the resume promise quietly evaporated).
    // Whole-hour offsets (to ±14 h) are equally FAT's fault: it stores LOCAL wall time,
    // so a DST flip or timezone change between runs shifts every stamp by exact hours
    // and broke every resume skip the same way. Size and the source's nanosecond
    // identity still gate above; the hour window only forgives the clock's frame.
    let dd = (scan::mtime_of(&md) - rec.mtime).abs();
    if dd > 2 {
        let nearest_hour = ((dd + 1800) / 3600) * 3600;
        let off_hour = (dd - nearest_hour).abs();
        if !(nearest_hour >= 3600 && nearest_hour <= 14 * 3600 && off_hour <= 2) {
            return false;
        }
    }
    if paranoid {
        let matches_rec = |p: &Path| match hash_file(p) {
            Ok(h) => format!("{:032x}", h) == rec.hash,
            Err(_) => false,
        };
        matches_rec(src) && matches_rec(dst)
    } else {
        true
    }
}

/// What the engine reports as it works. The GUI needs a live readout; the CLI ignores
/// all of it and keeps printing its own lines.
///
/// Deliberately narrow: it carries what a caller cannot already work out for itself
/// from the manifest it passed in. Totals and file sizes are not repeated here.
///
/// The CLI passes a no-op closure, so in a CLI-only build nothing reads these fields.
/// Scoped to that build rather than blanket-allowed, so a field that goes genuinely
/// unused in a GUI build is still reported.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub enum Progress {
    /// Emitted once, before the first byte. Deliberately carries no carried-forward
    /// count: the journal's claim is only an estimate until each file passes its
    /// re-validation, and a counter seeded from it double-counts every file that fails
    /// the check and is re-copied. `FileSkipped` fires per *verified* skip instead.
    Start,
    /// The destination folder skeleton is going in — real drive work that moves no file
    /// bytes. A 363k-file tree can hold tens of thousands of folders, minutes of work on
    /// a USB HDD during which the byte counter reads 0; without this event that phase is
    /// indistinguishable from a hang.
    Dirs { done: usize, total: usize },
    /// A file is now in flight, by path relative to the source root.
    File { rel: String },
    /// Re-verifying an already-written prefix before resuming into it. `bytes` is the
    /// running total validated for this file. Emitted so a long validation reads as
    /// work in progress rather than a frozen app.
    Checking { rel: String, bytes: u64 },
    /// More bytes are durable. Deltas, not totals — the caller accumulates.
    Bytes(u64),
    /// Bytes carried forward without being written this run — a whole file already present
    /// on resume, or the proven prefix of a part-copied file. They count toward the
    /// progress bar (this much of the job is done) but NOT toward the speed, which must
    /// reflect only bytes actually moving now. Without this a resumed transfer shows a bar
    /// near zero and an ETA of hours for a job that is almost finished.
    Skipped(u64),
    /// A whole file was confirmed already present (size + mtime against the journal —
    /// or a re-hash under --paranoid) and will not be copied this run. Fires only after
    /// the check passes, so `FileDone + FileSkipped` counts are exact, monotonic, and
    /// can never exceed the manifest total.
    FileSkipped,
    /// A file finished, was verified if asked for, and is renamed into place.
    FileDone,
    /// Move mode: everything is copied and proven; the originals are now being removed.
    DeletePhase { total: usize },
    /// One original removed.
    Deleted,
    /// Running total of files that could not be copied so far (locked, unreadable,
    /// failed verify). Carried as a total, not a delta, so the UI cannot drift. The
    /// copy continues past every one of them; this exists so 24,000 failures surface
    /// as a live counter during the run instead of an ambush on the final screen.
    Errors { total: usize },
    /// Periodic time-remaining estimate, in whole seconds. Computed from the manifest's
    /// two populations — small files cost a per-file price (seeks, metadata, AV), large
    /// files cost per byte — because a single bytes/rate extrapolation is wrong by hours
    /// in both directions on mixed trees. `seconds` is the total; the parts are carried
    /// too so the UI can itemise ("214,000 small files ≈ 2 h, then 71 GB ≈ 30 m").
    /// `None` means that part has not been sampled yet; consumers should say "working
    /// out how long…" rather than invent a number.
    Eta {
        seconds: Option<u64>,
        small_left: u64,
        small_secs: Option<u64>,
        big_bytes: u64,
        big_secs: Option<u64>,
    },
}

/// Files below this size are timed per file, not per byte: their cost is the seek and
/// the metadata, and the payload is noise.
const ETA_SMALL: u64 = 1 << 20;

/// The two-population time model behind `Progress::Eta`. Fed from the copy loop; emits
/// at most once a second.
struct EtaModel {
    /// Small files not yet reached.
    small_left: u64,
    /// Bytes of large files not yet reached.
    big_left: u64,
    /// Size of the large file in flight (0 while a small file is), and how much of it
    /// is already accounted for — copied this run or carried forward from a resume.
    cur_big: u64,
    cur_big_done: u64,
    cur_is_small: bool,
    /// Sampling window: completions and large-file bytes since `win_start`.
    win_start: Instant,
    win_small: u64,
    win_big: u64,
    /// Smoothed rates: small files per second, large-file bytes per second.
    rate_small: f64,
    rate_big: f64,
    last_emit: Instant,
}

impl EtaModel {
    fn new(m: &scan::Manifest) -> Self {
        let now = Instant::now();
        EtaModel {
            small_left: m.files.iter().filter(|e| e.size < ETA_SMALL).count() as u64,
            big_left: m.files.iter().filter(|e| e.size >= ETA_SMALL).map(|e| e.size).sum(),
            cur_big: 0,
            cur_big_done: 0,
            cur_is_small: true,
            win_start: now,
            win_small: 0,
            win_big: 0,
            rate_small: 0.0,
            rate_big: 0.0,
            last_emit: now,
        }
    }

    /// A file is being taken up. It leaves "ahead" now whether it ends copied, skipped
    /// or errored — only `completed` adds a rate sample, so the failure paths need no
    /// calls of their own.
    fn start_file(&mut self, size: u64) {
        self.cur_big = 0;
        self.cur_big_done = 0;
        self.cur_is_small = size < ETA_SMALL;
        if self.cur_is_small {
            self.small_left = self.small_left.saturating_sub(1);
        } else {
            self.big_left = self.big_left.saturating_sub(size);
            self.cur_big = size;
        }
    }

    /// The current file finished for real (copied, verified if asked, renamed): count
    /// it toward the small-file rate. Large files sample through `wrote` instead.
    fn completed(&mut self) {
        if self.cur_is_small {
            self.win_small += 1;
        }
        self.cur_big = 0;
        self.cur_big_done = 0;
    }

    /// Freshly written bytes on the current file. Takes the sink too because a large
    /// file can be the whole job for hours — this is the only place its ETA can tick.
    fn wrote(&mut self, n: u64, on: &mut dyn FnMut(Progress)) {
        if self.cur_big > 0 {
            self.cur_big_done += n;
            self.win_big += n;
        }
        self.maybe_emit(on);
    }

    /// Bytes carried forward on the current file (a proven resume prefix): they shrink
    /// the remaining work but sample no rate — nothing was written now.
    fn carried(&mut self, n: u64) {
        if self.cur_big > 0 {
            self.cur_big_done += n;
        }
    }

    fn maybe_emit(&mut self, on: &mut dyn FnMut(Progress)) {
        if self.last_emit.elapsed().as_secs_f64() < 1.0 {
            return;
        }
        self.last_emit = Instant::now();
        let dt = self.win_start.elapsed().as_secs_f64();
        if dt >= 1.0 {
            // Fold the window into the smoothed rates. A rate only updates while its
            // population is actually being worked (its counter moved, or its kind of
            // file is the one in flight): an hour inside one huge file is not evidence
            // that small files got slower, but a stall *on* a small file is.
            let blend = |old: f64, sample: f64| if old > 0.0 { 0.7 * old + 0.3 * sample } else { sample };
            if self.win_small > 0 || self.cur_is_small {
                self.rate_small = blend(self.rate_small, self.win_small as f64 / dt);
            }
            if self.win_big > 0 || self.cur_big > 0 {
                self.rate_big = blend(self.rate_big, self.win_big as f64 / dt);
            }
            self.win_start = Instant::now();
            self.win_small = 0;
            self.win_big = 0;
        }
        let big_remaining =
            self.big_left + self.cur_big.saturating_sub(self.cur_big_done.min(self.cur_big));
        let small_secs = if self.small_left == 0 {
            Some(0)
        } else if self.rate_small > 0.0 {
            Some((self.small_left as f64 / self.rate_small) as u64)
        } else {
            None
        };
        let big_secs = if big_remaining == 0 {
            Some(0)
        } else if self.rate_big > 0.0 {
            Some((big_remaining as f64 / self.rate_big) as u64)
        } else {
            None
        };
        on(Progress::Eta {
            seconds: match (small_secs, big_secs) {
                (Some(a), Some(b)) => Some(a + b),
                _ => None,
            },
            small_left: self.small_left,
            small_secs,
            big_bytes: big_remaining,
            big_secs,
        });
    }
}

/// The CLI entry point: no progress reporting, no cancellation.


/// The engine proper.
///
/// `cancel` is polled between files. Stopping is safe at any point precisely because
/// resume is: the journal records only what is durable, so a cancelled transfer is
/// indistinguishable from an unplugged drive and the same command continues it.
pub fn run_with(
    src: &Path,
    dst: &Path,
    m: &Manifest,
    opts: &Options,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Progress),
) -> io::Result<Summary> {
    let started = Instant::now();
    let mut sum = Summary::default();

    // A file source: the manifest's single entry names the file itself, so the
    // destination must resolve to a *file* path — into an existing directory under
    // the source's name (the `cp` convention), or at the destination path exactly as
    // typed. Resolved once, up front, and the safety check runs against the resolved
    // file rather than the raw argument: `kevat dir/f.txt dir` resolves back onto the
    // source itself, and a move would otherwise delete the only copy.
    let single_dst: Option<PathBuf> = if m.root_is_file {
        let resolved = if dst.is_dir() || spelled_as_dir(dst) {
            match src.file_name() {
                Some(name) => dst.join(name),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "source has no file name",
                    ))
                }
            }
        } else {
            dst.to_path_buf()
        };
        check_paths(src, &resolved, opts.mode, opts.dry_run)?;
        Some(resolved)
    } else {
        check_paths(src, dst, opts.mode, opts.dry_run)?;
        None
    };

    // Key on the *resolved* destination, not the argument as typed. For a file source,
    // `kevat big.bin out` and `kevat big.bin out/big.bin` name the same physical file;
    // keying on the raw argument gave them separate journals and separate locks, so both
    // ran at once on one .kpart and each truncated under the other's open descriptor.
    let key_dst: &Path = single_dst.as_deref().unwrap_or(dst);

    let jpath = journal::journal_path(src, key_dst, opts.mode, opts.job_tag.as_deref());
    let prev: ResumeState = journal::load(&jpath)?;
    let resuming = !prev.done.is_empty() || prev.partial.is_some();

    if resuming {
        println!(
            "resuming: {} file(s) already done{}",
            prev.done.len(),
            if prev.torn_tail { ", torn journal tail discarded" } else { "" }
        );
    }

    if opts.dry_run {
        println!(
            "dry run: would {} {} file(s), {} byte(s) → {}",
            match opts.mode { Mode::Copy => "copy", Mode::Move => "move" },
            m.file_count(),
            m.total_bytes,
            dst.display()
        );
        for (p, why) in &m.skipped {
            println!("  skip {}: {}", p.display(), why);
        }
        sum.elapsed_secs = started.elapsed().as_secs_f64();
        return Ok(sum);
    }

    // One transfer per destination, arbitrated on the destination alone. The journal's
    // own flock is keyed on (src, dst, mode), so it cannot see a second run that
    // differs in source or mode yet writes to the same place. Taken after the dry-run
    // return — a dry run writes nothing and may inspect while a transfer is live —
    // and held (by being bound here) until the run ends.
    let _dest_lock = journal::lock_destination(key_dst)?;

    // A missing destination root is fatal — nothing can proceed without it. One
    // uncreatable subdirectory (say, the destination already holds a *file* by that
    // name) must cost only the files under it, not the whole transfer; the files
    // themselves then fail one by one, each under its own name. A file source creates
    // no directory at all: the destination path names the file to write.
    if single_dst.is_none() {
        fs::create_dir_all(dst).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot create destination {}: {e}", dst.display()),
            )
        })?;
    }

    // Refuse doomed or destructive names before byte zero — enumerate-first makes this
    // free. Two families: (a) manifest entries that collide on a case-insensitive
    // destination (an exFAT/NTFS drive — even mounted on Linux), where `README` and
    // `readme` map to ONE destination file: the second copy replaces the first, and in
    // move mode both sources are then unlinked, leaving a file in neither location;
    // (b) on Windows, reserved device names (`con.txt` opens the console — reading it
    // back hangs the transfer) and names Win32 silently rewrites (trailing dots/spaces,
    // `:` starts an alternate data stream that swallows the file's bytes).
    check_manifest_names(dst, m)?;
    check_fat_size_limit(dst, m)?;
    if opts.on_exists == OnExists::Fail {
        let clashes = conflicts(dst, m);
        if !clashes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} file(s) already exist at the destination and differ — first: {}",
                    clashes.len(),
                    clashes[0].display()
                ),
            ));
        }
    }

    // Tens of thousands of folders on a USB HDD is minutes of real work that moves no
    // file bytes: report it, and honour Stop during it — the file loop below sees the
    // same flag and winds down before the first byte.
    let dir_total = m.dirs.len();
    for (i, dir) in m.dirs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Err(err) = fs::create_dir_all(dst.join(dir)) {
            sum.errors
                .push((dir.clone(), format!("cannot create directory: {err}")));
            on(Progress::Errors { total: sum.errors.len() });
        }
        if (i + 1) % 128 == 0 || i + 1 == dir_total {
            on(Progress::Dirs { done: i + 1, total: dir_total });
        }
    }

    let mut jr = Journal::create(&jpath)?;
    if !resuming {
        // Absolute paths, not the arguments as typed: `kevat src dst` run from a shell
        // would otherwise record "src" and "dst", and the resume offer in the GUI would
        // display junk and test existence against *its* working directory. Lexical (no
        // symlink resolution) so the string stays human-readable — `stable_key` starts
        // from the same normalisation, so feeding these back resumes under the same key.
        jr.append_and_commit(&Record::Session {
            v: journal::JOURNAL_VERSION,
            names: opts.selection.clone(),
            skip_caches: opts.skip_caches,
            skip_cloud: opts.skip_cloud,
            src: journal::lexical_abs(src).to_string_lossy().into_owned(),
            // key_dst, not dst: for a file source the destination resolves to the file
            // path, and recording the folder instead made a journal-sourced resume key
            // on a different string — a silent full re-copy.
            dst: journal::lexical_abs(key_dst).to_string_lossy().into_owned(),
            mode: opts.mode,
            files: m.file_count(),
            bytes: m.total_bytes,
        })?;
    }

    let already_deleted: std::collections::HashSet<String> =
        prev.deleted.iter().cloned().collect();
    // Files whose destination copy is proven this run — the only ones phase 2 may
    // delete a source for.
    let mut proven: std::collections::HashSet<String> = std::collections::HashSet::new();

    on(Progress::Start);
    let mut eta = EtaModel::new(m);

    for e in &m.files {
        // Between files, never mid-file: the destination is left at a proven
        // checkpoint either way, but stopping on a boundary keeps the common case
        // free of a partial .kpart to reason about.
        if cancel.load(Ordering::Relaxed) {
            sum.stopped = true;
            break;
        }

        let rel = e.rel.to_string_lossy().into_owned();
        let s = if m.root_is_file {
            src.to_path_buf()
        } else {
            src.join(&e.rel)
        };
        let d = match &single_dst {
            Some(p) => p.clone(),
            None => dst.join(&e.rel),
        };
        let part = part_path(&d);
        eta.start_file(e.size);

        if let Some(rec) = prev.done.get(&rel) {
            if already_present(&s, &d, e, rec, opts.paranoid) {
                sum.files_skipped += 1;
                // Already on the drive from a previous run — count it toward the bar so a
                // resume shows how much is really done, not a bar stuck near zero.
                on(Progress::Skipped(e.size));
                on(Progress::FileSkipped);
                // A move interrupted between the destination being recorded and the
                // source being unlinked leaves the file in both places; that debt is
                // settled uniformly in phase 2 below, which is the single place any
                // source is ever removed.
                if opts.mode == Mode::Move {
                    proven.insert(rel.clone());
                }
                continue;
            }
            // The record proves Kevat once wrote this destination — but its mismatch
            // with the journal now may be the user's (or another tool's) NEWER edit of
            // the destination file, which is exactly what Keep protects. Falling
            // through to the copy path here silently overwrote it; the `if let` above
            // made the Keep arm below structurally unreachable for every journal-backed
            // file. Found by adversarial review.
            if opts.on_exists == OnExists::Keep && d.exists() && !matches_destination(&d, e) {
                sum.files_skipped += 1;
                sum.kept_existing += 1;
                on(Progress::Skipped(e.size));
                on(Progress::FileSkipped);
                continue;
            }
        } else if opts.on_exists != OnExists::Replace
            && prev.done.get(&rel).is_none()
            && d.exists()
            && !matches_destination(&d, e)
        {
            // The destination holds a different file and the user asked to keep it.
            // (OnExists::Fail is refused up front, before byte zero — see below.)
            sum.files_skipped += 1;
            sum.kept_existing += 1;
            on(Progress::Skipped(e.size));
            on(Progress::FileSkipped);
            continue;
        } else if opts.mode == Mode::Copy && matches_destination_deep(&s, &d, e, opts.paranoid) {
            // No journal record — a *previous run finished*, so its journal was removed —
            // but the destination already holds this exact file. Re-copying it would be
            // pure waste, and it is the ordinary case: back up a folder, add a few files
            // next month, run it again. Without this, the second session re-copied the
            // whole tree (and the GUI could refuse outright, since the destination was
            // already full of the first copy).
            //
            // Copy mode only, deliberately. In move mode a skip *unlinks the source*, and
            // size+mtime alone is not proof enough to delete data with no record that
            // Kevat ever wrote that destination — there, re-copy and let verify prove it.
            sum.files_skipped += 1;
            on(Progress::Skipped(e.size));
            on(Progress::FileSkipped);
            eta.maybe_emit(on);
            continue;
        }

        on(Progress::File { rel: rel.clone() });

        // Resume mid-file only if the checkpoint chain still matches the medium.
        let mut resume_from = 0u64;
        if let Some(p) = prev.partial.as_ref().filter(|p| p.rel == rel) {
            // Everything below the offset is reused unread, so it is only sound if the
            // source is byte-for-byte the file those bytes came from. A source edited
            // while the drive was unplugged would otherwise be spliced onto the old
            // prefix, yielding a destination matching no version that ever existed —
            // and a recorded hash for a file that never existed. A zero size marks a
            // checkpoint from a build that did not record this: treat it as unproven.
            // The mtime compares at nanosecond precision — an older journal's
            // seconds-valued checkpoint simply fails the match and the file restarts,
            // which is the bounded-redo direction (invariant 6).
            let same_source = p
                .chain
                .iter()
                .all(|c| c.src_size != 0 && c.src_size == e.size && c.src_mtime == e.mtime_ns);
            // Every span in the chain is re-hashed off the medium, and the chain must
            // reach back to byte zero. Validating only the newest span — which is what
            // used to happen — left [0, off - span) trusted blind, a region that grew
            // 64 MiB per checkpoint; a bit flipped anywhere in it rode through to the
            // final output with exit 0.
            let covers_from_zero = p.chain.first().map_or(false, |c| c.off == c.span);
            // Validating the chain re-hashes the whole proven prefix off the medium —
            // for a 1 TB file resumed at 95%, hours of reading before one new byte is
            // written. Reporting each span as it is verified is what keeps that from
            // looking like a hang and getting killed, which would restart the same wait.
            let mut checked: u64 = 0;
            let intact = same_source
                && covers_from_zero
                && p.chain.iter().all(|c| {
                    if cancel.load(Ordering::Relaxed) {
                        return false;
                    }
                    let ok = checkpoint_is_intact(&part, c.off, c.span, &c.span_hash);
                    if ok {
                        checked += c.span;
                        on(Progress::Checking { rel: rel.clone(), bytes: checked });
                    }
                    ok
                });
            if intact {
                resume_from = p.chain.last().map_or(0, |c| c.off);
                // The proven prefix is part of the job already done — count it on the bar.
                on(Progress::Skipped(resume_from));
                eta.carried(resume_from);
                println!("  resuming {rel} at {resume_from} byte(s)");
            } else if !same_source {
                println!(
                    "  {}",
                    if p.chain.iter().any(|c| c.src_size == 0) {
                        // Written by a build that did not record the source's identity,
                        // so it proves nothing either way — restart rather than guess.
                        format!("{rel}: earlier progress cannot be verified — copying it again")
                    } else {
                        format!("{rel} changed since it was interrupted — copying it again")
                    }
                );
                let _ = fs::remove_file(&part);
            }
        }

        // Stop pressed during the chain validation above: the cancelled check reads as
        // "not intact", which without this gate falls into copy_file at offset zero —
        // truncating a .kpart holding gigabytes of proven progress because the user
        // asked to STOP. Leave everything as it stands; the next run re-validates the
        // same chain. Found by adversarial review.
        if cancel.load(Ordering::Relaxed) {
            sum.stopped = true;
            break;
        }

        // Per-file, not fatal: a parent that cannot exist (a file sits where the
        // directory should) costs this file alone, and the journal keeps its
        // records for the files that did land.
        if let Some(parent) = d.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                sum.errors.push((
                    e.rel.clone(),
                    format!("cannot create directory {}: {err}", parent.display()),
                ));
                on(Progress::Errors { total: sum.errors.len() });
                continue;
            }
        }

        let (hash, written, cancelled) = match copy_file(
            &s,
            &part,
            resume_from,
            &rel,
            &mut jr,
            cancel,
            &mut |n| {
                on(Progress::Bytes(n));
                eta.wrote(n, &mut *on);
            },
        ) {
            Ok(v) => v,
            Err(err) => {
                // A source that failed before contributing a byte leaves a 0-byte
                // .kpart. One is invisible; a cloud-placeholder tree that refuses
                // hydration leaves 24,000 of them, and the backup looks like junk.
                // An empty part holds nothing, so removing it loses nothing; a part
                // with bytes is checkpointed progress and stays for the resume.
                if fs::metadata(&part).map_or(false, |m| m.len() == 0) {
                    let _ = fs::remove_file(&part);
                }
                // Name both ends: a bare os-error string ("No such file or directory")
                // gives the user nothing to act on when the run touches thousands of
                // paths. The *final* name, not the .kpart it was being staged as —
                // users read the temp suffix as "my file was saved wrong".
                sum.errors.push((
                    e.rel.clone(),
                    format!("cannot copy {} to {}: {err}", s.display(), d.display()),
                ));
                on(Progress::Errors { total: sum.errors.len() });
                continue;
            }
        };
        sum.bytes_written += written;
        // Stopped inside this file: its .kpart and checkpoint are durable, so the next
        // run continues from here. Nothing is renamed, verified or recorded as done.
        if cancelled {
            sum.stopped = true;
            break;
        }

        // Verify before the rename, so a file only becomes "final" once proven.
        let hash_hex = format!("{:032x}", hash);
        let mut verified = false;
        if opts.verify {
            match hash_file(&part) {
                Ok(back) if format!("{:032x}", back) == hash_hex => {
                    verified = true;
                    sum.files_verified += 1;
                }
                Ok(_) => {
                    // Verification has just proven these bytes wrong, and some of them may
                    // sit below the last checkpoint, where checkpoint_is_intact never
                    // looks. Leaving the .kpart in place made the next run resume onto the
                    // same bad prefix and fail identically, forever. Discard it so the file
                    // restarts from zero.
                    let _ = fs::remove_file(&part);
                    sum.errors
                        .push((e.rel.clone(), "verify failed: destination differs, restarting this file next run".into()));
                    on(Progress::Errors { total: sum.errors.len() });
                    continue;
                }
                Err(err) => {
                    sum.errors.push((e.rel.clone(), format!("verify unreadable: {err}")));
                    on(Progress::Errors { total: sum.errors.len() });
                    continue;
                }
            }
        }

        // Same shape: a rename refused (a directory sits where the file should go)
        // is this file's problem. Aborting here with `?` used to drop the journal's
        // un-fsynced buffer too, so files that *had* copied lost their file-done
        // records and were re-copied on the next run.
        if let Err(err) = fs::rename(&part, &d) {
            sum.errors.push((
                e.rel.clone(),
                format!("cannot move into place at {}: {err}", d.display()),
            ));
            on(Progress::Errors { total: sum.errors.len() });
            // Only the structural case (a directory sits where the file should go)
            // refuses identically on every resume, so only that one may discard the
            // .kpart. A transient refusal — a Windows sharing violation from an
            // indexer, say — must keep those verified bytes, or the next run re-copies
            // the whole file for nothing.
            if fs::metadata(&d).map_or(false, |m| m.is_dir()) {
                let _ = fs::remove_file(&part);
            }
            continue;
        }
        let _ = set_mtime(&d, e.mtime);

        // Data durable, then the record. J-after-D.
        jr.append(&Record::FileDone {
            rel: rel.clone(),
            size: e.size,
            mtime: e.mtime,
            mtime_ns: e.mtime_ns,
            hash: hash_hex,
        })?;
        if jr.should_commit() {
            jr.commit()?;
        }
        sum.files_copied += 1;
        on(Progress::FileDone);
        eta.completed();
        eta.maybe_emit(on);

        // Move mode deletes nothing here. Sources go in phase 2, after EVERY file is
        // proven and one durability barrier has been forced over the whole destination
        // — see the phase-2 block below for why per-file deletion was unsafe.
        if opts.mode == Mode::Move && opts.verify && verified {
            proven.insert(rel.clone());
        } else if opts.mode == Mode::Move && !opts.verify {
            proven.insert(rel.clone());
        }
    }

    jr.commit()?;

    // ── phase 2: remove the originals ────────────────────────────────────────
    //
    // Move deletes nothing until everything is copied, verified and durable. Deleting
    // per file looked safe — each unlink followed that file's own proof — but the proof
    // was weaker than it appeared: the verify pass re-reads bytes written seconds
    // earlier, which the OS serves from its page cache, so it attests that the kernel
    // took the data, not that the drive holds it. A dying disk or a USB bridge that
    // acknowledges a flush it has not performed passes every per-file check while the
    // originals disappear one by one; by the time anything looks wrong, thousands of
    // sources exist only on failing hardware.
    //
    // Doing it in one phase makes a real proof affordable: a single barrier over the
    // whole destination, forced once, before the first source is touched. It also means
    // a wrong drive, a dying drive, a full drive or a change of mind costs nothing —
    // Stop before this point and the source tree is still whole.
    if opts.mode == Mode::Move && !sum.stopped && sum.errors.is_empty() && !proven.is_empty() {
        // The barrier. Per-file this was `sync_dir` on each parent; once, here, it is
        // both stronger and cheaper.
        if let Err(err) = sync_destination(dst, m) {
            sum.errors.push((
                PathBuf::from("."),
                format!("cannot make the destination durable: {err} — originals kept"),
            ));
        } else {
            jr.append_and_commit(&Record::AllCopied)?;
            on(Progress::DeletePhase { total: proven.len() });
            for e in &m.files {
                if cancel.load(Ordering::Relaxed) {
                    sum.stopped = true;
                    break;
                }
                let rel = e.rel.to_string_lossy().into_owned();
                if !proven.contains(&rel) || already_deleted.contains(&rel) {
                    continue;
                }
                let s_path = if m.root_is_file { src.to_path_buf() } else { src.join(&e.rel) };
                let Ok(md) = fs::metadata(&s_path) else {
                    continue; // already gone: the debt was settled by an earlier run
                };
                // The one hazard this design introduces: the gap between copying a file
                // and deleting it is now the length of the whole job, and the user may
                // have edited it in between. Deleting then would destroy the edit while
                // the destination kept the stale bytes. Re-check identity at nanosecond
                // precision — the same gate `already_present` uses — and keep anything
                // that changed. Withholding `Complete` leaves the journal, so the next
                // run re-copies the new version and settles it properly.
                if md.len() != e.size || scan::mtime_ns_of(&md) != e.mtime_ns {
                    sum.errors.push((
                        e.rel.clone(),
                        "changed since it was copied — original kept".to_string(),
                    ));
                    continue;
                }
                match fs::remove_file(&s_path) {
                    Ok(()) => {
                        // Group-committed: losing this record redoes as an unlink of an
                        // already-absent file, which the next run skips.
                        jr.append(&Record::SourceDeleted { rel: rel.clone() })?;
                        if jr.should_commit() {
                            jr.commit()?;
                        }
                        sum.sources_deleted += 1;
                        on(Progress::Deleted);
                    }
                    Err(err) => sum
                        .errors
                        .push((e.rel.clone(), format!("source not removed: {err}"))),
                }
            }
            jr.commit()?;
        }
    }

    let job_done = sum.errors.is_empty() && !sum.stopped;
    if job_done {
        jr.append_and_commit(&Record::Complete)?;
        jr.remove()?;
    }
    sum.elapsed_secs = started.elapsed().as_secs_f64();
    // Local history line, best-effort — recording must never fail a finished transfer.
    journal::history_append(&journal::HistoryEntry {
        at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        src: journal::lexical_abs(src).to_string_lossy().into_owned(),
        dst: journal::lexical_abs(dst).to_string_lossy().into_owned(),
        mode: opts.mode,
        copied: sum.files_copied,
        skipped: sum.files_skipped,
        bytes: sum.bytes_written,
        secs: sum.elapsed_secs,
        errors: sum.errors.len(),
        done: job_done,
    });
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe is the only same-directory defence that works on Windows, where
    /// `same_file` cannot compare inodes. Exercised here through a symlink so it is
    /// tested on its own rather than shadowed by the inode check.
    #[test]
    fn probe_detects_the_same_directory_through_an_alias() {
        let base = std::env::temp_dir().join(format!("kevat-probe-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let real = base.join("real");
        let other = base.join("other");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(&other).unwrap();

        #[cfg(unix)]
        {
            let alias = base.join("alias");
            std::os::unix::fs::symlink(&real, &alias).unwrap();
            assert!(probe_same_dir(&real, &alias), "alias of the same directory");
            assert!(!probe_same_dir(&real, &other), "genuinely different directories");
            // Nothing may be left behind by either outcome.
            assert_eq!(fs::read_dir(&real).unwrap().count(), 0, "probe not cleaned up");
        }

        let _ = fs::remove_dir_all(&base);
    }
}
