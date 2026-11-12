use crate::config::Target;
use crate::date;
use crate::graph::{Ascent, Graph};
use crate::guard;
use crate::inputs::{self, Rollback};
use crate::lock::Lock;
use crate::log;
use crate::logparse::{self, Failure};
use crate::nix::{Monitor, Nix};
use crate::pins::{Entry, Held, Kind, Pin, PinsFile, Reason};
use crate::resolve::{self, Matcher};
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

const ASCENT_CAP: usize = 20_000;

/// A rollback is tested by building what it changes under the failed drv's
/// name. More of those than this means the name says little, and the test
/// would build a large part of a system: the target is held instead.
const MAX_COUNTERPARTS: usize = 10;

/// A package more packages than this depend on is never pinned: the overlay
/// would swap it in for all of them, and every one would rebuild from source.
pub const MAX_DEPENDENTS: usize = 25;

pub struct Engine {
    pub nix: Nix,
    pub pins_path: PathBuf,
    pub new_lock: Lock,
    /// The last good lock for targets without one of their own.
    pub old_lock: Lock,
    /// The lock each target last built with, where it is known.
    pub last_good: HashMap<Target, Lock>,
    /// The pins each target built with on its last good lock.
    pub last_good_pins: HashMap<Target, Entry>,
    pub targets: Vec<Target>,
    /// The machine eppur runs on, when it is one of the targets and must not
    /// be held: its failure aborts the run instead of holding it.
    pub guarded: Option<Target>,
    /// The machine eppur runs on, when it is one of the targets.
    pub local: Option<Target>,
    /// Where pins.json sits in the flake's source tree: "pins.json", or
    /// "dir/pins.json" for a flake in a subdirectory of its repository.
    pub pins_rel: String,
    pub deny: Vec<String>,
    pub system: String,
    pub max_rounds: u32,
    pub max_new_pins: usize,
    pub warn_after_days: i64,
    pub today: i64,
    /// Inputs a failed leaf package may be pinned to instead.
    pub donors: Vec<String>,
    /// Flake inputs never to roll back.
    pub input_deny: Vec<String>,
    /// The package sets the config names, known even when no target evaluates.
    pub package_sets: Vec<String>,
    /// Every target the flake has, e.g. "host a", whether or not this run or
    /// this machine's config covers it; None when its outputs do not evaluate.
    pub flake_targets: Option<Vec<String>>,
    /// The names in nixosConfigurations and homeConfigurations, each None
    /// when that output does not evaluate: its targets are held for that by
    /// their probes.
    pub host_names: Option<Vec<String>>,
    pub home_names: Option<Vec<String>>,
    /// Whether a target with no pin to help may have flake inputs rolled back;
    /// off once a rollback in this run did not help.
    pub may_roll_back: bool,
    pub retry_same_after_days: i64,
    /// Whether a package nixpkgs now refuses as insecure may be pinned to the
    /// copy this machine already built; otherwise its targets are held.
    pub pin_insecure: bool,
    /// Evaluated copies of packages, by target, attr, revision and overlays.
    pub copies: std::cell::RefCell<HashMap<String, Option<Copy>>>,
    /// Whether a package set lost an attr between two revisions, by old
    /// revision, new revision, system and attr.
    pub lost: std::cell::RefCell<HashMap<(String, String, String, String), bool>>,
}

/// Something a run changed, for the closing line and the commit message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Pinned(String),
    Unpinned(String),
    Held(String),
    Released(String),
    Moved(String),
}

impl Change {
    /// How the closing line counts it.
    pub fn word(&self) -> &'static str {
        match self {
            Change::Pinned(_) => "pinned",
            Change::Unpinned(_) => "unpinned",
            Change::Held(_) => "held",
            Change::Released(_) => "released",
            Change::Moved(_) => "moved to a donor",
        }
    }
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Change::Pinned(what) => write!(f, "pinned {what}"),
            Change::Unpinned(what) => write!(f, "unpinned {what}"),
            Change::Held(what) => write!(f, "held {what}"),
            Change::Released(what) => write!(f, "released {what}"),
            Change::Moved(what) => write!(f, "moved pin {what}"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// One line per pin, unpin, hold and release, for logs and commit messages.
    pub changes: Vec<Change>,
    pub local_failure: Option<String>,
    /// The local machine's built system, for switching without evaluating it again.
    pub local_drv: Option<String>,
    /// Every target that built, with its system's drv.
    pub built: Vec<(Target, String)>,
    /// The pins each target that built built with.
    pub built_pins: Vec<(Target, Entry)>,
    /// The machine's own target was held (only possible with --hold-local).
    pub local_held: bool,
    /// How many targets are held after the run.
    pub held: usize,
    /// Every target that failed.
    pub failed: Vec<Target>,
    /// Flake inputs to take back before running again; when not empty the run
    /// recorded nothing.
    pub rollback: Vec<Rollback>,
}

/// What a target's own programs.eppur says, read before anything builds.
#[derive(Debug, serde::Deserialize)]
struct Probe {
    enable: bool,
    name: String,
    pins: Option<String>,
    sets: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum Status {
    Active,
    Done,
    Failed(String),
}

const PROBE: &str = "c: { enable = c.enable or false; name = c.name or \"\"; \
                     pins = if (c.pinsFile or null) == null then null else toString c.pinsFile; \
                     sets = builtins.attrNames (c.packageSets or { }); }";

/// One failure to pin around: a package, as every package set that provides
/// it names it. The members are pinned, retried and reported together.
struct Wanted {
    members: Vec<(String, String)>,
    reason: Reason,
    failed_drv: Option<String>,
    /// For a build failure, the drv of the package the members name.
    root_drv: Option<String>,
    /// Nothing else in the system depends on the package.
    leaf: bool,
    /// What failed, for messages: the failed derivation's name, or the error.
    cause: String,
    trail: Vec<String>,
    error: Option<String>,
    /// nixpkgs refuses it as insecure: only a copy this machine already
    /// built may be pinned.
    insecure: bool,
}

/// A failure no pin can help with, which a flake input may have caused.
#[derive(Debug, Clone)]
struct Unpinnable {
    /// The drv that failed; None when the target did not evaluate.
    failed_drv: Option<String>,
    error: String,
}

/// What stands in for the failed drv when an input is put back: the drvs of
/// the same name that the input changed. A system has many of the same name
/// (sources, vendored crates) that have nothing to do with the failure.
fn counterparts(
    failed: &str,
    closure: &HashSet<String>,
    baseline: &HashSet<String>,
) -> Vec<String> {
    let name = logparse::strip_version(crate::summary::drv_name(failed));
    closure
        .iter()
        .filter(|d| !baseline.contains(*d))
        .filter(|d| logparse::strip_version(crate::summary::drv_name(d)) == name)
        .cloned()
        .collect()
}

/// A package as one nixpkgs builds it for a target.
#[derive(Debug, Clone)]
pub struct Copy {
    drv: String,
    out: String,
    version: String,
    /// Whether the target's overlays were applied; None when it was imported
    /// plainly, for a target without importPinned.
    overlays: Option<bool>,
}

struct State {
    target: Target,
    base: Option<Entry>,
    entry: Entry,
    held_before: Option<Held>,
    held_lock: Option<Lock>,
    /// The pins this target built with on its last good lock.
    good_pins: Option<Entry>,
    old_lock: Lock,
    retrying: BTreeMap<(String, String), Pin>,
    /// Pins moved to a donor in this run, with the pin each replaced.
    upgrading: BTreeMap<(String, String), Pin>,
    inputs: Vec<String>,
    drv: Option<String>,
    /// The pins file the builds saw in the round this target built in, and
    /// the flake's source path in that round.
    built_with: Option<(Vec<u8>, Option<String>)>,
    system: std::cell::OnceCell<String>,
    status: Status,
    new_pins: usize,
    unpinnable: std::cell::RefCell<Option<Unpinnable>>,
}

impl Engine {
    pub fn run(&self) -> Result<Report> {
        self.check_targets()?;
        for donor in &self.donors {
            if self.new_lock.locked(donor).is_none() {
                log::warn(&format!(
                    "donor {donor} is not an input of flake.lock, so nothing is pinned to it"
                ));
            }
        }
        let original = PinsFile::load(&self.pins_path)?;
        // What each target needs evaluated before its first round, side by
        // side: each of these evaluates a whole configuration.
        let nix = &self.nix;
        let probes =
            crate::nix::parallel_map(&self.targets, |t| nix.eval_apply(&t.eppur_config(), PROBE));
        let requests: Vec<(&Target, Vec<CopyRequest>)> = self
            .targets
            .iter()
            .map(|t| (t, self.copy_requests(original.entry(t.kind, &t.name))))
            .collect();
        for found in crate::nix::parallel_map(&requests, |(t, r)| fetch_copies(nix, t, r)) {
            self.copies
                .borrow_mut()
                .extend(found.into_iter().map(|(key, copy)| (key, Some(copy))));
        }
        let mut states: Vec<State> = self
            .targets
            .iter()
            .zip(probes)
            .map(|(t, probe)| self.init(&original, t, probe))
            .collect::<Result<_>>()?;

        for round in 1..=self.max_rounds {
            let active: Vec<usize> = (0..states.len())
                .filter(|&i| states[i].status == Status::Active)
                .collect();
            // Once this machine's own target failed, the run gives up whatever
            // the others do: building them further would be thrown away.
            let given_up = states.iter().any(|st| {
                matches!(st.status, Status::Failed(_)) && Some(&st.target) == self.guarded.as_ref()
            });
            if active.is_empty() || given_up {
                break;
            }
            let written = self.write(&original, &states)?;
            let names: Vec<String> = active
                .iter()
                .map(|&i| states[i].target.to_string())
                .collect();
            let pins: usize = active
                .iter()
                .map(|&i| states[i].entry.packages().len())
                .sum();
            log::info(&format!(
                "round {round}: evaluating and building {}{}",
                names.join(", "),
                if pins == 0 {
                    String::new()
                } else {
                    format!(" with {}", plural(pins, "pinned package"))
                }
            ));
            self.round(&mut states, &active, round == self.max_rounds)?;
            if active.iter().any(|&i| states[i].status == Status::Done) {
                let source = self.nix.source_path().ok();
                for &i in &active {
                    if states[i].status == Status::Done {
                        states[i].built_with = Some((written.clone(), source.clone()));
                    }
                }
            }
            if active
                .iter()
                .any(|&i| matches!(states[i].status, Status::Failed(_)))
            {
                log::flush_nix_errors();
            } else {
                log::discard_nix_errors();
            }
        }

        for st in &mut states {
            if st.status == Status::Active {
                st.status =
                    Status::Failed(format!("still failing after {} rounds", self.max_rounds));
            }
        }
        // A nix command stopped by --timeout says nothing about any target.
        crate::nix::check_stopped()?;
        if !self.may_roll_back {
            for st in &states {
                if matches!(st.status, Status::Failed(_)) && st.unpinnable.borrow().is_some() {
                    log::detail(&format!(
                        "{}: no flake input is rolled back for this in this run",
                        st.target
                    ));
                }
            }
        } else {
            let rollback = self.rollbacks(&original, &states)?;
            if !rollback.is_empty() {
                return Ok(Report {
                    rollback,
                    // What the rollback must not break.
                    built: states
                        .iter()
                        .filter(|st| st.status == Status::Done)
                        .map(|st| (st.target.clone(), st.drv.clone().unwrap_or_default()))
                        .collect(),
                    ..Report::default()
                });
            }
        }
        self.finish(original, states)
    }

