//! Kevat — fast, resumable copy/move to external drives.
//!
//! v0.1 is the CLI engine only: raw copy/move, journal, resume, verify.
//! The GUI, compression and adaptive write concurrency are later milestones.

// The kevatw.exe build: same source, GUI subsystem, so launching from Explorer or the
// Start Menu opens no console window. The plain build stays console-subsystem so the
// CLI keeps working pipes, redirection and exit codes. Rationale in Cargo.toml at the
// `windows-subsystem` feature. On non-Windows targets the attribute is compiled out.
#![cfg_attr(all(windows, feature = "windows-subsystem"), windows_subsystem = "windows")]

mod engine;
#[cfg(feature = "gui")]
mod gui;
mod journal;
mod scan;

use std::path::PathBuf;
use std::process::ExitCode;

use engine::Options;
use journal::Mode;

// concat!/env! so the help text cannot drift from the crate version the way a
// hand-written "0.1.0" here did.
const USAGE: &str = concat!(
    "kevat ",
    env!("CARGO_PKG_VERSION"),
    " — fast, resumable copy/move to external drives

USAGE:
    kevat <SRC> <DEST> [OPTIONS]

OPTIONS:
        --move          Delete each source file after its copy is verified and recorded
        --verify        Read the destination back and compare hashes (default on for --move)
        --no-verify     Disable the verify pass, including for --move
        --paranoid      Re-hash already-completed files instead of trusting size+mtime
        --exists=WHAT   When a destination file exists and differs: replace (default),
                        keep, or fail
        --skip-caches   Leave out rebuildable app-cache folders (AppData, node_modules,
                        .cache, __pycache__ and similar) — often ~90% of a user
                        profile's file count and the slowest part of the copy
        --skip-cloud    Leave out cloud-sync folders (OneDrive*, Dropbox*,
                        Google Drive*, iCloud*) — they already live on their
                        service's servers
        --exclude=NAME  Leave out every folder or file with this exact name, anywhere
                        in the tree (case-insensitive; repeatable)
        --dry-run       Report what would happen; write nothing
    -h, --help          Show this help
    -V, --version       Show version

EXAMPLES:
    kevat ~/Photos /media/usb            copy a folder onto a drive
    kevat ~/Photos /media/usb --move     move it, deleting originals only once proven
    kevat ~/Photos /media/usb --dry-run  report what would happen, write nothing

EXIT CODES:
    0  everything asked for was done
    1  the transfer ran but something failed (refused pair, errors, conflicts)
    2  the arguments could not be understood

Resume is automatic: run the same command again after an interruption.
"
);

#[derive(Default)]
struct Args {
    src: Option<PathBuf>,
    dst: Option<PathBuf>,
    r#move: bool,
    verify: Option<bool>,
    paranoid: bool,
    dry_run: bool,
    on_exists: engine::OnExists,
    skip_caches: bool,
    skip_cloud: bool,
    exclude: Vec<String>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args::default();
    let mut positional = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("kevat {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--move" => a.r#move = true,
            "--verify" => a.verify = Some(true),
            "--no-verify" => a.verify = Some(false),
            "--paranoid" => a.paranoid = true,
            v if v.starts_with("--exists=") => {
                a.on_exists = match &v["--exists=".len()..] {
                    "replace" => engine::OnExists::Replace,
                    "keep" => engine::OnExists::Keep,
                    "fail" => engine::OnExists::Fail,
                    other => {
                        return Err(format!(
                            "--exists must be replace, keep or fail (got {other:?})"
                        ))
                    }
                }
            }
            "--skip-caches" => a.skip_caches = true,
            "--skip-cloud" => a.skip_cloud = true,
            v if v.starts_with("--exclude=") => {
                let name = &v["--exclude=".len()..];
                if name.is_empty() {
                    return Err("--exclude needs a name (--exclude=node_modules)".into());
                }
                a.exclude.push(name.to_string());
            }
            "--dry-run" => a.dry_run = true,
            s if s.starts_with('-') => return Err(format!("unknown option: {s}")),
            s => positional.push(PathBuf::from(s)),
        }
    }
    let mut it = positional.into_iter();
    a.src = it.next();
    a.dst = it.next();
    if let Some(extra) = it.next() {
        return Err(format!("unexpected argument: {}", extra.display()));
    }
    Ok(a)
}

