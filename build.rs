//! Windows-only: embed the app icon and version info into the .exe resource section.
//!
//! Without this, Explorer, the Start Menu and Task Manager show the generic
//! executable icon — the one visible difference between "a binary in a zip" and
//! an application. winresource only exists as a dependency when the *target* is
//! Windows (see Cargo.toml), and this repo builds Windows on a Windows runner, so
//! host and target agree and plain `#[cfg(windows)]` is sufficient here.

#[cfg(windows)]
fn main() {
    // Re-run only when the icon changes, not on every source edit.
    println!("cargo:rerun-if-changed=assets/icon/kevat.ico");
    // A missing rc.exe on some future runner image must cost the icon, not the
    // release: this is cosmetic, and failing the whole Windows build over it would
    // block every platform's assets (the publish job needs all six).
    if let Err(e) = winresource::WindowsResource::new()
        .set_icon("assets/icon/kevat.ico")
        // Task Manager and file Properties show these; winresource fills the
        // version numbers from CARGO_PKG_VERSION on its own.
        .set("ProductName", "Kevat")
        .set("FileDescription", "Kevat — fast, resumable copy/move to external drives")
        .compile()
    {
        println!("cargo:warning=could not embed Windows resources (icon/version block): {e}");
    }
}

#[cfg(not(windows))]
fn main() {}
