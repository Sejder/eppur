//! Flake inputs held back in flake.lock. A package from a flake input other
//! than a package set never passes through the overlay, so it cannot be pinned
//! the way a nixpkgs package is. The lock has one revision per input, so an
//! input that breaks a build goes back to the revision it last built with for
//! the whole flake, and moves on again once its new revision builds.

use crate::date;
use crate::engine::{Change, describe, describe_dated};
use crate::lock::Lock;
use crate::log;
use crate::nix::Nix;
use crate::pins::{InputPin, PinsFile, Reason};
use crate::resolve;
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// An input to take back to `locked`, and what made eppur do it.
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

/// One line per root input an update moved, added or removed, from the lock
/// before the update to the lock once settled: an input put straight back
/// to its pin is not among them.
pub fn updated(before: &Lock, after: &Lock) -> Vec<String> {
    let names: BTreeSet<String> = before.inputs().into_iter().chain(after.inputs()).collect();
    names
        .into_iter()
        .filter_map(|name| match (before.locked(&name), after.locked(&name)) {
            (Some(b), Some(a)) if b["narHash"] != a["narHash"] => Some(format!(
                "updated input {name}: {} -> {}",
                describe_dated(&b),
                describe_dated(&a)
            )),
            (None, Some(a)) => Some(format!("added input {name} at {}", describe_dated(&a))),
            (Some(_), None) => Some(format!("removed input {name}")),
            _ => None,
        })
        .collect()
}

/// Log what an update changed in the lock.
pub fn report_update(before: &Lock, lock_path: &Path) -> Result<()> {
    let lines = updated(before, &Lock::load(lock_path)?);
    if lines.is_empty() {
        log::info("no input changed");
    }
    for line in lines {
        log::info(&line);
    }
    Ok(())
}

fn announces_retry(pin: &InputPin, building: Option<&[String]>) -> bool {
    building.is_none_or(|b| pin.targets.is_empty() || pin.targets.iter().any(|t| b.contains(t)))
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
/// to be tried by the next build. `building` are the targets of the run that
/// follows, if any: a retry only matters to a run that builds a target the pin
/// failed on, so it is only announced there.
pub fn settle(
    nix: &Nix,
    lock_path: &Path,
    pins_path: &Path,
    deny: &[String],
    building: Option<&[String]>,
) -> Result<()> {
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
                let failed = lock
                    .locked(&name)
                    .map_or_else(|| "?".into(), |l| describe(&l));
                log::info(&format!(
                    "input {name}: {failed} is the revision that failed; keeping it at {}",
                    describe(&pin.locked)
                ));
                restore.push((name, pin.locked.clone()));
            }
            Step::Retry => {
                pin.retrying = true;
                if announces_retry(pin, building) {
                    log::info(&format!("retrying input {name}"));
                }
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
                "cannot put input {name} back ({e:#}); trying its current revision"
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

/// Take the inputs back in flake.lock and record the pins. `built` are the
/// targets that built in the run that found the rollbacks. Returns one line
/// per input for the run's changes.
pub fn apply(
    nix: &Nix,
    lock_path: &Path,
    pins_path: &Path,
    rollbacks: &[Rollback],
    built: &BTreeSet<String>,
    today: i64,
) -> Result<Vec<Change>> {
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
        let targets = pin_targets(r, existing, built);
        let pin = InputPin {
            locked: r.locked.clone(),
            failed_nar_hash,
            since,
            reason: r.reason,
            failed_drv: r.failed_drv.clone(),
            error: r.error.clone(),
            targets,
            retrying: false,
        };
        log::change(&format!(
            "pinned input {} at {}: {}; breaks {}",
            r.input,
            describe(&r.locked),
            crate::summary::input_cause(&pin),
            r.targets.join(", ")
        ));
        changes.push(Change::Pinned(format!(
            "input {}: {}",
            r.input,
            crate::summary::input_cause(&pin)
        )));
        file.inputs.insert(r.input.clone(), pin);
    }
    file.touch_inputs();
    crate::guard::critical(|| file.save(pins_path))?;
    Ok(changes)
}

/// The targets a pin records: those the rollback is for, and those of the
/// pin it replaces, less any of them that just built the newer revision a
/// retry was testing.
fn pin_targets(r: &Rollback, existing: Option<&InputPin>, built: &BTreeSet<String>) -> Vec<String> {
    let mut targets: BTreeSet<String> = r.targets.iter().cloned().collect();
    if let Some(p) = existing {
        targets.extend(
            p.targets
                .iter()
                .filter(|t| !(p.retrying && built.contains(*t)))
                .cloned(),
        );
    }
    targets.into_iter().collect()
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

    fn rollback(targets: &[&str]) -> Rollback {
        Rollback {
            input: "atlas".into(),
            locked: json!({ "narHash": "sha256-good" }),
            failed_nar_hash: "sha256-newer".into(),
            reason: Reason::Build,
            failed_drv: None,
            error: None,
            targets: targets.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn a_repin_after_a_retry_drops_targets_that_built_the_newer_revision() {
        let mut old = pin();
        old.targets = vec!["host home".into(), "host pi".into(), "host vps".into()];
        old.retrying = true;
        let built = BTreeSet::from(["host home".to_string()]);
        assert_eq!(
            pin_targets(&rollback(&["host pi"]), Some(&old), &built),
            ["host pi", "host vps"]
        );
        old.retrying = false;
        assert_eq!(
            pin_targets(&rollback(&["host pi"]), Some(&old), &built),
            ["host home", "host pi", "host vps"]
        );
        assert_eq!(
            pin_targets(&rollback(&["host pi"]), None, &built),
            ["host pi"]
        );
    }

    #[test]
    fn a_retry_is_announced_only_to_a_run_building_a_target_it_failed_on() {
        let ideapad = ["host ideapad".to_string()];
        let fleet = ["host ideapad".to_string(), "host a".to_string()];
        assert!(!announces_retry(&pin(), Some(&ideapad)));
        assert!(announces_retry(&pin(), Some(&fleet)));
        assert!(announces_retry(&pin(), None));
        let mut untargeted = pin();
        untargeted.targets.clear();
        assert!(announces_retry(&untargeted, Some(&ideapad)));
    }

    #[test]
    fn an_update_lists_only_the_inputs_that_moved() {
        let before = lock("a1", "t1", "n1");
        assert!(updated(&before, &before).is_empty());
        let mut json = lock("a1", "t2", "n1").json().clone();
        json["nodes"]["todo"]["locked"]["rev"] = json!("0123456789abcdef");
        json["nodes"]["root"]["inputs"]
            .as_object_mut()
            .unwrap()
            .remove("nixpkgs");
        let after = Lock::from_json(&json).unwrap();
        assert_eq!(
            updated(&before, &after),
            [
                "removed input nixpkgs",
                "updated input todo: t1 -> 0123456789ab"
            ]
        );
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
