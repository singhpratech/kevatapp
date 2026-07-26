//! Enumerate-first walker. The whole manifest exists before byte zero — that is what
//! makes progress, ETA and resume honest rather than estimated.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Folder names that hold only rebuildable application state — caches, package stores,
/// browser profiles. On a 363k-file user profile these are ~90% of the file *count* and
/// almost none of the value, and small files are exactly what an external HDD is worst
/// at. Deliberately conservative: nothing on this list may ever be the only copy of a
/// person's data (`.ssh`, `.gnupg`, `Documents` must never appear here — keys and
/// documents are not rebuildable).
pub const CACHE_NAMES: &[&str] = &[
    "AppData",
    "node_modules",
    ".cache",
    "__pycache__",
    ".npm",
    ".nuget",
    ".gradle",
    ".m2",
    ".rustup",
    ".thumbnails",
    "$RECYCLE.BIN",
    "System Volume Information",
    // A real profile measured 312,960 files — 84% of everything — inside one anaconda3
    // install: a reinstallable interpreter farm, the worst possible payload for a USB
    // HDD. Environments are rebuilt from a spec, not file-copied.
    "anaconda3",
    "miniconda3",
    ".conda",
    // ~/.vscode is downloaded extensions; settings live under AppData\Roaming\Code.
    ".vscode",
    // macOS: ~/Library/Caches and Xcode's build products; Linux/mac Python venvs.
    // Name matching is platform-agnostic — a name that never occurs never matches.
    "Caches",
    "DerivedData",
    ".venv",
];

/// Cloud-sync folder name prefixes, for the GUI's "leave out cloud folders" choice.
/// Prefixes, not names: work accounts sync to "OneDrive - Tenant". All platforms —
/// a service not installed simply never matches.
pub const CLOUD_PREFIXES: &[&str] = &["OneDrive", "Dropbox", "Google Drive", "iCloud"];

/// Names to leave out of a scan, matched against single path components,
/// ASCII-case-insensitively (`appdata` prunes `AppData` — the tree being rescued is
/// often on a case-insensitive filesystem even when the scan runs elsewhere).
///
/// The scan *root* and explicitly selected names are never filtered: choosing a folder
/// by hand outranks any rule about its name.
#[derive(Default, Clone)]
pub struct Filter {
    names: Vec<String>,
    /// Component *prefixes*: "OneDrive" must also prune "OneDrive - CECA" — cloud
    /// folders carry the tenant name in the folder name, so exact matching misses
    /// every work account.
    prefixes: Vec<String>,
}

impl Filter {
    pub fn none() -> Self {
        Filter::default()
    }

    /// The built-in app-cache list, the GUI's "skip app caches" toggle.
    pub fn caches() -> Self {
        Filter { names: CACHE_NAMES.iter().map(|s| s.to_string()).collect(), prefixes: Vec::new() }
    }