    /// The flake inputs behind failures no pin can help with: for each failure
    /// that still reproduces, the first input that changed since the target
    /// last built and whose earlier revision gets past it. Tested instead of
    /// guessed from names, since several inputs can provide a package of the
    /// same name. One per failure: the run starts again after this, and any
    /// other failure is found then.
    fn rollbacks(&self, original: &PinsFile, states: &[State]) -> Result<Vec<Rollback>> {
        let mut excluded: BTreeSet<String> = self.donors.iter().cloned().collect();
        excluded.extend(self.package_sets.iter().cloned());
        excluded.extend(states.iter().flat_map(|st| st.inputs.iter().cloned()));
        // What other inputs follow is a base they all share, such as nixpkgs.
        excluded.extend(self.new_lock.followed());
        let mut found: BTreeMap<String, Rollback> = BTreeMap::new();
        // The input found for a failed drv or evaluation error: the answer is
        // the same for every target that has it, and each test evaluates a
        // whole system. A wrong one for some target fails the run after it.
        let mut tested: HashMap<String, Option<String>> = HashMap::new();
        for st in states {
            if !matches!(st.status, Status::Failed(_)) {
                continue;
            }
            let Some(cause) = st.unpinnable.borrow().clone() else {
                continue;
            };
            // A held target's last good lock is the one it is held on: the
            // published lock may have moved on to the broken revision.
            let good = st.held_lock.as_ref().unwrap_or(&st.old_lock);
            let mut candidates =
                inputs::candidates(good, &self.new_lock, &excluded, &self.input_deny);
            // A retried pin knows where it last built, whatever the old lock says.
            let retrying = |name: &str| original.inputs.get(name).filter(|p| p.retrying);
            for (name, pin) in &original.inputs {
                if pin.retrying
                    && !candidates.contains(name)
                    && !excluded.contains(name)
                    && !inputs::denied(name, &self.input_deny)
                    && self.new_lock.is_direct(name)
                    && good.original(name) == self.new_lock.original(name)
                    && self.new_lock.nar_hash(name).as_deref() != pin.good_nar_hash()
                {
                    candidates.push(name.clone());
                }
            }
            let revision = |name: &str| {
                retrying(name)
                    .map(|p| p.locked.clone())
                    .or_else(|| good.locked(name))
            };
            let key = cause.failed_drv.as_ref().unwrap_or(&cause.error);
            let culprit = match tested.get(key) {
                Some(c) => c.clone().filter(|i| candidates.contains(i)),
                None => {
                    let mut culprit = None;
                    let baseline = if candidates.is_empty() {
                        None
                    } else {
                        self.reproduces(st, &cause)?
                    };
                    if let Some(baseline) = baseline {
                        for input in &self.suspects_first(st, &cause, &candidates) {
                            let Some(locked) = revision(input) else {
                                continue;
                            };
                            if self.fixed_by(st, &cause, &baseline, input, &locked)? {
                                culprit = Some(input.clone());
                                break;
                            }
                        }
                    }
                    // An evaluation that now passes may only have needed a
                    // fetch to work again; it must still fail without the input.
                    if culprit.is_some()
                        && cause.failed_drv.is_none()
                        && matches!(self.nix.drv_path_with(&st.target.output(), None), Ok(Ok(_)))
                    {
                        log::info(&format!(
                            "{}: the failure went away by itself, so no flake input is blamed for it",
                            st.target
                        ));
                        culprit = None;
                    }
                    tested.insert(key.clone(), culprit.clone());
                    culprit
                }
            };
            let Some(input) = culprit else {
                continue;
            };
            let (Some(locked), Some(failed)) = (revision(&input), self.new_lock.nar_hash(&input))
            else {
                continue;
            };
            let rollback = found.entry(input.clone()).or_insert_with(|| Rollback {
                input: input.clone(),
                locked: locked.clone(),
                failed_nar_hash: failed,
                reason: if cause.failed_drv.is_some() {
                    Reason::Build
                } else {
                    Reason::Eval
                },
                failed_drv: cause.failed_drv.clone(),
                error: cause.failed_drv.is_none().then(|| cause.error.clone()),
                targets: Vec::new(),
            });
            // Back far enough for every target it failed on.
            if let (Some(new), Some(old)) = (
                locked["lastModified"].as_i64(),
                rollback.locked["lastModified"].as_i64(),
            ) && new < old
            {
                rollback.locked = locked;
            }
            rollback.targets.push(st.target.to_string());
        }
        Ok(found.into_values().collect())
    }

