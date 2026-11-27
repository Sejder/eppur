//! What locks the flake's inputs: flake.lock, or tack's .tack/pins.lock.json.
//! A run works on one of them; which one is decided once, when the flake is
//! opened, and read from here by everything that touches the lock.

use anyhow::{Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

/// How the lock is chosen, from programs.eppur.locker or --locker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Choice {
    /// tack when the flake has .tack/pins.toml, flake.lock otherwise.
    #[default]
    Auto,
    Flake,
    Tack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locker {
    Flake,
    Tack,
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set(locker: Locker) {
    CURRENT.store(
        match locker {
            Locker::Flake => 0,
            Locker::Tack => 1,
        },
        Ordering::Relaxed,
    );
}

pub fn current() -> Locker {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Locker::Tack,
        _ => Locker::Flake,
    }
}

pub fn is_tack() -> bool {
    current() == Locker::Tack
}

pub const TACK_DIR: &str = ".tack";

pub fn tack_dir(flake: &Path) -> PathBuf {
    flake.join(TACK_DIR)
}

/// The locker `choice` picks for `flake`. A flake whose flake.lock still
/// locks inputs of its own besides tack's pins is refused under tack: each
/// input must have one place it is locked in, or a rollback in one would be
/// undone by the other.
pub fn detect(flake: &Path, choice: Choice) -> Result<Locker> {
    let has_tack = tack_dir(flake).join("pins.toml").is_file();
    let locker = match choice {
        Choice::Flake => Locker::Flake,
        Choice::Tack => Locker::Tack,
        Choice::Auto if has_tack => Locker::Tack,
        Choice::Auto => Locker::Flake,
    };
    if locker == Locker::Tack {
        if !has_tack {
            bail!(
                "{} has no .tack/pins.toml; run tack init there, or set programs.eppur.locker (--locker) to flake",
                flake.display()
            );
        }
        if let Ok(lock) = crate::lock::Lock::load(&flake.join("flake.lock"))
            && !lock.inputs().is_empty()
        {
            let mut inputs = lock.inputs();
            inputs.sort();
            bail!(
                "{} locks its inputs in both flake.lock ({}) and tack; move them to tack (tack init --convert), or set programs.eppur.locker (--locker) to flake",
                flake.display(),
                inputs.join(", ")
            );
        }
    }
    Ok(locker)
}

impl Locker {
    pub fn lock_path(self, flake: &Path) -> PathBuf {
        match self {
            Locker::Flake => flake.join("flake.lock"),
            Locker::Tack => tack_dir(flake).join("pins.lock.json"),
        }
    }
}

/// The lock file's name, for messages: "flake.lock" or "pins.lock.json".
pub fn lock_name() -> &'static str {
    match current() {
        Locker::Flake => "flake.lock",
        Locker::Tack => "pins.lock.json",
    }
}

/// What a run writes, for messages: "flake.lock and pins.json", or tack's
/// files and pins.json.
pub fn files() -> &'static str {
    match current() {
        Locker::Flake => "flake.lock and pins.json",
        Locker::Tack => ".tack and pins.json",
    }
}

/// What an input of the lock is called, for messages.
pub fn input_word() -> &'static str {
    match current() {
        Locker::Flake => "flake input",
        Locker::Tack => "tack pin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("eppur-locker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn auto_follows_the_tack_dir() {
        let d = dir("auto");
        assert_eq!(detect(&d, Choice::Auto).unwrap(), Locker::Flake);
        std::fs::create_dir_all(d.join(".tack")).unwrap();
        std::fs::write(d.join(".tack/pins.toml"), "").unwrap();
        assert_eq!(detect(&d, Choice::Auto).unwrap(), Locker::Tack);
        assert_eq!(detect(&d, Choice::Flake).unwrap(), Locker::Flake);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tack_without_its_dir_is_refused() {
        let d = dir("missing");
        assert!(detect(&d, Choice::Tack).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn inputs_locked_in_both_places_are_refused() {
        let d = dir("hybrid");
        std::fs::create_dir_all(d.join(".tack")).unwrap();
        std::fs::write(d.join(".tack/pins.toml"), "").unwrap();
        std::fs::write(
            d.join("flake.lock"),
            r#"{"nodes":{"root":{"inputs":{"nixpkgs":"nixpkgs"}},"nixpkgs":{}},"root":"root"}"#,
        )
        .unwrap();
        let err = detect(&d, Choice::Auto).unwrap_err().to_string();
        assert!(err.contains("both flake.lock (nixpkgs) and tack"), "{err}");
        std::fs::write(
            d.join("flake.lock"),
            r#"{"nodes":{"root":{}},"root":"root"}"#,
        )
        .unwrap();
        assert_eq!(detect(&d, Choice::Auto).unwrap(), Locker::Tack);
        let _ = std::fs::remove_dir_all(&d);
    }
}