    pub fn add(&mut self, name: &str) {
        self.names.push(name.to_string());
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn add_prefix(&mut self, prefix: &str) {
        self.prefixes.push(prefix.to_string());
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    fn excludes(&self, name: &OsStr) -> bool {
        if self.names.is_empty() && self.prefixes.is_empty() {
            return false;
        }
        let name = name.to_string_lossy();
        self.names.iter().any(|n| n.eq_ignore_ascii_case(&name))
            || self.prefixes.iter().any(|p| {
                // .get, not [..]: slicing mid-codepoint on a non-ASCII name would panic.
                name.get(..p.len()).is_some_and(|s| s.eq_ignore_ascii_case(p))
            })
    }

    /// Would any component of this relative path be pruned? Used on an *unfiltered*
    /// manifest to count what a filter would have saved — the "most of what's left is
    /// app caches" hint needs the number without a second walk.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn matches_path(&self, rel: &Path) -> bool {
        rel.components().any(|c| self.excludes(c.as_os_str()))
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Path relative to the scan root. Byte-exact, never case-folded.
    pub rel: PathBuf,
    pub size: u64,
    pub mtime: i64,
    /// Nanosecond mtime, for the checkpoint source-identity check only. Whole seconds
    /// are too coarse there: a same-size source regenerated within the same second as
    /// the original slips past and gets spliced onto the stale prefix. The destination
    /// round-trip checks stay on seconds — FAT-family drives cannot store finer.
    pub mtime_ns: i64,
}

#[derive(Debug, Default)]
pub struct Manifest {
    pub dirs: Vec<PathBuf>,
    pub files: Vec<Entry>,
    pub total_bytes: u64,
    /// Paths skipped, with the reason. Reported, never silently dropped.
    pub skipped: Vec<(PathBuf, String)>,
    /// Whole subtrees pruned by the caller's `Filter`, by relative path. Separate from
    /// `skipped`: these are a choice the user made, not a limitation, and they must not
    /// trip the "everything was skipped" failure exit.
    pub excluded: Vec<PathBuf>,
    /// True when the scan root is itself a file. The single entry's `rel` is then the
    /// file's own name, and the engine must resolve the destination as a file path —
    /// joining root onto rel would produce `one.txt/one.txt`.
    pub root_is_file: bool,
}

impl Manifest {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

pub fn mtime_of(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn mtime_ns_of(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Scan a chosen *set* of entries inside one folder — what a multi-selection in the
/// picker means. Each selected name keeps its own name at the destination, so the
/// relative paths start at the selection, not at the folder holding it.
///
/// This exists so N selected items are ONE job with one manifest, one journal and one
/// progress total. Running them as N separate transfers would be simpler, but it would
/// break two-phase move's whole point: the guarantee is that nothing is deleted until
/// *everything* is proven, and N independent jobs would each delete their own sources
/// as they finished.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn scan_selected_with(parent: &Path, names: &[PathBuf], filter: &Filter) -> io::Result<Manifest> {
    let mut m = Manifest::default();
    let mut queue: Vec<(PathBuf, PathBuf)> = Vec::new();
    for name in names {
        let abs = parent.join(name);
        let md = match fs::metadata(&abs) {
            Ok(md) => md,
            Err(e) => {
                m.skipped.push((name.clone(), format!("unreadable: {e}")));
                continue;
            }
        };
        if md.is_dir() {
            m.dirs.push(name.clone());
            queue.push((abs, name.clone()));
        } else if md.is_file() {
            m.total_bytes += md.len();
            m.files.push(Entry {
                rel: name.clone(),
                size: md.len(),
                mtime: mtime_of(&md),
                mtime_ns: mtime_ns_of(&md),
            });
        } else {
            m.skipped.push((name.clone(), "not a regular file".into()));
        }
    }
    walk(&mut m, queue, filter);
    m.dirs.sort_by_key(|d| d.components().count());
    m.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(m)
}

/// The breadth-walk shared by `scan` and `scan_selected`: drain a queue of
/// (absolute, relative-to-the-job-root) directories, recording what is inside.
fn walk(m: &mut Manifest, mut queue: Vec<(PathBuf, PathBuf)>, filter: &Filter) {
    while let Some((abs, rel)) = queue.pop() {
        let rd = match fs::read_dir(&abs) {
            Ok(rd) => rd,
            Err(e) => {
                m.skipped.push((abs, format!("unreadable directory: {e}")));
                continue;
            }
        };

        for ent in rd {
            let ent = match ent {
                Ok(e) => e,
                Err(e) => {
                    m.skipped.push((abs.clone(), format!("unreadable entry: {e}")));
                    continue;
                }
            };
            let child_abs = ent.path();
            let child_rel = rel.join(ent.file_name());

            // Filtered before the metadata call: pruning a `node_modules` here saves
            // not just its copy but the stat of every one of its descendants.
            if filter.excludes(&ent.file_name()) {
                m.excluded.push(child_rel);
                continue;
            }

            // symlink_metadata: never follow links during enumeration, or a link pointing
            // upward turns the walk into an infinite loop.
            let md = match fs::symlink_metadata(&child_abs) {
                Ok(md) => md,
                Err(e) => {
                    m.skipped.push((child_abs, format!("unreadable: {e}")));
                    continue;
                }
            };
            let ft = md.file_type();

            if ft.is_symlink() {
                m.skipped.push((child_rel, "symlink (not handled in v0.1)".into()));
            } else if ft.is_dir() {
                m.dirs.push(child_rel.clone());
                queue.push((child_abs, child_rel));
            } else if ft.is_file() {
                m.total_bytes += md.len();
                m.files.push(Entry {
                    rel: child_rel,
                    size: md.len(),
                    mtime: mtime_of(&md),
                    mtime_ns: mtime_ns_of(&md),
                });
            } else {
                m.skipped.push((child_rel, "not a regular file".into()));
            }
        }
    }
}

/// Walk `root` breadth-first. Directories are recorded before their contents so the
/// destination tree can be created in the same order.
pub fn scan_with(root: &Path, filter: &Filter) -> io::Result<Manifest> {
    let mut m = Manifest::default();
    // The *root* argument resolves through a symlink deliberately: `kevat link.txt out`
    // names the link's target as the thing to copy. With symlink_metadata here, a
    // symlink-to-a-file root fell into the directory walk, produced "0 file(s)", exit 0
    // — and created a *directory* at the destination, so a script chaining
    // `kevat "$f" "$d" && rm "$f"` deleted its data on a success that copied nothing.
    // Links *inside* the tree stay unfollowed (below); only the root is special.
    let root_md = fs::metadata(root)?;

    if root_md.is_file() {
        m.root_is_file = true;
        m.total_bytes = root_md.len();
        m.files.push(Entry {
            rel: PathBuf::from(
                root.file_name()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no file name"))?,
            ),
            size: root_md.len(),
            mtime: mtime_of(&root_md),
            mtime_ns: mtime_ns_of(&root_md),
        });
        return Ok(m);
    }

    // (absolute, relative-to-root) pairs still to visit.
    walk(&mut m, vec![(root.to_path_buf(), PathBuf::new())], filter);

    // Shallow-first, so `create_dir` never needs a missing parent.
    m.dirs.sort_by_key(|d| d.components().count());
    // Deterministic order makes a resumed run visit files in the same sequence.
    m.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(m)
}

#[cfg(test)]
mod selected_tests {
    use super::*;

    /// A multi-selection copies exactly the chosen entries, each keeping its own name,
    /// and nothing else from the folder around them.
    #[test]
    fn scan_selected_takes_only_the_chosen_names() {
        let base = std::env::temp_dir().join(format!("kevat-sel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a")).unwrap();
        fs::create_dir_all(base.join("b")).unwrap();
        fs::create_dir_all(base.join("skipme")).unwrap();
        fs::write(base.join("a/one.txt"), b"1").unwrap();
        fs::write(base.join("b/two.txt"), b"22").unwrap();
        fs::write(base.join("skipme/no.txt"), b"333").unwrap();
        fs::write(base.join("top.txt"), b"4444").unwrap();

        let names = vec![PathBuf::from("a"), PathBuf::from("top.txt")];
        let m = scan_selected_with(&base, &names, &Filter::none()).unwrap();

        let rels: Vec<String> =
            m.files.iter().map(|e| e.rel.to_string_lossy().into_owned()).collect();
        assert!(rels.iter().any(|r| r.ends_with("one.txt")), "chosen folder's file missing");
        assert!(rels.iter().any(|r| r == "top.txt"), "chosen file missing");
        assert!(!rels.iter().any(|r| r.contains("two.txt")), "unchosen sibling included");
        assert!(!rels.iter().any(|r| r.contains("no.txt")), "unchosen folder included");
        // Names are preserved relative to the selection, not the folder holding it.
        assert!(rels.iter().any(|r| r == &format!("a{}one.txt", std::path::MAIN_SEPARATOR)));
        assert_eq!(m.total_bytes, 1 + 4);
        let _ = fs::remove_dir_all(&base);
    }

    /// The cache filter prunes whole subtrees by component name, case-insensitively,
    /// records what it pruned, and never touches the explicitly selected roots — a
    /// user who *chose* a folder named AppData gets it copied.
    #[test]
    fn filter_prunes_cache_folders_but_not_chosen_roots() {
        let base = std::env::temp_dir().join(format!("kevat-filter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("docs")).unwrap();
        fs::create_dir_all(base.join("appdata/Local/deep")).unwrap();
        fs::create_dir_all(base.join("proj/node_modules/pkg")).unwrap();
        fs::write(base.join("docs/keep.txt"), b"keep").unwrap();
        fs::write(base.join("appdata/Local/deep/cache.bin"), b"junk").unwrap();
        fs::write(base.join("proj/node_modules/pkg/index.js"), b"junk").unwrap();
        fs::write(base.join("proj/main.rs"), b"code").unwrap();

        let m = scan_with(&base, &Filter::caches()).unwrap();
        let rels: Vec<String> =
            m.files.iter().map(|e| e.rel.to_string_lossy().into_owned()).collect();
        assert!(rels.iter().any(|r| r.ends_with("keep.txt")));
        assert!(rels.iter().any(|r| r.ends_with("main.rs")));
        assert!(!rels.iter().any(|r| r.contains("cache.bin")), "AppData not pruned");
        assert!(!rels.iter().any(|r| r.contains("index.js")), "node_modules not pruned");
        assert_eq!(m.excluded.len(), 2, "each pruned subtree recorded once");
        assert_eq!(m.total_bytes, 4 + 4);

        // Selecting the folder by name outranks the rule about its name.
        let sel = scan_selected_with(
            &base,
            &[PathBuf::from("appdata")],
            &Filter::caches(),
        )
        .unwrap();
        let sel_rels: Vec<String> =
            sel.files.iter().map(|e| e.rel.to_string_lossy().into_owned()).collect();
        assert!(
            sel_rels.iter().any(|r| r.ends_with("cache.bin")),
            "explicitly chosen root was filtered away"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
