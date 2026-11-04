use crate::config::Target;
use crate::lock::Lock;
use crate::pins::Entry;
use crate::pins::write_atomic;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|d| state_dir_in(&d))
}

/// The state directory under `base`. One from when eppur was called nixheal is
/// taken over, as its last good locks are what a failing package is pinned to.
fn state_dir_in(base: &Path) -> PathBuf {
    let dir = base.join("eppur");
    let old = base.join("nixheal");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    dir
}

pub fn key(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Where the last lock that built is kept: one file per flake and target, and
/// one per flake for targets that have not built yet.
fn last_good_path(flake: &Path, target: Option<&Target>) -> Option<PathBuf> {
    let flake = flake.to_string_lossy();
    let name = match target {
        Some(t) => format!("last-good-{}.lock", key(&format!("{flake}\0{t}"))),
        None => format!("last-good-{}.lock", key(&flake)),
    };
    Some(state_dir()?.join(name))
}

/// The run lock for a flake that is not in a git repository.
pub fn lock_path(flake: &Path) -> Result<PathBuf> {
    let dir = state_dir().context("neither XDG_STATE_HOME nor HOME is set")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!("run-{}.lock", key(&flake.to_string_lossy()))))
}

/// The pins a target built with on its last good lock: a lock only counts as
/// good together with them, so a package pinned then is pinned to the same
/// revision when it fails again.
fn last_good_pins_path(flake: &Path, target: &Target) -> Option<PathBuf> {
    let lock = last_good_path(flake, Some(target))?;
    Some(lock.with_extension("pins.json"))
}

pub fn load_last_good_pins(flake: &Path, target: &Target) -> Option<Entry> {
    let bytes = std::fs::read(last_good_pins_path(flake, target)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn save_last_good_pins(flake: &Path, target: &Target, entry: &Entry) -> Result<()> {
    let Some(path) = last_good_pins_path(flake, target) else {
        return Ok(());
    };
    let mut bytes = serde_json::to_vec_pretty(entry)?;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)
}

pub fn load_last_good(flake: &Path, target: Option<&Target>) -> Option<Lock> {
    Lock::load(&last_good_path(flake, target)?).ok()
}

pub fn save_last_good(flake: &Path, target: Option<&Target>, lock: &Lock) -> Result<()> {
    let Some(path) = last_good_path(flake, target) else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    write_atomic(&path, lock.bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pins::Kind;

    #[test]
    fn takes_over_the_old_state_directory() {
        let base = std::env::temp_dir().join(format!("eppur-state-{}", std::process::id()));
        let old = base.join("nixheal");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("last-good.lock"), "{}").unwrap();
        let dir = state_dir_in(&base);
        assert!(dir.join("last-good.lock").exists());
        assert!(!old.exists());

        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("stale.lock"), "{}").unwrap();
        state_dir_in(&base);
        assert!(!dir.join("stale.lock").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn one_file_per_target() {
        let flake = Path::new("/home/alice/nixos");
        let a = last_good_path(flake, Some(&Target::new(Kind::Nixos, "a")));
        let b = last_good_path(flake, Some(&Target::new(Kind::Nixos, "b")));
        let home = last_good_path(flake, Some(&Target::new(Kind::Home, "a")));
        let any = last_good_path(flake, None);
        assert_ne!(a, b);
        assert_ne!(a, home);
        assert_ne!(a, any);
    }
}
