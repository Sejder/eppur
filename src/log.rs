use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);
static DRY_RUN: AtomicBool = AtomicBool::new(false);
static HELD_BACK: Mutex<Vec<String>> = Mutex::new(Vec::new());

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// --verbose: progress is reported whatever the config's quiet says.
pub fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
}

pub fn set_quiet(quiet: bool) {
    if quiet && !VERBOSE.load(Ordering::Relaxed) {
        QUIET.store(true, Ordering::Relaxed);
    }
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// Mark changes as only what a dry run would do.
pub fn set_dry_run(dry: bool) {
    DRY_RUN.store(dry, Ordering::Relaxed);
}

/// Whether stderr is the journal: systemd sets JOURNAL_STREAM to the
/// device and inode of the stream it connected, which a terminal or a pipe a
/// child of the service opened does not match.
fn journal() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(stream) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Some((dev, ino)) = stream
        .to_string_lossy()
        .split_once(':')
        .map(|(d, i)| (d.to_string(), i.to_string()))
    else {
        return false;
    };
    std::fs::metadata("/proc/self/fd/2")
        .map(|m| m.dev().to_string() == dev && m.ino().to_string() == ino)
        .unwrap_or(false)
}

/// Progress; hidden with --quiet.
pub fn info(msg: &str) {
    if !is_quiet() {
        eprintln!("eppur: {msg}");
    }
}

/// Reasoning that only matters when debugging; shown only with --verbose.
pub fn detail(msg: &str) {
    if VERBOSE.load(Ordering::Relaxed) {
        eprintln!("eppur: {msg}");
    }
}

/// A pin, unpin, hold or release, or why eppur gave up; always shown.
pub fn change(msg: &str) {
    if DRY_RUN.load(Ordering::Relaxed) {
        eprintln!("eppur: (dry run) {msg}");
    } else {
        eprintln!("eppur: {msg}");
    }
}

/// A build or download nix starts, forwarded off a terminal so the journal
/// shows progress as plain nix would; hidden with --quiet.
pub fn nix_progress(msg: &str) {
    if !is_quiet() {
        eprintln!("{msg}");
    }
}

/// An error message forwarded from nix. With --quiet it is held back, and
/// only shown when eppur could not handle it (see flush_nix_errors).
pub fn nix_error(msg: &str) {
    if is_quiet() {
        if let Ok(mut held) = HELD_BACK.lock() {
            held.push(msg.to_string());
        }
    } else {
        eprintln!("{msg}");
    }
}

/// Print the nix errors held back under --quiet: a run is about to report a
/// hold, give up, or fail, and they say why.
pub fn flush_nix_errors() {
    if let Ok(mut held) = HELD_BACK.lock() {
        for msg in held.drain(..) {
            eprintln!("{msg}");
        }
    }
}

/// Drop the nix errors held back under --quiet: eppur handled them.
pub fn discard_nix_errors() {
    if let Ok(mut held) = HELD_BACK.lock() {
        held.clear();
    }
}

/// Logged at warning priority when stderr goes to the journal.
pub fn warn(msg: &str) {
    if journal() {
        eprintln!("<4>eppur: warning: {msg}");
    } else {
        eprintln!("eppur: warning: {msg}");
    }
}
