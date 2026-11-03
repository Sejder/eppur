//! Flake inputs held back in flake.lock. A package from a flake input other
//! than a package set never passes through the overlay, so it cannot be pinned
//! the way a nixpkgs package is. The lock has one revision per input, so an
//! input that breaks a build goes back to the revision it last built with for
//! the whole flake, and moves on again once its new revision builds.

use crate::date;
use crate::engine::describe;
use crate::lock::Lock;
use crate::log;
use crate::nix::Nix;
use crate::pins::{InputPin, PinsFile, Reason};
use crate::resolve;
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// An input to take back to `locked`, and what made nixheal do it.
#[derive(Debug, Clone)]
pub struct Rollback {
    pub input: String,
    pub locked: Value,
    pub failed_nar_hash: String,
    pub reason: Reason,
    pub failed_drv: Option<String>,
    pub error: Option<String>,
    pub targets: Vec<String>,
}

/// What to do with an input pin, given the input's narHash in the lock.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// The lock has the pinned revision.
    Keep,
    /// The lock has the revision that failed: nothing new to try.
    Restore,
    /// The lock has a revision not tried yet: let the build test it.
    Retry,
    /// The flake has no such input any more.
    Drop,
}

pub fn step(pin: &InputPin, current: Option<&str>) -> Step {
    match current {
        None => Step::Drop,
        Some(c) if pin.good_nar_hash() == Some(c) => Step::Keep,
        Some(c) if pin.failed_nar_hash == c => Step::Restore,
        Some(_) => Step::Retry,
    }
}

/// The root inputs that may be rolled back: they changed since `good`, are
/// the flake's own rather than a `follows`, still come from where they came
/// from (a new branch or URL in flake.nix is not to be undone), and are not
/// excluded.
pub fn candidates(
    good: &Lock,
    new: &Lock,
    excluded: &BTreeSet<String>,
    deny: &[String],
) -> Vec<String> {
    let mut names: Vec<String> = new
        .inputs()
        .into_iter()
        .filter(|i| !excluded.contains(i) && !denied(i, deny) && new.is_direct(i))
        .filter(|i| good.original(i) == new.original(i))
        .filter(|i| match (good.nar_hash(i), new.nar_hash(i)) {
            (Some(g), Some(n)) => g != n,
            _ => false,
        })
        .collect();
    names.sort();
    names
}

pub fn denied(input: &str, deny: &[String]) -> bool {
    deny.iter().any(|p| resolve::glob(p, input))
}

fn set_lock(nix: &Nix, lock_path: &Path, wanted: &[(String, Value)]) -> Result<()> {
    let refs: Vec<(String, String)> = wanted
        .iter()
        .map(|(name, locked)| Ok((name.clone(), crate::nix::flake_ref(locked)?)))
        .collect::<Result<_>>()?;
    nix.lock_override(&refs)?;
    let lock = Lock::load(lock_path)?;
    for (name, locked) in wanted {
        if lock.nar_hash(name).as_deref() != locked["narHash"].as_str() {
            bail!(
                "flake.lock does not have {name} at {} after locking it there",
                describe(locked)
            );
        }
    }
    Ok(())
}

/// Bring the lock and the input pins in line after an update: an input that
/// came back with the revision that failed goes back to its pin, so the lock
/// stays the same while upstream does, and one with a new revision is marked
/// to be tried by the next build.
pub fn settle(nix: &Nix, lock_path: &Path, pins_path: &Path, deny: &[String]) -> Result<()> {
    let mut file = PinsFile::load(pins_path)?;
    if file.inputs.is_empty() {
        return Ok(());
    }
    let lock = Lock::load(lock_path)?;
    let before = file.inputs.clone();
    let mut restore = Vec::new();
    let names: Vec<String> = file.inputs.keys().cloned().collect();
    for name in names {
        let pin = file.inputs.get_mut(&name).expect("listed above");
        let current = lock.nar_hash(&name);
        let step = if denied(&name, deny) || (current.is_some() && !lock.is_direct(&name)) {
            Step::Drop
        } else {
            step(pin, current.as_deref())
        };
        match step {
            Step::Drop => {
                file.inputs.remove(&name);
                log::change(&format!(
                    "unpinned input {name}: it is no longer an input of its own, or inputDeny lists it"
                ));
            }
            Step::Keep => pin.retrying = false,
            Step::Restore => {
                pin.retrying = false;
                log::info(&format!(
                    "input {name} is still at the revision that failed; keeping it at {}",
                    describe(&pin.locked)
                ));
                restore.push((name, pin.locked.clone()));
            }
            Step::Retry => {
                pin.retrying = true;
                log::info(&format!(
                    "retrying input {name}: it moved since it was pinned"
                ));
            }
        }
    }
    for (name, locked) in restore {
        // Its revision may be gone upstream; that must not stop every update,
        // so the build tries what the lock has now, and a hold is the worst of it.
        if let Err(e) = set_lock(nix, lock_path, &[(name.clone(), locked)]) {
            if crate::guard::timed_out() {
                return Err(e);
            }
            log::warn(&format!(
                "cannot put input {name} back ({e:#}); trying its current revision. 'nixheal clear --flake-input {name}' removes the pin"
            ));
            if let Some(pin) = file.inputs.get_mut(&name) {
                pin.retrying = true;
            }
        }
    }
    if file.inputs != before {
        file.touch_inputs();
        crate::guard::critical(|| file.save(pins_path))?;
    }
    Ok(())
}