/// Is stderr a terminal? Progress redraws belong on a screen, never in a log file or a
/// pipe. `std::io::IsTerminal` is stable since 1.70; this crate targets 1.77.
fn is_terminal_stderr() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, U[0])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kevat: {e}\n");
            print!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    // No arguments and a GUI build: open the window. With arguments it stays a CLI,
    // so one binary serves both without a flag to remember.
    #[cfg(feature = "gui")]
    if args.src.is_none() && args.dst.is_none() {
        return match gui::launch() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("kevat: {e}");
                ExitCode::from(1)
            }
        };
    }

    let (Some(src), Some(dst)) = (args.src, args.dst) else {
        print!("{USAGE}");
        return ExitCode::from(2);
    };

    if !src.exists() {
        eprintln!("kevat: source does not exist: {}", src.display());
        return ExitCode::from(2);
    }

    let mode = if args.r#move { Mode::Move } else { Mode::Copy };
    // Verify defaults on for move, off for copy: a move deletes originals, so it must
    // only ever act on proven bytes.
    let verify = args.verify.unwrap_or(mode == Mode::Move);
    let opts = Options {
        mode,
        job_tag: None,
        verify,
        paranoid: args.paranoid,
        dry_run: args.dry_run,
        on_exists: args.on_exists,
        selection: Vec::new(),
        skip_caches: args.skip_caches,
        skip_cloud: args.skip_cloud,
    };

    let mut filter =
        if args.skip_caches { scan::Filter::caches() } else { scan::Filter::none() };
    if args.skip_cloud {
        for c in scan::CLOUD_PREFIXES {
            filter.add_prefix(c);
        }
    }
    for name in &args.exclude {
        filter.add(name);
    }
    let m = match scan::scan_with(&src, &filter) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("kevat: cannot scan {}: {e}", src.display());
            return ExitCode::from(1);
        }
    };
    println!(
        "{} file(s), {} to {}",
        m.file_count(),
        human(m.total_bytes),
        dst.display()
    );
    for (p, why) in &m.skipped {
        eprintln!("kevat: skipping {}: {}", p.display(), why);
    }
    // Chosen exclusions, stated so "why is folder X not on the drive?" has an answer
    // in the output. Capped: a profile can match the cache rule hundreds of times.
    if !m.excluded.is_empty() {
        println!("left out by --skip-caches/--exclude ({}):", m.excluded.len());
        for p in m.excluded.iter().take(20) {
            println!("  {}", p.display());
        }
        if m.excluded.len() > 20 {
            println!("  … and {} more", m.excluded.len() - 20);
        }
    }
    // A scan that found nothing *because everything was skipped* must not exit 0: a
    // script chaining `kevat "$f" "$d" && rm "$f"` reads 0 as "the data is safely
    // across". A genuinely empty tree (nothing skipped) stays a success, and a dry
    // run still gets its report.
    if m.file_count() == 0 && !m.skipped.is_empty() && !opts.dry_run {
        eprintln!("kevat: nothing copied — every entry was skipped");
        return ExitCode::from(1);
    }

    // Never overwrite silently: say how many destination files will be replaced before
    // a byte moves. --exists=keep or =fail change what happens; this only makes the
    // default visible.
    if !opts.dry_run && opts.on_exists == engine::OnExists::Replace {
        let clashes = engine::conflicts(&dst, &m);
        if !clashes.is_empty() {
            eprintln!(
                "kevat: {} file(s) already at the destination differ and will be replaced \
                 (use --exists=keep to leave them)",
                clashes.len()
            );
        }
    }

    // A live line on stderr while it works. Without it the CLI printed the scan line
    // and then nothing — possibly for hours — which reads as a hang on exactly the
    // long transfers this tool is for. stderr so `kevat … > list.txt` stays clean, and
    // only when stderr is a terminal so logs and pipes are not filled with redraws.
    let tty = is_terminal_stderr();
    let total = m.total_bytes;
    let started = std::time::Instant::now();
    let mut done: u64 = 0;
    let mut last_paint = std::time::Instant::now();
    let mut painted = false;
    let mut eta_note = String::new();
    let mut err_note = String::new();
    let mut on_progress = |p: engine::Progress| {
        if !tty {
            return;
        }
        match p {
            engine::Progress::Bytes(n) | engine::Progress::Skipped(n) => done += n,
            engine::Progress::Dirs { done, total } => {
                // Folder creation moves no file bytes; without its own line, a big
                // tree on a slow drive reads as a hang before the first byte.
                if last_paint.elapsed() >= std::time::Duration::from_millis(100) {
                    last_paint = std::time::Instant::now();
                    eprint!("\r\x1b[K  creating folders  {done} / {total}");
                    painted = true;
                }
                return;
            }
            engine::Progress::Eta { seconds, small_left, small_secs, .. } => {
                eta_note = match seconds {
                    Some(s) if s >= 3600 => format!("  about {} h {} m left", s / 3600, (s % 3600 + 30) / 60),
                    Some(s) if s >= 60 => format!("  about {} m left", (s + 30) / 60),
                    Some(_) => "  under a minute left".to_string(),
                    None => String::new(),
                };
                // Name the culprit when it is the small files: the MB/s beside it looks
                // like a fault, and the fix (--skip-caches) is a flag away.
                if let (Some(t), Some(s)) = (seconds, small_secs) {
                    if s > 600 && s * 2 > t && small_left > 10_000 {
                        eta_note.push_str(&format!("  ({small_left} small files are most of it)"));
                    }
                }
                return;
            }
            engine::Progress::Errors { total } => {
                // Live, because 24,000 failures must build up in view during the run,
                // not ambush the user in the final report.
                err_note = format!("  · {total} couldn't be read");
                return;
            }
            engine::Progress::DeletePhase { total } => {
                eprint!("\r\x1b[K  removing originals ({total})\n");
                painted = false;
                return;
            }
            _ => return,
        }
        if last_paint.elapsed() < std::time::Duration::from_millis(100) {
            return;
        }
        last_paint = std::time::Instant::now();
        let secs = started.elapsed().as_secs_f64();
        let rate = if secs > 0.0 { done as f64 / secs } else { 0.0 };
        let pct = if total > 0 { done * 100 / total } else { 0 };
        eprint!(
            "\r\x1b[K  {pct:3}%  {} / {}  {:.0} MB/s{eta_note}{err_note}",
            human(done),
            human(total),
            rate / 1_000_000.0
        );
        painted = true;
    };
    let result = engine::run_with(
        &src,
        &dst,
        &m,
        &opts,
        &std::sync::atomic::AtomicBool::new(false),
        &mut on_progress,
    );
    if painted {
        eprint!("\r\x1b[K");
    }

    match result {
        Ok(s) => {
            if opts.dry_run {
                return ExitCode::SUCCESS;
            }
            let rate = if s.elapsed_secs > 0.0 {
                s.bytes_written as f64 / s.elapsed_secs / (1024.0 * 1024.0)
            } else {
                0.0
            };
            println!(
                "copied {} file(s), skipped {}, {} in {:.2}s ({:.1} MiB/s)",
                s.files_copied,
                s.files_skipped,
                human(s.bytes_written),
                s.elapsed_secs,
                rate
            );
            if s.files_verified > 0 {
                println!("verified {} file(s)", s.files_verified);
            }
            if s.sources_deleted > 0 {
                println!("removed {} source file(s)", s.sources_deleted);
            }
            if !s.errors.is_empty() {
                eprintln!("\n{} error(s):", s.errors.len());
                for (p, e) in &s.errors {
                    eprintln!("  {}: {e}", p.display());
                }
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kevat: {e}");
            ExitCode::from(1)
        }
    }
}