    /// The candidate inputs in the order to test them: those whose source the
    /// failure visibly involves first, since each test evaluates a whole
    /// system. Only an order: the test still has to prove it, and a source that
    /// was filtered on its way into the build just leaves the order as it was.
    fn suspects_first(&self, st: &State, cause: &Unpinnable, candidates: &[String]) -> Vec<String> {
        if candidates.len() < 2 {
            return candidates.to_vec();
        }
        let locked: Option<Vec<Value>> =
            candidates.iter().map(|i| self.new_lock.locked(i)).collect();
        let Some(locked) = locked else {
            return candidates.to_vec();
        };
        let Ok(paths) = crate::nix::source_paths(&locked) else {
            return candidates.to_vec();
        };
        let evidence = match &cause.failed_drv {
            Some(drv) => self.nix.show_drvs(std::slice::from_ref(drv)).map(|graph| {
                graph
                    .nodes
                    .into_values()
                    .flat_map(|n| n.srcs)
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
            None => self
                .nix
                .eval_trace(&format!("{}.drvPath", st.target.output())),
        };
        let Ok(evidence) = evidence else {
            return candidates.to_vec();
        };
        let marks: Vec<Vec<String>> = locked
            .iter()
            .zip(&paths)
            .map(|(l, path)| source_marks(l, path))
            .collect();
        evidence_first(candidates, &marks, |mark| evidence.contains(mark))
    }

    /// Whether the failure is still there without changing anything, built or
    /// evaluated again, since a flaky test or a failed fetch would otherwise
    /// look fixed by the first input tried. Returns the target's closure as it
    /// is, for telling what an input changes; empty when it does not evaluate.
    fn reproduces(&self, st: &State, cause: &Unpinnable) -> Result<Option<HashSet<String>>> {
        let result = (|| -> Result<Option<HashSet<String>>> {
            match (
                self.nix.drv_path_with(&st.target.output(), None)?,
                &cause.failed_drv,
            ) {
                (Ok(drv), Some(failed)) => {
                    let closure = crate::nix::drv_closure(&drv)?;
                    let still = closure.contains(failed)
                        && !self.nix.builds(std::slice::from_ref(failed))?;
                    Ok(still.then_some(closure))
                }
                (Ok(_), None) | (Err(_), Some(_)) => Ok(None),
                (Err(_), None) => Ok(Some(HashSet::new())),
            }
        })();
        let reproduced = match result {
            Ok(r) => r,
            Err(e) if guard::timed_out() => return Err(e),
            Err(e) => {
                log::info(&format!("testing a flake input failed: {e:#}"));
                None
            }
        };
        if reproduced.is_none() {
            log::info(&format!(
                "{}: the failure did not happen again, so no flake input is blamed for it",
                st.target
            ));
        }
        Ok(reproduced)
    }

    /// A test that could not run says nothing against the input, so the target
    /// is held; only running out of time stops the run.
    fn or_false(&self, result: Result<bool>) -> Result<bool> {
        match result {
            Err(e) if guard::timed_out() => Err(e),
            Err(e) => {
                log::info(&format!("testing a flake input failed: {e:#}"));
                Ok(false)
            }
            ok => ok,
        }
    }

    /// Whether the target gets past its failure with `input` at `locked`. A
    /// build failure counts as fixed only if it actually builds: nearly any
    /// input can change the hash of a drv like the system's own, so a drv that
    /// merely changed proves nothing.
    fn fixed_by(
        &self,
        st: &State,
        cause: &Unpinnable,
        baseline: &HashSet<String>,
        input: &str,
        locked: &Value,
    ) -> Result<bool> {
        log::info(&format!(
            "{}: testing whether {input} at {} helps",
            st.target,
            describe(locked)
        ));
        let result = (|| -> Result<bool> {
            let flake_ref = crate::nix::flake_ref(locked)?;
            let drv = match self
                .nix
                .drv_path_with(&st.target.output(), Some((input, &flake_ref)))?
            {
                Ok(drv) => drv,
                Err(_) => return Ok(false),
            };
            let Some(failed) = &cause.failed_drv else {
                return Ok(true);
            };
            let closure = crate::nix::drv_closure(&drv)?;
            if closure.contains(failed) {
                return Ok(false);
            }
            let counterparts = counterparts(failed, &closure, baseline);
            if counterparts.len() > MAX_COUNTERPARTS {
                log::info(&format!(
                    "{}: {input} changes {} derivations named like the failed one; not testing it",
                    st.target,
                    counterparts.len()
                ));
                return Ok(false);
            }
            // With none, the failed drv is simply gone: the run after a
            // rollback builds the system anyway.
            if counterparts.is_empty() {
                return Ok(true);
            }
            self.nix.builds(&counterparts)
        })();
        self.or_false(result)
    }

    /// Every configured target must exist in the flake: a typo in hosts or
    /// homes would otherwise be held as a configuration that does not
    /// evaluate, and recorded in pins.json.
    fn check_targets(&self) -> Result<()> {
        for (kind, output, option, known) in [
            (
                Kind::Nixos,
                "nixosConfigurations",
                "hosts",
                &self.host_names,
            ),
            (Kind::Home, "homeConfigurations", "homes", &self.home_names),
        ] {
            let Some(names) = known else {
                continue;
            };
            let wanted: Vec<&Target> = self.targets.iter().filter(|t| t.kind == kind).collect();
            if wanted.is_empty() {
                continue;
            }
            if names.is_empty() {
                bail!(
                    "programs.eppur.{option} lists {}, but the flake has no {output}",
                    wanted
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            let missing: Vec<&str> = wanted
                .iter()
                .filter(|t| !names.contains(&t.name))
                .map(|t| t.name.as_str())
                .collect();
            if !missing.is_empty() {
                bail!(
                    "programs.eppur.{option} lists {}, which {output} does not have (it has: {})",
                    missing.join(", "),
                    names.join(", ")
                );
            }
        }
        Ok(())
    }

    /// What the startup checks of `entry`'s pins will ask `copy` for, so they
    /// can all be evaluated at once: whether a moved pin's package changed, the
    /// donors of a leaf pin, and a version not recorded yet.
    fn copy_requests(&self, entry: Option<&Entry>) -> Vec<CopyRequest> {
        let mut requests: Vec<CopyRequest> = Vec::new();
        let Some(entry) = entry else {
            return requests;
        };
        for (input, attr, pin) in entry.iter() {
            if pin.reason == Reason::Manual {
                continue;
            }
            let overlays = pin.overlays.unwrap_or(false);
            let mut want = |locked: Value, overlays: bool| {
                requests.push(CopyRequest {
                    attr: attr.to_string(),
                    locked,
                    overlays,
                })
            };
            if pin.reason == Reason::Build
                && pin.root_drv.is_some()
                && self.retry_same_after_days > 0
                && !pin.checked_nar_hash.is_empty()
                && self.new_lock.nar_hash(input).as_deref() != Some(pin.checked_nar_hash.as_str())
                && let Some(new) = self.new_lock.locked(input)
            {
                want(new, true);
            }
            if pin.version.is_none() {
                want(pin.locked.clone(), overlays);
            }
            if pin.reason == Reason::Build && pin.leaf && !self.donors.is_empty() {
                want(pin.locked.clone(), overlays);
                for donor in &self.donors {
                    if let Some(locked) = self.new_lock.locked(donor) {
                        want(locked, overlays);
                    }
                }
            }
        }
        requests
    }

    fn init(&self, original: &PinsFile, target: &Target, probe: Result<Value>) -> Result<State> {
        let base = original.entry(target.kind, &target.name).cloned();
        let mut entry = base.clone().unwrap_or_default();
        let held_before = entry.held.take();
        for (input, attr, pin) in entry.iter().filter(|(_, _, p)| p.insecure) {
            log::warn(&format!(
                "{target}: {attr} ({input}) is kept at {}, which nixpkgs now marks insecure",
                describe_dated(&pin.locked)
            ));
        }
        let held_lock = held_before
            .as_ref()
            .map(|h| Lock::from_json(&h.lock))
            .transpose()?;

        // A pin is retried when its input moved; the other pins from the same
        // failure go with it, or the package would build only because a
        // sibling still pins it, and look fixed when it is not.
        let mut retrying = BTreeMap::new();
        let system = std::cell::OnceCell::new();
        let moved: Vec<(String, String)> = entry
            .iter()
            .filter(|(i, _, p)| {
                p.reason != Reason::Manual
                    && self.new_lock.nar_hash(i).as_deref() != Some(p.checked_nar_hash.as_str())
            })
            .map(|(i, a, _)| (i.to_string(), a.to_string()))
            .collect();
        for (input, attr) in moved {
            let group = entry.same_failure(&input, &attr);
            if group.is_empty() {
                continue;
            }
            let asked = entry
                .get(&input, &attr)
                .is_some_and(|p| p.checked_nar_hash.is_empty());
            if !asked && let Some(until) = self.unchanged(target, &system, &entry, &group, &attr) {
                for i in &group {
                    if let (Some(checked), Some(pin)) = (
                        self.new_lock.nar_hash(i),
                        entry.pins.get_mut(i).and_then(|s| s.get_mut(&attr)),
                    ) {
                        pin.checked_nar_hash = checked;
                    }
                }
                log::info(&format!(
                    "{target}: {attr} ({}): same failed derivation, not rebuilt until {until}",
                    group.join(", ")
                ));
                continue;
            }
            log::info(&format!("retrying {target}: {attr} ({})", group.join(", ")));
            for i in group {
                if let Some(pin) = entry.remove(&i, &attr) {
                    retrying.insert((i, attr.clone()), pin);
                }
            }
        }
        entry.lock_sha256 = self.new_lock.sha256();
        entry.lock_last_modified = self.new_lock.last_modified();
        entry.checked_last_modified = self.new_lock.last_modified();

        let unpinnable = std::cell::RefCell::new(None);
        let (inputs, status) = match self.probe(target, probe)? {
            Ok(sets) => (sets, Status::Active),
            Err(why) => {
                unpinnable.replace(Some(Unpinnable {
                    failed_drv: None,
                    error: why.clone(),
                }));
                (Vec::new(), Status::Failed(why))
            }
        };
        let old_lock = self.last_good.get(target).unwrap_or(&self.old_lock).clone();

        let mut st = State {
            target: target.clone(),
            base,
            entry,
            held_before,
            held_lock,
            good_pins: self.last_good_pins.get(target).cloned(),
            old_lock,
            retrying,
            upgrading: BTreeMap::new(),
            inputs,
            drv: None,
            built_with: None,
            system,
            status,
            new_pins: 0,
            unpinnable,
        };
        if st.status == Status::Active {
            self.upgrade(&mut st);
            let unversioned: Vec<(String, String, Pin)> = st
                .entry
                .iter()
                .filter(|(_, _, p)| p.version.is_none())
                .map(|(i, a, p)| (i.to_string(), a.to_string(), p.clone()))
                .collect();
            for (input, attr, pin) in unversioned {
                let pin = self.versioned(&st, &attr, pin);
                st.entry.insert(&input, &attr, pin);
            }
        }
        Ok(st)
    }

    /// For a group of pins from one build failure whose inputs moved: the day
    /// from which it is retried anyway, when the package's drv is still the
    /// one that failed in every moved input. None when it should be retried
    /// now, or eppur cannot tell.
    fn unchanged(
        &self,
        target: &Target,
        system: &std::cell::OnceCell<String>,
        entry: &Entry,
        group: &[String],
        attr: &str,
    ) -> Option<String> {
        if self.retry_same_after_days <= 0 {
            return None;
        }
        let mut last = i64::MIN;
        for input in group {
            let pin = entry.get(input, attr)?;
            if pin.reason != Reason::Build {
                return None;
            }
            let root = pin.root_drv.as_deref()?;
            let tried = date::parse(pin.tried.as_deref().unwrap_or(&pin.since))?;
            last = last.max(tried);
            if self.new_lock.nar_hash(input).as_deref() == Some(pin.checked_nar_hash.as_str()) {
                continue;
            }
            let locked = self.new_lock.locked(input)?;
            let copy = self.copy(target, system, attr, &locked, true)?;
            if copy.drv != root {
                return None;
            }
        }
        let from = last + self.retry_same_after_days;
        (from > self.today).then(|| date::format(from))
    }

    /// `attr` as the nixpkgs at `locked` builds it for `target`: through the
    /// target's own importPinned where it has one, with its overlays when
    /// `overlays` (falling back to none when they do not evaluate there), else
    /// imported plainly for its system. None when it is not a derivation there.
    fn copy(
        &self,
        target: &Target,
        system: &std::cell::OnceCell<String>,
        attr: &str,
        locked: &serde_json::Value,
        overlays: bool,
    ) -> Option<Copy> {
        let key = copy_key(target, attr, locked, overlays);
        if let Some(hit) = self.copies.borrow().get(&key) {
            return hit.clone();
        }
        let literal = crate::nix::nix_json(locked).ok()?;
        if !resolve::is_attr_name(attr) {
            return None;
        }
        let via = |with: bool| {
            self.nix
                .eval_apply(
                    &target.eppur_config(),
                    &format!(
                        "c: if !(c ? importPinned) then {{ none = true; }} else let p = c.importPinned {{ locked = {literal}; overlays = {with}; }}; in {}",
                        resolve::copy_expr(attr)
                    ),
                )
                .ok()
        };
        let value = match via(overlays) {
            Some(v) if v.get("none").is_some() => {
                let system = system.get_or_init(|| {
                    self.nix
                        .eval_raw(&target.system_attr())
                        .unwrap_or_else(|_| self.system.clone())
                });
                resolve::copy_at(locked, system, attr)
                    .ok()
                    .flatten()
                    .map(|v| (v, None))
            }
            Some(v) if !v.is_null() => Some((v, Some(overlays))),
            _ if overlays => via(false)
                .filter(|v| !v.is_null() && v.get("none").is_none())
                .map(|v| (v, Some(false))),
            _ => None,
        };
        let copy = value.and_then(|(v, overlays)| copy_from(&v, overlays));
        self.copies.borrow_mut().insert(key, copy.clone());
        copy
    }

    /// The donor with the highest version of `attr` that is newer than the
    /// copy at `locked` and already built or in a substituter, as (donor,
    /// its locked input, its copy, the version it replaces). Never the drv
    /// that failed.
    fn donor(
        &self,
        st: &State,
        attr: &str,
        locked: &serde_json::Value,
        overlays: bool,
        failed: Option<&str>,
    ) -> Option<(String, serde_json::Value, Copy, String)> {
        if self.donors.is_empty() {
            return None;
        }
        let current = self.copy(&st.target, &st.system, attr, locked, overlays)?;
        if current.version.is_empty() {
            return None;
        }
        let mut best: Option<(String, serde_json::Value, Copy)> = None;
        for donor in &self.donors {
            let Some(donor_locked) = self.new_lock.locked(donor) else {
                continue;
            };
            if donor_locked["narHash"] == locked["narHash"] {
                continue;
            }
            let Some(copy) = self.copy(&st.target, &st.system, attr, &donor_locked, overlays)
            else {
                continue;
            };
            let newer = resolve::compare_versions(&copy.version, &current.version)
                == std::cmp::Ordering::Greater;
            let better = best.as_ref().is_none_or(|(_, _, b)| {
                resolve::compare_versions(&copy.version, &b.version) == std::cmp::Ordering::Greater
            });
            if newer
                && better
                && Some(copy.drv.as_str()) != failed
                && crate::nix::available(&copy.out)
            {
                best = Some((donor.clone(), donor_locked, copy));
            }
        }
        best.map(|(d, l, c)| (d, l, c, current.version))
    }

    /// `pin` with the version of the package it provides filled in, when
    /// eppur can evaluate it.
    fn versioned(&self, st: &State, attr: &str, mut pin: Pin) -> Pin {
        if pin.version.is_none() {
            pin.version = self
                .copy(
                    &st.target,
                    &st.system,
                    attr,
                    &pin.locked,
                    pin.overlays.unwrap_or(false),
                )
                .map(|c| c.version)
                .filter(|v| !v.is_empty());
        }
        pin
    }

    /// Move the target's build pins that a donor has a newer, already built
    /// version of to that donor.
    fn upgrade(&self, st: &mut State) {
        let candidates: Vec<(String, String, Pin)> = st
            .entry
            .iter()
            .filter(|(_, _, p)| p.reason == Reason::Build && p.leaf)
            .map(|(i, a, p)| (i.to_string(), a.to_string(), p.clone()))
            .collect();
        for (input, attr, pin) in candidates {
            let pin = self.with_donor(st, &input, &attr, pin, true);
            st.entry.insert(&input, &attr, pin);
        }
    }

    /// Check that the target applies the pins eppur writes for it, and return
    /// its package sets. An eppur setting that would make every pin silently
    /// do nothing stops the run. A configuration that does not evaluate at all,
    /// such as one setting an option the update removed, is that target's
    /// problem: it comes back as the reason to hold it.
    fn probe(&self, target: &Target, value: Result<Value>) -> Result<Result<Vec<String>, String>> {
        let probe: Probe = match value {
            Ok(v) => serde_json::from_value(v)?,
            Err(e) => {
                let text = e.to_string();
                if text.contains("attribute 'eppur' missing")
                    || text.contains("attribute 'programs' missing")
                {
                    bail!("{target} has no programs.eppur; import the eppur module there");
                }
                return Ok(Err(format!(
                    "its configuration does not evaluate: {}",
                    logparse::headline(&text)
                )));
            }
        };
        if !probe.enable {
            bail!("programs.eppur.enable is off in {target}, so its pins would not be applied");
        }
        if probe.name != target.name {
            bail!(
                "{target} has programs.eppur.name = \"{}\"; it must be \"{}\", its configuration's name, or its pins are never applied",
                probe.name,
                target.name
            );
        }
        match probe.pins.as_deref().map(flake_relative) {
            None => bail!("programs.eppur.pinsFile is not set in {target}"),
            Some(Some(rel)) if rel == self.pins_rel => {}
            Some(rel) => bail!(
                "{target} has programs.eppur.pinsFile = {}; it must be {}, the pins.json next to flake.lock, where eppur writes it",
                rel.unwrap_or_else(|| probe.pins.clone().unwrap_or_default()),
                self.pins_rel
            ),
        }
        for set in &probe.sets {
            if self.new_lock.locked(set).is_none() {
                log::warn(&format!(
                    "{target}: package set {set} is not an input of flake.lock, so its packages cannot be pinned"
                ));
            }
        }
        Ok(Ok(probe.sets))
    }

    /// The pins file as the builds should see it: working entries for active
    /// and finished targets, the original entry for failed ones. Returns what
    /// was written.
    fn write(&self, original: &PinsFile, states: &[State]) -> Result<Vec<u8>> {
        let mut file = original.clone();
        for st in states {
            match (&st.status, &st.base) {
                (Status::Failed(_), Some(e)) => {
                    file.set(st.target.kind, &st.target.name, e.clone())
                }
                (Status::Failed(_), None) => {}
                _ => file.set(st.target.kind, &st.target.name, st.entry.clone()),
            }
        }
        guard::critical(|| file.save(&self.pins_path))?;
        Ok(file.to_bytes())
    }

    fn round(&self, states: &mut [State], active: &[usize], last_round: bool) -> Result<()> {
        // Evaluate each host once and build what evaluated, in one nom view.
        // The drvs are kept, so a failure is traced without evaluating again.
        let mut monitor = Monitor::start();
        let mut built: Vec<(usize, String)> = Vec::new();
        let mut eval_failed: Vec<(usize, Vec<String>)> = Vec::new();
        let outputs: Vec<String> = active.iter().map(|&i| states[i].target.output()).collect();
        for (&i, evaluated) in active
            .iter()
            .zip(self.nix.drv_paths(&outputs, &mut monitor)?)
        {
            match evaluated {
                Ok(drv) => built.push((i, drv)),
                Err(msgs) => eval_failed.push((i, msgs)),
            }
        }
        let drvs: Vec<String> = built.iter().map(|(_, d)| d.clone()).collect();
        let mut outcome = if drvs.is_empty() {
            None
        } else {
            Some(self.nix.build_drvs(&drvs, &mut monitor)?)
        };
        monitor.finish();
        let graph = match &outcome {
            Some(o) if !o.success => Some(self.nix.show_drvs(&drvs)?),
            _ => None,
        };

        // A download or a builder killed by a signal may have failed for the
        // moment only (the network, memory); one retry tells that apart from
        // a real breakage before anything is pinned or the run stops. Only
        // those are retried: a real failure next to them would fail again, and
        // may take long to. What the retry says about them replaces what the
        // first build said. When nothing is left failing, or in the last round,
        // where nothing can be left for a next one, the whole build runs again.
        let mut retried_drvs: HashSet<String> = HashSet::new();
        let mut retry_errors: Vec<String> = Vec::new();
        if let (Some(first), Some(graph)) = (&outcome, &graph) {
            let transient = transient_drvs(&first.errors, graph);
            if !transient.is_empty() {
                log::info("a download or a killed builder failed; building once more");
                let mut monitor = Monitor::start();
                let retried = self.nix.build_drvs(&transient, &mut monitor)?;
                let named = logparse::failed_drvs(&retried.errors);
                // Failed without naming a derivation: the retry says nothing.
                if retried.success || !named.is_empty() {
                    let others = logparse::parse(&first.errors).into_iter().any(|f| match f {
                        Failure::Build { drv } | Failure::Infra { drv, .. } => {
                            !transient.contains(&drv)
                        }
                        _ => false,
                    });
                    if (others || !retried.success) && !last_round {
                        retried_drvs.extend(transient);
                        retry_errors = retried.errors;
                    } else {
                        outcome = Some(self.nix.build_drvs(&drvs, &mut monitor)?);
                    }
                }
                monitor.finish();
            }
        }

        if let Some(outcome) = outcome {
            if outcome.success {
                if graph.is_some() {
                    log::discard_nix_errors();
                }
                for (i, drv) in &built {
                    states[*i].status = Status::Done;
                    states[*i].drv = Some(drv.clone());
                }
            } else {
                let superseded = |f: &Failure| match f {
                    Failure::Build { drv } | Failure::Infra { drv, .. } => {
                        retried_drvs.contains(drv)
                    }
                    _ => false,
                };
                let parsed: Vec<Failure> = logparse::parse(&outcome.errors)
                    .into_iter()
                    .filter(|f| !superseded(f))
                    .chain(logparse::parse(&retry_errors))
                    .collect();
                let infra: Vec<(&String, &String)> = parsed
                    .iter()
                    .filter_map(|f| match f {
                        Failure::Infra { drv, reason } => Some((drv, reason)),
                        _ => None,
                    })
                    .collect();
                let graph = graph.expect("shown for a failed build");
                let failed: Vec<String> = parsed
                    .iter()
                    .filter_map(|f| match f {
                        Failure::Build { drv } => Some(drv.clone()),
                        _ => None,
                    })
                    .collect();
                let mut not_built = logparse::failed_drvs(&outcome.errors);
                not_built.extend(logparse::failed_drvs(&retry_errors));
                let again = logparse::failed_drvs(&retry_errors);
                let recovered: Vec<&String> = retried_drvs
                    .iter()
                    .filter(|d| !again.contains(*d))
                    .collect();
                let why = outcome
                    .errors
                    .last()
                    .map(|m| logparse::headline(m))
                    .unwrap_or_else(|| "no error message".into());
                // nix failed without naming a derivation (the daemon went away,
                // the store is full): nothing can be said about any host, so
                // none is held for it.
                if not_built.is_empty() {
                    bail!(
                        "the build failed without naming a derivation, so eppur cannot tell which host it affects; nothing was pinned or held: {why}"
                    );
                }
                for (i, top) in &built {
                    let closure = graph.closure(top);
                    let mine: Vec<&String> =
                        failed.iter().filter(|d| closure.contains(*d)).collect();
                    let st = &mut states[*i];
                    let own_build_failed = closure.iter().any(|d| not_built.contains(d));
                    // A build that cannot run here (another platform, no
                    // builder, out of memory) is this machine's problem, not
                    // the package's: the host waits for it, without a pin.
                    if let Some((drv, reason)) = infra.iter().find(|(d, _)| closure.contains(*d)) {
                        st.status = Status::Failed(format!(
                            "{} could not run on this machine ({reason})",
                            graph.display_name(drv)
                        ));
                    } else if mine.is_empty() && recovered.iter().any(|d| closure.contains(*d)) {
                        // Only held up by what recovered: the next round builds it.
                    } else if mine.is_empty() && (own_build_failed || !crate::nix::is_built(top)?) {
                        st.status = Status::Failed(format!("its system did not build: {why}"));
                    } else if mine.is_empty() {
                        st.status = Status::Done;
                        st.drv = Some(top.clone());
                    } else {
                        let wanted = self.build_wanted(&graph, &closure, &mine, st)?;
                        self.apply(st, wanted);
                    }
                }
            }
        }

        for (i, msgs) in eval_failed {
            let st = &mut states[i];
            let parsed = logparse::parse(&msgs);
            if let Some((attr, reason)) = parsed.iter().find_map(|f| match f {
                Failure::Refused { attr, reason } => Some((attr, reason)),
                Failure::Insecure { attr, reason } if !self.pin_insecure => Some((attr, reason)),
                _ => None,
            }) {
                st.status = Status::Failed(format!("nixpkgs refuses {attr} because {reason}"));
                continue;
            }
            let attrs: Vec<(String, bool)> = parsed
                .into_iter()
                .filter_map(|f| match f {
                    Failure::Eval { attr } => Some((attr, false)),
                    Failure::Insecure { attr, .. } => Some((attr, true)),
                    _ => None,
                })
                .collect();
            if attrs.is_empty() {
                let why = msgs
                    .first()
                    .map_or_else(String::new, |m| logparse::headline(m));
                st.unpinnable.replace(Some(Unpinnable {
                    failed_drv: None,
                    error: why.clone(),
                }));
                st.status = Status::Failed(format!("evaluation fails: {why}"));
                continue;
            }
            let error = msgs.first().map(|m| logparse::headline(m));
            let mut context: Option<Vec<String>> = None;
            let mut wanted = Vec::new();
            for (attr, insecure) in attrs {
                let inputs = self.eval_input(st, &attr)?;
                let mut found = (!inputs.is_empty()).then(|| (inputs, attr.clone()));
                if found.is_none() {
                    let context = match &context {
                        Some(c) => c,
                        None => {
                            let trace = self
                                .nix
                                .eval_trace(&format!("{}.drvPath", st.target.output()))?;
                            let full = logparse::eval_context(&[trace]);
                            let frames = if full.is_empty() {
                                logparse::eval_context(&msgs)
                            } else {
                                full
                            };
                            self.check_lost(st, &frames);
                            context.insert(frames)
                        }
                    };
                    for outer in context {
                        let inputs = self.eval_input(st, outer)?;
                        if !inputs.is_empty() {
                            log::detail(&format!(
                                "{}: {attr} is not pinnable itself; {outer}, which evaluation fails inside, is",
                                st.target
                            ));
                            found = Some((inputs, outer.clone()));
                            break;
                        }
                    }
                }
                match found {
                    Some((inputs, outer)) => wanted.push(Wanted {
                        members: inputs.into_iter().map(|i| (i, outer.clone())).collect(),
                        reason: Reason::Eval,
                        failed_drv: None,
                        root_drv: None,
                        leaf: false,
                        cause: if insecure {
                            format!("nixpkgs now marks {attr} as insecure")
                        } else {
                            format!("evaluation failed: {}", error.clone().unwrap_or_default())
                        },
                        trail: Vec::new(),
                        error: error.clone(),
                        insecure,
                    }),
                    None if insecure => {
                        st.status = Status::Failed(format!(
                            "nixpkgs refuses {attr} because it is marked as insecure, and no older nixpkgs that built has it"
                        ));
                        break;
                    }
                    None => {
                        st.unpinnable.replace(Some(Unpinnable {
                            failed_drv: None,
                            error: error.clone().unwrap_or_default(),
                        }));
                        st.status = Status::Failed(format!(
                            "evaluation fails on {attr:?}, which no older nixpkgs that built has"
                        ));
                        break;
                    }
                }
            }
            if st.status == Status::Active {
                self.apply(st, Ok(wanted));
            }
        }
        Ok(())
    }

    fn build_wanted(
        &self,
        graph: &Graph,
        closure: &HashSet<String>,
        failed: &[&String],
        st: &State,
    ) -> Result<Result<Vec<Wanted>, String>> {
        let mut nodes = HashSet::new();
        for drv in failed {
            match graph.ancestors(drv, closure, ASCENT_CAP) {
                Some(a) => nodes.extend(a),
                None => {
                    return Ok(Err(format!(
                        "{} is a dependency of nearly everything",
                        graph.display_name(drv)
                    )));
                }
            }
        }
        let matcher = match Matcher::build(
            &self.nix,
            graph,
            &nodes,
            &st.target.package_sets(),
            &st.inputs,
        ) {
            Ok(m) => m,
            Err(e) => return Ok(Err(format!("matching packages failed: {e:#}"))),
        };

        let mut wanted: Vec<Wanted> = Vec::new();
        for drv in failed {
            let roots = match graph.find_roots(drv, closure, ASCENT_CAP, |p| matcher.matches(p)) {
                Ascent::Roots(r) => r,
                Ascent::Unpinnable { via } => {
                    log::info(&format!(
                        "unpinnable on {}: {} reaches {} without passing a package",
                        st.target,
                        graph.display_name(drv),
                        graph.display_name(&via)
                    ));
                    st.unpinnable.replace(Some(Unpinnable {
                        failed_drv: Some(drv.to_string()),
                        error: format!("{} failed to build", graph.display_name(drv)),
                    }));
                    return Ok(Err(format!(
                        "{} is not inside any pinnable package",
                        graph.display_name(drv)
                    )));
                }
                Ascent::TooLarge => {
                    return Ok(Err(format!(
                        "{} is a dependency of nearly everything",
                        graph.display_name(drv)
                    )));
                }
            };
            for root in &roots {
                // Everything above the root rebuilds once it is swapped, the
                // packages that are not top-level attributes (python3Packages.*)
                // as much as those that are.
                let dependents =
                    graph
                        .ancestors(root, closure, ASCENT_CAP)
                        .map_or(usize::MAX, |a| {
                            a.iter()
                                .filter(|d| {
                                    *d != root && (matcher.matches(d) || graph.is_package(d))
                                })
                                .count()
                        });
                if dependents > MAX_DEPENDENTS {
                    return Ok(Err(format!(
                        "{} is used by {} other packages, too many to pin",
                        graph.display_name(root),
                        if dependents == usize::MAX {
                            "too many".to_string()
                        } else {
                            dependents.to_string()
                        }
                    )));
                }
                let members: Vec<(String, String)> = matcher
                    .attrs(root)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .filter(|m| !wanted.iter().any(|w| w.members.contains(m)))
                    .cloned()
                    .collect();
                if !members.is_empty() {
                    wanted.push(Wanted {
                        members,
                        reason: Reason::Build,
                        failed_drv: Some(drv.to_string()),
                        root_drv: Some(root.to_string()),
                        leaf: dependents == 0,
                        cause: format!("{} failed to build", graph.display_name(drv)),
                        trail: graph.trail(drv, root, closure),
                        error: None,
                        insecure: false,
                    });
                }
            }
        }
        Ok(Ok(wanted))
    }

    /// The inputs whose last good set provides `attr` while the new one does
    /// not; every one of them, since the configuration may use any. A pin
    /// under retry counts as last good: the last lock only built because of it.
    fn eval_input(&self, st: &State, attr: &str) -> Result<Vec<String>> {
        if !resolve::is_attr_name(attr) {
            return Ok(Vec::new());
        }
        let retried: Vec<String> = st
            .retrying
            .keys()
            .filter(|(_, a)| a == attr)
            .map(|(i, _)| i.clone())
            .collect();
        if !retried.is_empty() {
            return Ok(retried);
        }
        let mut found = Vec::new();
        for (input, old, new) in self.moved_sets(st) {
            let system = self.system_of(st);
            let key = lost_key(&old, &new, system, attr);
            let cached = self.lost.borrow().get(&key).copied();
            let lost = match cached {
                Some(lost) => lost,
                None => {
                    let one = [attr.to_string()];
                    let lost = resolve::lost_between(&old, &new, system, &one)?
                        .get(attr)
                        .copied()
                        .unwrap_or(false);
                    self.lost.borrow_mut().insert(key, lost);
                    lost
                }
            };
            if lost {
                found.push(input);
            }
        }
        Ok(found)
    }

    /// Answer `eval_input` for all of `attrs` in one evaluation per package
    /// set. One that fails on an attr, in a way tryEval cannot catch, answers
    /// nothing, and they are asked one at a time, as far as that gets.
    fn check_lost(&self, st: &State, attrs: &[String]) {
        let attrs: Vec<String> = attrs
            .iter()
            .filter(|a| resolve::is_attr_name(a))
            .cloned()
            .collect();
        let system = self.system_of(st);
        for (_, old, new) in self.moved_sets(st) {
            let missing: Vec<String> = attrs
                .iter()
                .filter(|a| {
                    !self
                        .lost
                        .borrow()
                        .contains_key(&lost_key(&old, &new, system, a))
                })
                .cloned()
                .collect();
            if missing.len() < 2 {
                continue;
            }
            if let Ok(answers) = resolve::lost_between(&old, &new, system, &missing) {
                let mut lost = self.lost.borrow_mut();
                for (attr, answer) in answers {
                    lost.insert(lost_key(&old, &new, system, &attr), answer);
                }
            }
        }
    }

    /// The target's package sets that moved since it last built, as (input,
    /// last good revision, new revision).
    fn moved_sets(&self, st: &State) -> Vec<(String, Value, Value)> {
        let old_lock = st.held_lock.as_ref().unwrap_or(&st.old_lock);
        st.inputs
            .iter()
            .filter_map(|input| {
                let old = old_lock.locked(input)?;
                let new = self.new_lock.locked(input)?;
                (old["narHash"] != new["narHash"]).then(|| (input.clone(), old, new))
            })
            .collect()
    }

    fn system_of<'a>(&'a self, st: &'a State) -> &'a str {
        st.system.get_or_init(|| {
            self.nix
                .eval_raw(&st.target.system_attr())
                .unwrap_or_else(|_| self.system.clone())
        })
    }

    fn apply(&self, st: &mut State, wanted: Result<Vec<Wanted>, String>) {
        let wanted = match wanted {
            Ok(w) if w.is_empty() => {
                st.status = Status::Failed("it fails, but nothing to pin was found".into());
                return;
            }
            Ok(w) => w,
            Err(why) => {
                st.status = Status::Failed(why);
                return;
            }
        };
        if let Err(why) = self.add_pins(st, wanted) {
            st.status = Status::Failed(why);
        }
    }

    /// Pin each failure in every package set that can take the pin; a set
    /// that cannot (it did not move, it is new in the lock, its pin did not
    /// help) is skipped, and only a failure no set can take fails the target.
    fn add_pins(&self, st: &mut State, wanted: Vec<Wanted>) -> Result<(), String> {
        for w in wanted {
            if let Some((input, attr)) = w
                .members
                .iter()
                .find(|(_, a)| resolve::denied(a, &self.deny))
            {
                return Err(format!("{input}.{attr} fails and is on the never-pin list"));
            }
            // (attr, input, why not) for each set that cannot take the pin.
            let mut skipped: Vec<(String, String, String)> = Vec::new();
            let mut kept: Vec<String> = Vec::new();
            let mut added: Vec<(String, String, Pin)> = Vec::new();
            for (input, attr) in &w.members {
                if let Some(previous) = st.upgrading.remove(&(input.clone(), attr.clone())) {
                    log::change(&format!(
                        "{}: {attr} ({input}) fails from {}, so it goes back to {}",
                        st.target,
                        st.entry
                            .get(input, attr)
                            .and_then(|p| p.donor.clone())
                            .unwrap_or_else(|| "its donor".into()),
                        describe_dated(&previous.locked)
                    ));
                    kept.push(format!("{input} (since {})", previous.since));
                    st.entry.insert(input, attr, previous);
                    continue;
                }
                if st.entry.get(input, attr).is_some() {
                    skipped.push((
                        attr.clone(),
                        input.clone(),
                        "it is pinned there already and still fails".into(),
                    ));
                    continue;
                }
                let Some(checked) = self.new_lock.nar_hash(input) else {
                    skipped.push((
                        attr.clone(),
                        input.clone(),
                        format!("{input} is not in the new lock"),
                    ));
                    continue;
                };
                if let Some(mut pin) = st.retrying.remove(&(input.clone(), attr.clone())) {
                    pin.checked_nar_hash = checked;
                    if w.failed_drv.is_some() || w.error.is_some() {
                        pin.failed_drv = w.failed_drv.clone();
                        pin.trail = w.trail.clone();
                        pin.error = w.error.clone();
                    }
                    pin.insecure = w.insecure;
                    if w.root_drv.is_some() {
                        pin.root_drv = w.root_drv.clone();
                        pin.leaf = w.leaf;
                        pin.tried = Some(date::format(self.today));
                    }
                    kept.push(format!("{input} (since {})", pin.since));
                    let pin = self.with_donor(st, input, attr, pin, true);
                    let pin = self.versioned(st, attr, pin);
                    st.entry.insert(input, attr, pin);
                    continue;
                }
                // Pinned when this target last built: that revision is the one
                // known to work, whatever the last good lock itself says (the
                // package may have been broken there too, and built only
                // because of this pin).
                if let Some(mut pin) = st
                    .good_pins
                    .as_ref()
                    .and_then(|e| e.get(input, attr))
                    .filter(|p| p.reason != Reason::Manual)
                    .cloned()
                {
                    pin.checked_nar_hash = checked;
                    pin.failed_drv = w.failed_drv.clone();
                    pin.trail = w.trail.clone();
                    pin.error = w.error.clone();
                    pin.root_drv = w.root_drv.clone();
                    pin.leaf = w.leaf;
                    pin.tried = None;
                    pin.insecure = w.insecure;
                    let pin = self.with_donor(st, input, attr, pin, false);
                    let pin = self.versioned(st, attr, pin);
                    added.push((input.clone(), attr.clone(), pin));
                    continue;
                }
                let old_lock = st.held_lock.as_ref().unwrap_or(&st.old_lock);
                let Some(locked) = old_lock.locked(input) else {
                    skipped.push((
                        attr.clone(),
                        input.clone(),
                        format!("{input} is not in the lock this host last built with"),
                    ));
                    continue;
                };
                if locked["narHash"].as_str() == Some(checked.as_str()) {
                    skipped.push((
                        attr.clone(),
                        input.clone(),
                        format!("{input} is still at the revision this host last built with"),
                    ));
                    continue;
                }
                let overlays = choose_overlays(&self.nix, &st.target, input, attr, &locked);
                if w.insecure
                    && !{
                        let system = self.system_of(st).to_string();
                        pinned_copy_built(&self.nix, &st.target, &system, attr, &locked, overlays)
                    }
                {
                    skipped.push((
                        attr.clone(),
                        input.clone(),
                        format!(
                            "nixpkgs marks it insecure, and this machine has not built its copy at {}, so pinning it would install it rather than keep it",
                            describe_dated(&locked)
                        ),
                    ));
                    continue;
                }
                let pin = Pin {
                    locked,
                    since: date::format(self.today),
                    reason: w.reason,
                    failed_drv: w.failed_drv.clone(),
                    trail: w.trail.clone(),
                    error: w.error.clone(),
                    overlays,
                    checked_nar_hash: checked,
                    root_drv: w.root_drv.clone(),
                    tried: None,
                    donor: None,
                    leaf: w.leaf,
                    version: None,
                    insecure: w.insecure,
                };
                let pin = self.with_donor(st, input, attr, pin, false);
                let pin = self.versioned(st, attr, pin);
                added.push((input.clone(), attr.clone(), pin));
            }
            if added.is_empty() && kept.is_empty() {
                return Err(skipped
                    .iter()
                    .map(|(a, i, why)| format!("{a} ({i}) fails: {why}"))
                    .collect::<Vec<_>>()
                    .join("; "));
            }
            let attrs: BTreeSet<&str> = w.members.iter().map(|(_, a)| a.as_str()).collect();
            let name = attrs.into_iter().collect::<Vec<_>>().join(", ");
            if !kept.is_empty() {
                log::info(&format!(
                    "{}: {name} still fails; keeping its pin in {}",
                    st.target,
                    kept.join(", ")
                ));
            }
            if !added.is_empty() {
                st.new_pins += 1;
                if st.new_pins > self.max_new_pins {
                    return Err(format!(
                        "it needs more than {} in one run (--max-new-pins {})",
                        plural(self.max_new_pins, "new pin"),
                        self.max_new_pins
                    ));
                }
                // One revision for all sets when they agree, else each its own.
                let revs: BTreeSet<String> = added
                    .iter()
                    .map(|(_, _, p)| describe_dated(&p.locked))
                    .collect();
                let sets = if revs.len() == 1 {
                    format!(
                        "({}) at {}",
                        added
                            .iter()
                            .map(|(i, _, _)| i.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        describe_dated(&added[0].2.locked)
                    )
                } else {
                    format!(
                        "({})",
                        added
                            .iter()
                            .map(|(i, _, p)| format!("{i} at {}", describe_dated(&p.locked)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                log::change(&format!("pinned {}: {name} {sets}: {}", st.target, w.cause));
                for (input, attr, pin) in added {
                    st.entry.insert(&input, &attr, pin);
                }
            }
            for (a, i, why) in skipped {
                log::detail(&format!("{}: not pinning {a} ({i}): {why}", st.target));
            }
        }
        Ok(())
    }

    /// `pin`, or the same pin moved to a donor that has a newer, already
    /// built version of the package; the pin it replaces is kept to go back
    /// to if the donor's copy fails. `announce` logs the move as a change of
    /// its own, for a pin that is otherwise kept as it was.
    fn with_donor(&self, st: &mut State, input: &str, attr: &str, pin: Pin, announce: bool) -> Pin {
        if pin.reason != Reason::Build || !pin.leaf {
            return pin;
        }
        let Some((donor, locked, copy, was)) = self.donor(
            st,
            attr,
            &pin.locked,
            pin.overlays.unwrap_or(false),
            pin.root_drv.as_deref(),
        ) else {
            return pin;
        };
        let why = format!(
            "{donor} at {}: it has {attr} {} already built, newer than {was}",
            describe_dated(&locked),
            copy.version
        );
        if announce {
            log::change(&format!(
                "moved pin {}: {attr} ({input}) to {why}",
                st.target
            ));
        } else {
            log::info(&format!(
                "{}: {attr} ({input}) is pinned to {why}",
                st.target
            ));
        }
        let mut moved = pin.clone();
        moved.locked = locked;
        moved.donor = Some(donor);
        moved.version = Some(copy.version).filter(|v| !v.is_empty());
        if copy.overlays.is_some() {
            moved.overlays = copy.overlays;
        }
        st.upgrading
            .insert((input.to_string(), attr.to_string()), pin);
        moved
    }

    fn finish(&self, original: PinsFile, states: Vec<State>) -> Result<Report> {
        let mut report = Report {
            failed: states
                .iter()
                .filter(|st| matches!(st.status, Status::Failed(_)))
                .map(|st| st.target.clone())
                .collect(),
            ..Report::default()
        };
        let built: BTreeSet<String> = states
            .iter()
            .filter(|st| st.status == Status::Done)
            .map(|st| st.target.to_string())
            .collect();
        let local_failure = states.iter().find_map(|st| match &st.status {
            Status::Failed(reason) if Some(&st.target) == self.guarded.as_ref() => {
                Some(reason.clone())
            }
            _ => None,
        });
        if let Some(reason) = local_failure {
            log::flush_nix_errors();
            log::change(&format!(
                "giving up on {}: {reason}",
                st_name(self.guarded.as_ref())
            ));
            report.local_failure = Some(reason);
            return Ok(report);
        }
        let mut file = original;
        // A retried input built on every target it failed on that the flake
        // still has: its pin goes. So does one with no such target left. A
        // target merely missing from this run or this machine's config says
        // nothing: it has not been tried.
        let passed: Vec<String> = file
            .inputs
            .iter()
            .filter(|(_, p)| {
                let mut remaining = p
                    .targets
                    .iter()
                    .filter(|t| self.flake_targets.as_ref().is_none_or(|f| f.contains(t)))
                    .peekable();
                remaining.peek().is_none() || (p.retrying && remaining.all(|t| built.contains(t)))
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in passed {
            file.inputs.remove(&name);
            file.touch_inputs();
            log::change(&format!("unpinned input {name}: builds again"));
            report
                .changes
                .push(Change::Unpinned(format!("input {name}")));
        }
        for (name, pin) in &file.inputs {
            if let Some(days) = date::parse(&pin.since).map(|d| self.today - d)
                && days > self.warn_after_days
            {
                log::warn(&format!(
                    "flake input {name} has been held back for {days} days (warn after {})",
                    self.warn_after_days
                ));
            }
        }
        let mut done_with: Vec<(Target, Vec<u8>, Option<String>)> = Vec::new();
        for st in states {
            let target = st.target.clone();
            match st.status {
                Status::Done => {
                    if let Some((bytes, source)) = &st.built_with {
                        done_with.push((target.clone(), bytes.clone(), source.clone()));
                    }
                    if Some(&target) == self.local.as_ref() {
                        report.local_drv = st.drv.clone();
                    }
                    if let Some(drv) = &st.drv {
                        report.built.push((target.clone(), drv.clone()));
                    }
                    let mut released: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
                    for (input, attr) in st.retrying.keys() {
                        released.entry(attr).or_default().push(input);
                    }
                    for (attr, inputs) in released {
                        let still: Vec<String> = st.entry.inputs_pinning(attr);
                        if still.is_empty() {
                            log::change(&format!(
                                "unpinned {target}: {attr} ({}): builds again",
                                inputs.join(", ")
                            ));
                            report
                                .changes
                                .push(Change::Unpinned(format!("{target}: {attr}")));
                        } else {
                            log::info(&format!(
                                "{target}: {attr} no longer needs its pin in {}; still pinned in {}",
                                inputs.join(", "),
                                still.join(", ")
                            ));
                        }
                    }
                    if st.held_before.is_some() {
                        log::change(&format!("released {target}: it builds with the new lock"));
                        report.changes.push(Change::Released(target.to_string()));
                    }
                    let previous = st.base.as_ref();
                    for (attr, pins) in st.entry.packages() {
                        let new: Vec<&str> = pins
                            .iter()
                            .filter(|(i, _)| previous.and_then(|b| b.get(i, attr)).is_none())
                            .map(|(i, _)| *i)
                            .collect();
                        if !new.is_empty() {
                            report.changes.push(Change::Pinned(format!(
                                "{target}: {attr} ({})",
                                new.join(", ")
                            )));
                        }
                        self.log_age(&target, attr, pins[0].1);
                    }
                    for (input, attr) in st.upgrading.keys() {
                        if let Some(pin) = st.entry.get(input, attr)
                            && let Some(donor) = &pin.donor
                            && previous
                                .and_then(|b| b.get(input, attr))
                                .is_some_and(|b| b.locked != pin.locked)
                        {
                            report.changes.push(Change::Moved(format!(
                                "{target}: {attr} ({input}) to {donor}"
                            )));
                        }
                    }
                    report.built_pins.push((target.clone(), st.entry.clone()));
                    file.set(target.kind, &target.name, st.entry);
                    log::detail(&format!("{target} builds"));
                }
                Status::Failed(reason) => {
                    report.held += 1;
                    if Some(&target) == self.local.as_ref() {
                        report.local_held = true;
                    }
                    let dropped = st
                        .entry
                        .packages()
                        .keys()
                        .filter(|a| {
                            st.base
                                .as_ref()
                                .is_none_or(|b| b.inputs_pinning(a).is_empty())
                        })
                        .count();
                    let mut entry = st.base.unwrap_or_default();
                    // A held entry is computed from the lock it is held on,
                    // which is what consumers compare; merge ranks it by the
                    // lock it was tried against.
                    if st.held_before.is_none() {
                        entry.lock_sha256 = st.old_lock.sha256();
                        entry.lock_last_modified = st.old_lock.last_modified();
                    }
                    entry.checked_last_modified = self.new_lock.last_modified();
                    let dropped = if dropped == 0 {
                        String::new()
                    } else {
                        format!("; dropped {} from this run", plural(dropped, "new pin"))
                    };
                    let reason = reason.trim_end_matches('.').to_string();
                    let held = match st.held_before {
                        Some(mut h) => {
                            log::change(&format!(
                                "still holding {target} on its last good lock (since {}): {reason}{dropped}",
                                h.since
                            ));
                            h.reason = reason.clone();
                            h
                        }
                        None => {
                            report
                                .changes
                                .push(Change::Held(format!("{target}: {reason}")));
                            log::change(&format!(
                                "held {target} on its last good lock: {reason}{dropped}"
                            ));
                            Held {
                                since: date::format(self.today),
                                reason: reason.clone(),
                                lock: st.old_lock.json().clone(),
                            }
                        }
                    };
                    entry.held = Some(held);
                    file.set(target.kind, &target.name, entry);
                }
                Status::Active => unreachable!("active targets are failed before finishing"),
            }
        }
        guard::critical(|| file.save(&self.pins_path))?;
        self.refresh_built(&file.to_bytes(), &done_with, &mut report)?;
        Ok(report)
    }

    /// A target that built in an early round saw a pins file other rounds
    /// changed since. Its system only depends on that file when the
    /// configuration embeds the flake's source, but then what it built is not
    /// what gets committed: evaluate it again and build what changed.
    fn refresh_built(
        &self,
        final_bytes: &[u8],
        done_with: &[(Target, Vec<u8>, Option<String>)],
        report: &mut Report,
    ) -> Result<()> {
        let mut stale: Vec<&Target> = Vec::new();
        for (target, bytes, source) in done_with {
            if bytes.as_slice() == final_bytes {
                continue;
            }
            let Some(drv) = report
                .built
                .iter()
                .find(|(t, _)| t == target)
                .map(|(_, d)| d)
            else {
                continue;
            };
            // Only a system that takes the flake's source tree as a source can
            // depend on what other targets' entries say.
            let embeds = match source {
                Some(src) => self
                    .nix
                    .show_drvs(std::slice::from_ref(drv))?
                    .nodes
                    .values()
                    .any(|n| n.srcs.iter().any(|p| p == src || p.ends_with("-pins.json"))),
                None => true,
            };
            if embeds {
                stale.push(target);
            }
        }
        if stale.is_empty() {
            return Ok(());
        }
        let mut monitor = Monitor::start();
        let mut rebuild = Vec::new();
        let outputs: Vec<String> = stale.iter().map(|t| t.output()).collect();
        for (target, evaluated) in stale
            .into_iter()
            .zip(self.nix.drv_paths(&outputs, &mut monitor)?)
        {
            let drv = match evaluated {
                Ok(d) => d,
                Err(msgs) => bail!(
                    "{target} built, but no longer evaluates with the final pins.json: {}",
                    msgs.first()
                        .map(|m| logparse::headline(m))
                        .unwrap_or_default()
                ),
            };
            if let Some(slot) = report.built.iter_mut().find(|(t, _)| t == target)
                && slot.1 != drv
            {
                slot.1 = drv.clone();
                if Some(target) == self.local.as_ref() {
                    report.local_drv = Some(drv.clone());
                }
                rebuild.push(drv);
            }
        }
        if !rebuild.is_empty() {
            log::info(&format!(
                "building {} again with the final pins.json",
                plural(rebuild.len(), "system")
            ));
            let outcome = self.nix.build_drvs(&rebuild, &mut monitor)?;
            if !outcome.success {
                log::flush_nix_errors();
                bail!("a system that built failed to build again with the final pins.json");
            }
        }
        monitor.finish();
        Ok(())
    }

    /// Warn about a pin older than warnAfterDays; younger ones are left to
    /// eppur status.
    fn log_age(&self, target: &Target, attr: &str, pin: &Pin) {
        match date::parse(&pin.since).map(|d| self.today - d) {
            Some(days) if days > self.warn_after_days && pin.reason != Reason::Manual => {
                log::warn(&format!(
                    "{target}: {attr} has been pinned for {days} days (warn after {})",
                    self.warn_after_days
                ))
            }
            Some(_) => {}
            None => log::warn(&format!(
                "{target}: {attr} has an unreadable date {:?}",
                pin.since
            )),
        }
    }
}

/// A package to evaluate at a revision before the run starts.
#[derive(Debug, Clone)]
struct CopyRequest {
    attr: String,
    locked: Value,
    overlays: bool,
}

fn copy_key(target: &Target, attr: &str, locked: &Value, overlays: bool) -> String {
    format!(
        "{target}\0{attr}\0{}\0{overlays}",
        locked["narHash"].as_str().unwrap_or_default()
    )
}

fn copy_from(value: &Value, overlays: Option<bool>) -> Option<Copy> {
    Some(Copy {
        drv: value["drv"].as_str()?.to_string(),
        out: value["out"].as_str()?.to_string(),
        version: value["version"].as_str()?.to_string(),
        overlays,
    })
}

/// The copies `requests` ask for, keyed as `Engine::copy` finds them, from one
/// evaluation of the target that imports each revision once. Any of them can
/// fail in a way tryEval cannot catch (an overlay that breaks on an old
/// nixpkgs), which leaves nothing here: `copy` then asks one at a time. It also
/// handles whatever comes back null, and a target without importPinned.
fn fetch_copies(nix: &Nix, target: &Target, requests: &[CopyRequest]) -> Vec<(String, Copy)> {
    let mut groups: BTreeMap<(String, bool), (Value, BTreeSet<&str>)> = BTreeMap::new();
    for r in requests {
        if resolve::is_attr_name(&r.attr) {
            let hash = r.locked["narHash"].as_str().unwrap_or_default().to_string();
            groups
                .entry((hash, r.overlays))
                .or_insert_with(|| (r.locked.clone(), BTreeSet::new()))
                .1
                .insert(&r.attr);
        }
    }
    let wanted: usize = groups.values().map(|(_, attrs)| attrs.len()).sum();
    if wanted < 2 {
        return Vec::new();
    }
    let mut imports = Vec::new();
    for ((_, overlays), (locked, attrs)) in &groups {
        let Ok(literal) = crate::nix::nix_json(locked) else {
            return Vec::new();
        };
        let fields: Vec<String> = attrs
            .iter()
            .map(|a| format!("\"{a}\" = {};", resolve::copy_expr(a)))
            .collect();
        imports.push(format!(
            "(let p = c.importPinned {{ locked = {literal}; overlays = {overlays}; }}; in {{ {} }})",
            fields.join(" ")
        ));
    }
    let apply = format!(
        "c: if !(c ? importPinned) then null else [ {} ]",
        imports.join(" ")
    );
    let Ok(Value::Array(results)) = nix.eval_apply(&target.eppur_config(), &apply) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (((_, overlays), (locked, _)), result) in groups.iter().zip(&results) {
        for (attr, value) in result.as_object().into_iter().flatten() {
            if let Some(copy) = copy_from(value, Some(*overlays)) {
                found.push((copy_key(target, attr, locked, *overlays), copy));
            }
        }
    }
    found
}

fn lost_key(
    old: &Value,
    new: &Value,
    system: &str,
    attr: &str,
) -> (String, String, String, String) {
    (
        old["narHash"].as_str().unwrap_or_default().to_string(),
        new["narHash"].as_str().unwrap_or_default().to_string(),
        system.to_string(),
        attr.to_string(),
    )
}

/// What gives a locked input away in a build's sources or an evaluation trace.
/// A source appears by its store object's name, which `src = ./.` inside the
/// input turns into <hash>-<its hash>-source; nix names the files of a trace
/// by where they came from, which is a revision or a local path.
fn source_marks(locked: &Value, store_path: &str) -> Vec<String> {
    let url = locked["url"]
        .as_str()
        .and_then(|u| u.strip_prefix("file://"));
    [
        store_path.rsplit('/').next(),
        locked["rev"].as_str(),
        url,
        locked["path"].as_str(),
    ]
    .into_iter()
    .flatten()
    .filter(|mark| !mark.is_empty())
    .map(str::to_string)
    .collect()
}

/// `candidates` with those having a mark that `found` confirms first, each
/// group in its own order. `marks` runs alongside `candidates`.
fn evidence_first(
    candidates: &[String],
    marks: &[Vec<String>],
    found: impl Fn(&str) -> bool,
) -> Vec<String> {
    let (first, rest): (Vec<_>, Vec<_>) = candidates
        .iter()
        .zip(marks)
        .partition(|(_, marks)| marks.iter().any(|m| found(m)));
    first
        .into_iter()
        .chain(rest)
        .map(|(c, _)| c.clone())
        .collect()
}

/// The failed drvs that may have failed only for the moment: builders killed
/// by a signal, and downloads (fixed-output derivations) whose builder failed.
fn transient_drvs(errors: &[String], graph: &Graph) -> Vec<String> {
    let mismatch = errors.iter().any(|m| m.contains("hash mismatch"));
    let mut drvs: Vec<String> = logparse::parse(errors)
        .into_iter()
        .filter_map(|f| match f {
            Failure::Infra { drv, reason } if reason.contains("due to signal") => Some(drv),
            Failure::Build { drv }
                if !mismatch
                    && graph
                        .nodes
                        .get(drv.as_str())
                        .is_some_and(|n| n.fixed_output) =>
            {
                Some(drv)
            }
            _ => None,
        })
        .collect();
    drvs.sort();
    drvs.dedup();
    drvs
}

fn st_name(target: Option<&Target>) -> String {
    target.map_or_else(|| "the local machine".into(), Target::to_string)
}

/// The path of a store path below its store object, e.g. "hosts/a/pins.json"
/// for /nix/store/<hash>-source/hosts/a/pins.json; None for other paths.
fn flake_relative(path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let parts: Vec<&str> = rest.split('/').collect();
    let object = parts.iter().position(|p| {
        p.len() > 33
            && p.as_bytes()[32] == b'-'
            && p[..32].bytes().all(|b| b.is_ascii_alphanumeric())
    })?;
    let rel = parts[object + 1..].join("/");
    (!rel.is_empty()).then_some(rel)
}

/// Whether a new pin of `attr` from `locked` should carry the target's own
/// overlays: with programs.eppur.pinnedOverlays on, yes when the overlaid
/// copy evaluates on the old nixpkgs; a warning says when they are left out
/// and change the package. None when the target cannot tell (no
/// importPinned, e.g. a hand-written configuration).
pub fn choose_overlays(
    nix: &Nix,
    target: &Target,
    input: &str,
    attr: &str,
    locked: &serde_json::Value,
) -> Option<bool> {
    let literal = crate::nix::nix_json(locked).ok()?;
    if !resolve::is_attr_name(attr) {
        return None;
    }
    // Evaluated one way at a time: an overlay that breaks on the old nixpkgs
    // is an error tryEval cannot catch.
    let probe = |with: bool| {
        nix.eval_apply(
            &target.eppur_config(),
            &format!(
                "c: if !(c ? importPinned) then null else {{ carry = c.pinnedOverlays or true; out = (c.importPinned {{ locked = {literal}; overlays = {with}; }}).{attr}.outPath; }}"
            ),
        )
    };
    let out = |v: &serde_json::Value| v["out"].as_str().map(str::to_string);
    match probe(true) {
        Ok(serde_json::Value::Null) => None,
        Ok(with) => {
            let carry = with["carry"].as_bool().unwrap_or(true);
            let plain = probe(false).ok().and_then(|v| out(&v));
            let changes = plain.is_some() && plain != out(&with);
            if carry && changes {
                log::detail(&format!(
                    "{target}: {attr} ({input}) is pinned with your overlays, which change it"
                ));
            } else if !carry && changes {
                log::warn(&format!(
                    "{target}: {attr} ({input}) is pinned without your overlays, which change it"
                ));
            }
            Some(carry)
        }
        Err(e) => match probe(false) {
            Ok(plain) if !plain.is_null() => {
                if plain["carry"].as_bool().unwrap_or(true) {
                    log::warn(&format!(
                        "{target}: {attr} ({input}) is pinned without your overlays: with them it does not evaluate on the old nixpkgs: {}",
                        logparse::headline(&e.to_string())
                    ));
                }
                Some(false)
            }
            _ => Some(false),
        },
    }
}

/// Whether the copy of `attr` a pin to `locked` provides, with or without the
/// machine's overlays as chosen, is already in this machine's store. A target
/// without importPinned gets the package set imported plainly.
fn pinned_copy_built(
    nix: &Nix,
    target: &Target,
    system: &str,
    attr: &str,
    locked: &serde_json::Value,
    overlays: Option<bool>,
) -> bool {
    let Ok(literal) = crate::nix::nix_json(locked) else {
        return false;
    };
    if !resolve::is_attr_name(attr) {
        return false;
    }
    let out = nix.eval_apply(
        &target.eppur_config(),
        &format!(
            "c: if !(c ? importPinned) then null else (c.importPinned {{ locked = {literal}; overlays = {}; }}).{attr}.outPath",
            overlays.unwrap_or(false)
        ),
    );
    let path = match out {
        Ok(serde_json::Value::String(path)) => path,
        Ok(serde_json::Value::Null) => match resolve::copy_at(locked, system, attr) {
            Ok(Some(copy)) => match copy["out"].as_str() {
                Some(out) => out.to_string(),
                None => return false,
            },
            _ => return false,
        },
        _ => return false,
    };
    crate::nix::is_valid(&path).unwrap_or(false)
}

/// "1 package", "2 packages".
pub fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// A locked input as a revision and the day it is from, e.g.
/// "718f670e8453 from 2026-10-20".
pub fn describe_dated(locked: &serde_json::Value) -> String {
    match locked["lastModified"].as_i64() {
        Some(t) => format!("{} from {}", describe(locked), date::format(t / 86_400)),
        None => describe(locked),
    }
}

pub fn describe(locked: &serde_json::Value) -> String {
    locked["rev"]
        .as_str()
        .map(|r| r.chars().take(12).collect())
        .or_else(|| locked["narHash"].as_str().map(str::to_string))
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counterparts_are_only_what_the_input_changed() {
        let set = |names: &[&str]| -> HashSet<String> {
            names
                .iter()
                .map(|n| format!("/nix/store/{n}.drv"))
                .collect()
        };
        let baseline = set(&["fff-web-1.0", "aaa-source", "ccc-openssl-3.0", "ddd-top"]);
        let closure = set(&[
            "ggg-web-0.9",
            "aaa-source",
            "ccc-openssl-3.0",
            "hhh-other-1.0",
            "eee-top",
        ]);
        assert_eq!(
            counterparts("/nix/store/fff-web-1.0.drv", &closure, &baseline),
            ["/nix/store/ggg-web-0.9.drv"]
        );
        // One of many of its name, none of which the input touched.
        assert!(counterparts("/nix/store/zzz-source.drv", &closure, &baseline).is_empty());
        assert!(counterparts("/nix/store/ccc-openssl-3.1.drv", &closure, &baseline).is_empty());
        assert_eq!(
            counterparts("/nix/store/ddd-top.drv", &closure, &baseline),
            ["/nix/store/eee-top.drv"]
        );
    }

    #[test]
    fn evidence_only_changes_the_order() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let candidates = names(&["atlas", "exta", "todo", "zed"]);
        let marks: Vec<Vec<String>> = ["a", "e", "t", "z"].iter().map(|m| names(&[m])).collect();
        assert_eq!(
            evidence_first(&candidates, &marks, |m| m == "t" || m == "z"),
            names(&["todo", "zed", "atlas", "exta"])
        );
        assert_eq!(evidence_first(&candidates, &marks, |_| false), candidates);
    }

    #[test]
    fn an_input_is_known_by_its_store_name_revision_and_local_path() {
        let git = serde_json::json!({ "type": "git", "url": "file:///tmp/ext", "rev": "abc" });
        assert_eq!(
            source_marks(&git, "/nix/store/h1-source"),
            ["h1-source", "abc", "/tmp/ext"]
        );
        let github = serde_json::json!({ "type": "github", "rev": "abc", "url": "" });
        assert_eq!(
            source_marks(&github, "/nix/store/h1-source"),
            ["h1-source", "abc"]
        );
    }

    #[test]
    fn only_downloads_and_killed_builders_are_transient() {
        let mut graph = Graph::default();
        graph.nodes.insert(
            "/nix/store/aaa-src.tar.gz.drv".into(),
            crate::graph::Node {
                fixed_output: true,
                ..Default::default()
            },
        );
        graph
            .nodes
            .insert("/nix/store/bbb-app-1.0.drv".into(), Default::default());
        let download = "builder for '/nix/store/aaa-src.tar.gz.drv' failed with exit code 1";
        let compile = "builder for '/nix/store/bbb-app-1.0.drv' failed with exit code 1";
        let killed = "builder for '/nix/store/bbb-app-1.0.drv' failed due to signal 9 (Killed)";
        let errors = |m: &[&str]| m.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            transient_drvs(&errors(&[download, compile]), &graph),
            ["/nix/store/aaa-src.tar.gz.drv"]
        );
        assert!(transient_drvs(&errors(&[compile]), &graph).is_empty());
        assert_eq!(
            transient_drvs(&errors(&[killed]), &graph),
            ["/nix/store/bbb-app-1.0.drv"]
        );
        let mismatch = "hash mismatch in fixed-output derivation '/nix/store/aaa-src.tar.gz.drv'";
        assert!(transient_drvs(&errors(&[mismatch]), &graph).is_empty());
    }

    #[test]
    fn pins_file_relative_to_the_flake() {
        let h = "0123456789abcdfghijklmnpqrsvwxyz";
        assert_eq!(
            flake_relative(&format!("/nix/store/{h}-source/pins.json")).as_deref(),
            Some("pins.json")
        );
        assert_eq!(
            flake_relative(&format!("/nix/store/{h}-source/hosts/laptop/pins.json")).as_deref(),
            Some("hosts/laptop/pins.json")
        );
        assert_eq!(
            flake_relative(&format!("/opt/store/{h}-source/pins.json")).as_deref(),
            Some("pins.json")
        );
        assert_eq!(flake_relative("/home/alice/nixos/pins.json"), None);
    }
}