/// Take the inputs back in flake.lock and record the pins. Returns one line
/// per input for the run's changes.
pub fn apply(
    nix: &Nix,
    lock_path: &Path,
    pins_path: &Path,
    rollbacks: &[Rollback],
    today: i64,
) -> Result<Vec<String>> {
    let wanted: Vec<(String, Value)> = rollbacks
        .iter()
        .map(|r| (r.input.clone(), r.locked.clone()))
        .collect();
    set_lock(nix, lock_path, &wanted)?;
    let mut file = PinsFile::load(pins_path)?;
    let mut changes = Vec::new();
    for r in rollbacks {
        let existing = file.inputs.get(&r.input);
        let since = existing.map_or_else(|| date::format(today), |p| p.since.clone());
        // Rolled back again in the same run, the revision upstream has is the
        // one this pin first failed on, not the one it just came from.
        let failed_nar_hash = existing
            .filter(|p| !p.retrying)
            .map_or_else(|| r.failed_nar_hash.clone(), |p| p.failed_nar_hash.clone());
        let mut targets: BTreeSet<String> = r.targets.iter().cloned().collect();
        targets.extend(existing.into_iter().flat_map(|p| p.targets.iter().cloned()));
        let pin = InputPin {
            locked: r.locked.clone(),
            failed_nar_hash,
            since,
            reason: r.reason,
            failed_drv: r.failed_drv.clone(),
            error: r.error.clone(),
            targets: targets.into_iter().collect(),
            retrying: false,
        };
        log::change(&format!(
            "pinned input {} at {}: {}, on {}",
            r.input,
            describe(&r.locked),
            crate::summary::input_cause(&pin),
            r.targets.join(", ")
        ));
        changes.push(format!(
            "pinned input {}: {}",
            r.input,
            crate::summary::input_cause(&pin)
        ));
        file.inputs.insert(r.input.clone(), pin);
    }
    file.touch_inputs();
    crate::guard::critical(|| file.save(pins_path))?;
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pin() -> InputPin {
        InputPin {
            locked: json!({ "narHash": "sha256-good", "rev": "aaa" }),
            failed_nar_hash: "sha256-bad".into(),
            since: "2026-11-02".into(),
            reason: Reason::Build,
            failed_drv: None,
            error: None,
            targets: vec!["host a".into()],
            retrying: false,
        }
    }

    #[test]
    fn steps_follow_the_locked_hash() {
        assert_eq!(step(&pin(), Some("sha256-good")), Step::Keep);
        assert_eq!(step(&pin(), Some("sha256-bad")), Step::Restore);
        assert_eq!(step(&pin(), Some("sha256-new")), Step::Retry);
        assert_eq!(step(&pin(), None), Step::Drop);
    }

    fn lock(atlas: &str, todo: &str, nixpkgs: &str) -> Lock {
        Lock::from_json(&json!({
            "nodes": {
                "atlas": { "locked": { "narHash": atlas } },
                "todo": { "locked": { "narHash": todo } },
                "nixpkgs": { "locked": { "narHash": nixpkgs } },
                "root": { "inputs": {
                    "atlas": "atlas", "todo": "todo", "nixpkgs": "nixpkgs",
                    "alias": ["atlas"],
                } },
            },
            "root": "root",
        }))
        .unwrap()
    }

    #[test]
    fn candidates_are_the_changed_direct_inputs_not_excluded() {
        let old = lock("a1", "t1", "n1");
        let new = lock("a2", "t2", "n2");
        let none = BTreeSet::new();
        assert_eq!(
            candidates(&old, &new, &none, &[]),
            ["atlas", "nixpkgs", "todo"]
        );
        assert_eq!(candidates(&old, &old, &none, &[]), Vec::<String>::new());
        let sets = BTreeSet::from(["nixpkgs".to_string()]);
        assert_eq!(
            candidates(&old, &new, &sets, &["at*".to_string()]),
            ["todo"]
        );
        assert_eq!(
            candidates(&old, &new, &none, &["*".to_string()]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_input_moved_to_another_source_is_not_a_candidate() {
        let old = lock("a1", "t1", "n1");
        let mut json = lock("a2", "t2", "n2").json().clone();
        json["nodes"]["atlas"]["original"] = json!({ "type": "git", "ref": "dev" });
        let new = Lock::from_json(&json).unwrap();
        assert_eq!(
            candidates(&old, &new, &BTreeSet::new(), &[]),
            ["nixpkgs", "todo"]
        );
    }

    #[test]
    fn an_input_new_in_the_lock_is_not_a_candidate() {
        let old = Lock::from_json(&json!({
            "nodes": { "root": { "inputs": {} } }, "root": "root",
        }))
        .unwrap();
        let new = lock("a2", "t2", "n2");
        assert_eq!(
            candidates(&old, &new, &BTreeSet::new(), &[]),
            Vec::<String>::new()
        );
    }
}
