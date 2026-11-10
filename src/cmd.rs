use crate::config::{self, Config, ROOT_PREFIX, Target};
use crate::date;
use crate::engine::{self, Change, Engine};
use crate::git::Git;
use crate::guard;
use crate::inputs;
use crate::lock::Lock;
use crate::log;
use crate::nix::{self, Nix};
use crate::pins::{Kind, Pin, PinsFile, Reason};
use crate::state;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const PINS: &str = "pins.json";

/// A duration such as "12h", "45min", "30m", "90s", "1d" or "3600".
fn parse_duration(text: &str) -> Result<std::time::Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (n, unit) = text.split_at(split);
    let n: u64 = n
        .parse()
        .map_err(|_| format!("{text:?} is not a duration such as 12h, 45min or 90s"))?;
    let secs = match unit.trim() {
        "" | "s" | "sec" => n,
        "m" | "min" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => {
            return Err(format!(
                "{text:?} is not a duration such as 12h, 45min or 90s"
            ));
        }
    };
    Ok(std::time::Duration::from_secs(secs))
}

fn default_system() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

#[derive(clap::Args)]
pub struct Where {
    /// The config file; by default $EPPUR_CONFIG, then
    /// ~/.config/eppur.json, then /etc/eppur.json.
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        help_heading = "Global options"
    )]
    pub config: Option<PathBuf>,
    /// The flake directory; by default the config's flake, then the current
    /// directory.
    #[arg(
        long,
        global = true,
        value_name = "DIR",
        help_heading = "Global options"
    )]
    pub flake: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    /// Run nix flake update first; only the named inputs when given.
    #[arg(short, long, num_args = 0.., value_name = "INPUT")]
    pub update: Option<Vec<String>>,
    /// Evaluate and build as usual and report what would be pinned and
    /// unpinned, then restore flake.lock and pins.json; nothing is committed
    /// or activated.
    #[arg(long)]
    pub dry_run: bool,
    /// Commit flake.lock and pins.json afterwards (default:
    /// programs.eppur.commit, which is off).
    #[arg(long, overrides_with = "no_commit")]
    pub commit: bool,
    /// Don't commit, overriding programs.eppur.commit.
    #[arg(long, overrides_with = "commit")]
    pub no_commit: bool,

    /// Only these hosts or homes from the config; repeatable, or
    /// comma-separated. host:NAME or home:NAME where a host and a home share
    /// a name.
    #[arg(
        long = "only",
        value_name = "NAME",
        value_delimiter = ',',
        help_heading = "Fleet options"
    )]
    pub only: Vec<String>,
    /// Hold this machine's own host like any other when it cannot be fixed,
    /// instead of giving up; for a machine that builds a fleet.
    #[arg(long, help_heading = "Fleet options")]
    pub hold_local: bool,
    /// Point this symlink at this machine's own system once it built, and
    /// remove it otherwise, so a separate step can activate it.
    #[arg(long, value_name = "PATH", help_heading = "Fleet options")]
    pub built_link: Option<PathBuf>,
    /// Keep every system that built alive with a GC root in this directory,
    /// e.g. for a binary cache to serve them. Without --only, roots this
    /// flake's runs made there for targets no longer configured are removed;
    /// nothing else is.
    #[arg(long, value_name = "DIR", help_heading = "Fleet options")]
    pub gc_roots: Option<PathBuf>,
    /// Exit with 3 when a host or home is held, e.g. so a service's
    /// OnFailure reports it.
    #[arg(long, help_heading = "Fleet options")]
    pub fail_on_hold: bool,

    /// The lock to pin back to; by default the one each target last built with.
    #[arg(long, value_name = "FILE", help_heading = "Tuning")]
    pub old_lock: Option<PathBuf>,
    /// Give up after this many evaluate-and-build rounds.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 8,
        value_parser = clap::value_parser!(u32).range(1..),
        help_heading = "Tuning"
    )]
    pub max_rounds: u32,
    /// Hold a host, or give up on this machine's own, rather than pin more
    /// than this many new packages for it in one run.
    #[arg(long, value_name = "N", default_value_t = 10, help_heading = "Tuning")]
    pub max_new_pins: usize,
    /// Stop updating, evaluating and building after this long, e.g. 12h,
    /// 45min or 90s; activation afterwards is not cut short.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration, help_heading = "Tuning")]
    pub timeout: Option<std::time::Duration>,
    /// The system for eval checks when a target's own cannot be evaluated.
    #[arg(long, value_name = "SYSTEM", default_value_t = default_system(), help_heading = "Tuning")]
    pub system: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    None,
    Switch,
    Boot,
}

#[derive(clap::Args)]
pub struct Pick {
    /// A NixOS host; by default this machine's own host or home.
    #[arg(long, value_name = "NAME", conflicts_with = "home")]
    pub host: Option<String>,
    /// A Home Manager configuration.
    #[arg(long, value_name = "NAME")]
    pub home: Option<String>,
}

struct Ctx {
    config: Option<Config>,
    config_path: Option<PathBuf>,
    flake: PathBuf,
    /// Where the flake directory came from, for error messages.
    source: String,
}

impl Ctx {
    /// The config and flake a command acts on. A config named by --config or
    /// $EPPUR_CONFIG must load; one found by itself is skipped with a
    /// warning when the command can do without.
    fn open(at: &Where, need_config: bool) -> Result<Self> {
        let (config, config_path) = match config::locate(at.config.as_deref()) {
            None if need_config => return Err(config::missing()),
            None => (None, None),
            Some(found) => match config::read(&found.path) {
                Ok(c) => {
                    log::set_quiet(c.quiet);
                    (Some(c), Some(found.path))
                }
                Err(e) if need_config || found.explicit => {
                    return Err(e.context(format!("the config file from {}", found.from)));
                }
                Err(e) => {
                    log::warn(&format!("ignoring {}: {e:#}", found.path.display()));
                    (None, None)
                }
            },
        };
        let (flake, source) = match (&at.flake, config.as_ref().and_then(|c| c.flake.clone())) {
            (Some(f), _) => (f.clone(), "from --flake".to_string()),
            (None, Some(f)) => (
                PathBuf::from(f),
                format!(
                    "programs.eppur.flake in {}",
                    config_path.as_deref().unwrap_or(Path::new("?")).display()
                ),
            ),
            (None, None) => (std::env::current_dir()?, "the current directory".into()),
        };
        let flake = match flake.canonicalize() {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "flake directory {} does not exist ({source}){}",
                flake.display(),
                if at.flake.is_some() {
                    ""
                } else {
                    "; pass --flake DIR"
                }
            ),
            Err(e) => {
                return Err(anyhow!(e).context(format!(
                    "resolving the flake directory {} ({source})",
                    flake.display()
                )));
            }
        };
        Ok(Self {
            config,
            config_path,
            flake,
            source,
        })
    }

    /// Which flake and config a command acts on, said first, since both can
    /// come from far away (/etc/eppur.json from any directory).
    fn announce(&self) {
        match &self.config_path {
            Some(c) => log::info(&format!(
                "flake {} (config {})",
                self.flake.display(),
                c.display()
            )),
            None => log::info(&format!("flake {}", self.flake.display())),
        }
    }

    fn pins_path(&self) -> PathBuf {
        self.flake.join(PINS)
    }

    /// The target --host or --home names, or this machine's own. A name the
    /// config does not list (and `file` has no entry for) is a typo: it would
    /// write an entry nothing ever reads.
    fn target(&self, pick: &Pick, file: Option<&PinsFile>) -> Result<Target> {
        let target = match (&pick.host, &pick.home, &self.config) {
            (Some(h), _, _) => Target::new(Kind::Nixos, h),
            (_, Some(h), _) => Target::new(Kind::Home, h),
            (None, None, Some(c)) => return Ok(c.local()),
            (None, None, None) => bail!("no config to take the host from; pass --host or --home"),
        };
        let Some(cfg) = &self.config else {
            return Ok(target);
        };
        let mut known = cfg.targets();
        known.push(cfg.local());
        if known.contains(&target)
            || file.is_some_and(|f| f.entry(target.kind, &target.name).is_some())
        {
            return Ok(target);
        }
        let names: Vec<String> = known
            .iter()
            .filter(|t| t.kind == target.kind)
            .map(|t| t.name.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        bail!(
            "{target} is not a configured {} (configured: {}); check --{}",
            target.kind.noun(),
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            },
            target.kind.noun()
        )
    }

    /// The flags that name `target` on the command line, empty for this
    /// machine's own entry.
    fn flag_for(&self, target: &Target) -> String {
        if self.config.as_ref().map(Config::local).as_ref() == Some(target) {
            return String::new();
        }
        match target.kind {
            Kind::Nixos => format!(" --host {}", target.name),
            Kind::Home => format!(" --home {}", target.name),
        }
    }

    /// For an attr that is not pinned on `target`: where it is pinned instead.
    fn pinned_elsewhere(&self, file: &PinsFile, target: &Target, attr: &str) -> String {
        let others: Vec<String> = file
            .entries()
            .filter(|(k, n, e)| {
                !(*k == target.kind && *n == target.name) && !e.inputs_pinning(attr).is_empty()
            })
            .map(|(k, n, _)| {
                let t = Target::new(k, n);
                let flag = self.flag_for(&t);
                if flag.is_empty() {
                    format!("{t} (this machine's own; leave out --host and --home)")
                } else {
                    format!("{t} (pass{flag})")
                }
            })
            .collect();
        if others.is_empty() {
            String::new()
        } else {
            format!("; it is pinned on {}", others.join(", "))
        }
    }
}

/// Create the pins file when missing and make sure a git flake can see it.
/// Returns whether it created the file, and whether it added it to the index.
fn ensure_pins(path: &Path, git: Option<&Git>, quietly: bool) -> Result<(bool, bool)> {
    let say = |msg: String| {
        if !quietly {
            log::info(&msg);
        }
    };
    let created = !path.exists();
    if created {
        PinsFile::default().save(path)?;
        say(format!("created {}", path.display()));
    }
    let mut added = false;
    if let Some(git) = git
        && !git.tracked(path)
    {
        git.intent_to_add(path)?;
        added = true;
        say(format!("added {} to the git index", path.display()));
    }
    Ok((created, added))
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Activate the built system. With nh it is handed the store path, so it only
/// diffs and activates; nixos-rebuild and home-manager evaluate the flake again.
fn activate(
    target: &Target,
    flake: &Path,
    built: Option<&str>,
    how: Activation,
    switcher: &str,
) -> Result<ExitCode> {
    let verb = match (how, target.kind) {
        (Activation::None, _) => return Ok(ExitCode::SUCCESS),
        (Activation::Switch, _) => "switch",
        (Activation::Boot, Kind::Nixos) => "boot",
        (Activation::Boot, Kind::Home) => {
            bail!("Home Manager has no boot activation; use eppur switch")
        }
    };
    let installable = format!("{}#{}", flake.display(), target.name);
    let mut cmd = match (switcher, target.kind) {
        ("native", Kind::Nixos) => {
            let mut c = Command::new("nixos-rebuild");
            c.args([verb, "--flake", &installable]);
            if !is_root() {
                c.arg("--sudo");
            }
            c
        }
        ("native", Kind::Home) => {
            let mut c = Command::new("home-manager");
            c.args([verb, "--flake", &installable]);
            c
        }
        (_, kind) => {
            let mut c = Command::new("nh");
            c.args([if kind == Kind::Nixos { "os" } else { "home" }, verb]);
            match (built, kind) {
                (Some(path), _) => c.arg(path),
                (None, Kind::Nixos) => c.arg(flake).args(["-H", &target.name]),
                (None, Kind::Home) => c.arg(flake).args(["-c", &target.name]),
            };
            c
        }
    };
    let program = cmd.get_program().to_string_lossy().into_owned();
    log::info(&format!(
        "{} {target} with {program}",
        if verb == "boot" {
            "setting up boot of"
        } else {
            "switching"
        }
    ));
    let status = cmd
        .status()
        .with_context(|| format!("running {program}; is it installed?"))?;
    if status.success() {
        return Ok(ExitCode::SUCCESS);
    }
    log::warn(&format!(
        "{program} failed ({status}); {target} built, flake.lock and pins.json kept"
    ));
    Ok(ExitCode::from(4))
}

/// The file in a --gc-roots directory listing the roots this flake's runs
/// made, so two flakes sharing the directory leave each other's alone.
fn roots_manifest(dir: &Path, flake: &Path) -> PathBuf {
    dir.join(format!(
        ".{ROOT_PREFIX}roots-{}",
        state::key(&flake.to_string_lossy())
    ))
}

/// Roots made when eppur was called nixheal take the new names, rooted again
/// since nix tracks a root by its path. A held host has no other root.
fn adopt_old_roots(dir: &Path, flake: &Path) -> Result<()> {
    let old_manifest = dir.join(format!(
        ".nixheal-roots-{}",
        state::key(&flake.to_string_lossy())
    ));
    let Ok(text) = std::fs::read_to_string(&old_manifest) else {
        return Ok(());
    };
    let mut names = Vec::new();
    for old_name in text.lines() {
        let Some(rest) = old_name.strip_prefix("nixheal-") else {
            continue;
        };
        let name = format!("{ROOT_PREFIX}{rest}");
        let old = dir.join(old_name);
        if let Ok(path) = std::fs::read_link(&old) {
            if path.exists() {
                nix::add_path_root(&path, &dir.join(&name))?;
            }
            std::fs::remove_file(&old).with_context(|| format!("removing {}", old.display()))?;
        }
        names.push(name);
    }
    let mut text = names.join("\n");
    text.push('\n');
    crate::pins::write_atomic(&roots_manifest(dir, flake), text.as_bytes())?;
    std::fs::remove_file(&old_manifest)
        .with_context(|| format!("removing {}", old_manifest.display()))
}

/// Root the systems that built, and remove the roots this flake's runs made
/// for targets that are no longer configured (only when `prune`), so their
/// systems can be collected. Roots eppur did not make are left alone.
fn update_roots(
    dir: &Path,
    flake: &Path,
    built: &[(Target, String)],
    targets: &[Target],
    prune: bool,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    adopt_old_roots(dir, flake)?;
    let manifest = roots_manifest(dir, flake);
    // Roots are named by target only, so two flakes rooting a target of the
    // same name in one directory would overwrite and prune each other's.
    let prefix = format!(".{ROOT_PREFIX}roots-");
    for other in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let other = other?.path();
        let name = other.file_name().map(|n| n.to_string_lossy().into_owned());
        if other == manifest || !name.is_some_and(|n| n.starts_with(&prefix)) {
            continue;
        }
        let theirs = std::fs::read_to_string(&other).unwrap_or_default();
        if let Some((target, _)) = built
            .iter()
            .find(|(t, _)| theirs.lines().any(|l| l == t.root_name()))
        {
            bail!(
                "{} already roots {} for another flake (see {}); give each flake its own --gc-roots directory",
                dir.display(),
                target.root_name(),
                other.display()
            );
        }
    }
    for (target, drv) in built {
        nix::add_root(drv, &dir.join(target.root_name()))?;
    }
    let ours: Vec<String> = std::fs::read_to_string(&manifest)
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let keep: Vec<String> = targets.iter().map(Target::root_name).collect();
    // A target still configured keeps its place, built this run or not (a
    // held one keeps its old root), so it is pruned once it goes.
    let mut listed: Vec<String> = if prune {
        for name in ours.iter().filter(|n| !keep.contains(n)) {
            let path = dir.join(name);
            if path
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                log::info(&format!(
                    "removed the root {name}: its host or home is no longer configured"
                ));
            }
        }
        ours.into_iter().filter(|n| keep.contains(n)).collect()
    } else {
        ours
    };
    for (target, _) in built {
        let name = target.root_name();
        if !listed.contains(&name) {
            listed.push(name);
        }
    }
    listed.sort();
    let mut text = listed.join("\n");
    text.push('\n');
    crate::pins::write_atomic(&manifest, text.as_bytes())
}

fn remove_link(link: &Path) -> Result<()> {
    match std::fs::remove_file(link) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", link.display()))
        }
        _ => Ok(()),
    }
}

/// Refuse setups where a command that writes would do damage or write to the
/// wrong place: a directory that is not a locked flake, running as root in a
/// checkout someone else owns, or a flake that is a store path.
fn check_flake(ctx: &Ctx) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let flake = ctx.flake.as_path();
    if !flake.join("flake.lock").is_file() {
        bail!(
            "no flake.lock in {} ({}); eppur needs a locked flake: {}run nix flake lock there",
            flake.display(),
            ctx.source,
            if ctx.source == "from --flake" {
                ""
            } else {
                "pass --flake DIR, or "
            }
        );
    }
    let text = flake.to_string_lossy();
    if text.contains('#') || text.contains('?') {
        bail!(
            "{} contains '#' or '?', which nix reads as part of a flake reference; move the checkout",
            flake.display()
        );
    }
    if flake.starts_with("/nix/store") {
        bail!(
            "{} is in the nix store; point programs.eppur.flake (or --flake) at your checkout",
            flake.display()
        );
    }
    let owner = std::fs::metadata(flake)
        .with_context(|| format!("reading {}", flake.display()))?
        .uid();
    if is_root() && owner != 0 {
        bail!(
            "{} belongs to uid {owner}; run eppur as that user, or as root it would leave root-owned files and git objects there (for the timer, set programs.eppur.autoUpgrade.user)",
            flake.display()
        );
    }
    Ok(())
}

/// What became of flake.lock and pins.json, for the closing line.
enum Closing {
    NoRepository { commit: bool },
    Unchanged,
    Uncommitted,
    Committed(String),
}

impl std::fmt::Display for Closing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Closing::NoRepository { commit: true } => {
                f.write_str("the flake is not in a git repository, so nothing was committed")
            }
            Closing::NoRepository { commit: false } => {
                f.write_str("flake.lock and pins.json are not in a git repository")
            }
            Closing::Unchanged => f.write_str("flake.lock and pins.json unchanged"),
            Closing::Uncommitted => f.write_str("flake.lock and pins.json uncommitted"),
            Closing::Committed(what) => write!(f, "committed {what}"),
        }
    }
}

/// The counts of a run's changes, as the closing line reports them.
fn tally(changes: &[Change]) -> String {
    let parts: Vec<String> = ["pinned", "unpinned", "held", "released", "moved to a donor"]
        .into_iter()
        .filter_map(|word| {
            let n = changes.iter().filter(|c| c.word() == word).count();
            (n > 0).then(|| format!("{n} {word}"))
        })
        .collect();
    if parts.is_empty() {
        "nothing pinned or unpinned in this run".into()
    } else {
        parts.join(", ")
    }
}

/// Run the engine, and again each time it finds flake inputs to take back: a
/// new lock changes what every target evaluates to, so nothing of the run
/// before it stands. If a target that built before a rollback, or one it was
/// for, then fails, or an input comes to a revision it was already sent back
/// to (targets that last built at different revisions can each undo what the
/// other needs), rolling back cannot settle it: the lock and pins go back to
/// how they were, and the run ends without it, holding the targets as it would
/// have. That can happen once, so with finitely many revisions this ends.
fn run_rolling_back(
    engine_with: &dyn Fn(bool) -> Result<Engine>,
    lock_path: &Path,
    pins_path: &Path,
    allowed: bool,
) -> Result<(Engine, engine::Report)> {
    let start_lock = std::fs::read(lock_path)?;
    let start_pins = std::fs::read(pins_path)?;
    let mut may_roll_back = allowed;
    let mut tried: BTreeSet<(String, String)> = BTreeSet::new();
    let mut must_build: BTreeSet<String> = BTreeSet::new();
    let mut changes: Vec<Change> = Vec::new();
    loop {
        let engine = engine_with(may_roll_back)?;
        let pins_now = std::fs::read(pins_path)?;
        let mut report = engine.run()?;
        let mut undo = false;
        if !report.rollback.is_empty() {
            must_build.extend(report.built.iter().map(|(t, _)| t.to_string()));
            must_build.extend(report.rollback.iter().flat_map(|r| r.targets.clone()));
            for r in &report.rollback {
                let revision = r.locked["narHash"].as_str().unwrap_or_default();
                undo |= !tried.insert((r.input.clone(), revision.to_string()));
            }
            if !undo {
                guard::critical(|| crate::pins::write_atomic(pins_path, &pins_now))?;
                // A revision that cannot be locked is one more reason to hold.
                match inputs::apply(
                    &engine.nix,
                    lock_path,
                    pins_path,
                    &report.rollback,
                    &report.built.iter().map(|(t, _)| t.to_string()).collect(),
                    date::today(),
                ) {
                    Ok(applied) => {
                        changes.extend(applied);
                        continue;
                    }
                    Err(e) if guard::timed_out() => return Err(e),
                    Err(e) => {
                        log::warn(&format!("rolling back a flake input failed: {e:#}"));
                        undo = true;
                    }
                }
            }
        }
        undo |= report
            .failed
            .iter()
            .any(|t| must_build.contains(&t.to_string()));
        if undo {
            log::change(
                "rolling back flake inputs did not get every target building; putting them back",
            );
            guard::critical(|| {
                crate::pins::write_atomic(lock_path, &start_lock)?;
                crate::pins::write_atomic(pins_path, &start_pins)
            })?;
            may_roll_back = false;
            must_build.clear();
            changes.clear();
            continue;
        }
        changes.append(&mut report.changes);
        report.changes = changes;
        return Ok((engine, report));
    }
}

pub fn reconcile(at: &Where, args: &BuildArgs, how: Activation) -> Result<ExitCode> {
    let update = args.update.as_deref();
    let ctx = Ctx::open(at, true)?;
    let cfg = ctx.config.clone().expect("config required");
    ctx.announce();
    let mut targets = cfg.targets();
    // --only NAME, or host:NAME / home:NAME where a host and a home share it.
    let wanted = |t: &Target, n: &str| match n.split_once(':') {
        Some(("host", name)) => t.kind == Kind::Nixos && t.name == name,
        Some(("home", name)) => t.kind == Kind::Home && t.name == name,
        _ => t.name == n,
    };
    if !args.only.is_empty() {
        for n in &args.only {
            if !n.contains(':') && targets.iter().filter(|t| t.name == *n).count() > 1 {
                bail!("--only {n} names both a host and a home; pass host:{n} or home:{n}");
            }
        }
        let unknown: Vec<&String> = args
            .only
            .iter()
            .filter(|n| !targets.iter().any(|t| wanted(t, n)))
            .collect();
        if !unknown.is_empty() {
            let known: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
            bail!(
                "--only {} is not among the configured hosts and homes ({})",
                unknown
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                known.join(", ")
            );
        }
        targets.retain(|t| args.only.iter().any(|n| wanted(t, n)));
    }
    if targets.is_empty() {
        bail!("no hosts or homes to build: set programs.eppur.hosts or homes");
    }
    let local = Some(cfg.local()).filter(|l| targets.contains(l));
    if how == Activation::Boot && local.as_ref().is_some_and(|l| l.kind == Kind::Home) {
        bail!("Home Manager has no boot activation; use eppur switch");
    }
    // The machine's own host is given up on, not held, unless it builds a
    // fleet: holding it would leave it running an old system unnoticed.
    let guarded = local.clone().filter(|_| !args.hold_local);

    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;

    let git = Git::open(&ctx.flake);
    let pins_path = ctx.pins_path();
    let lock_path = ctx.flake.join("flake.lock");
    let orig_lock =
        std::fs::read(&lock_path).with_context(|| format!("reading {}", lock_path.display()))?;
    if let Some(names) = update {
        let known = Lock::from_bytes(orig_lock.clone())
            .with_context(|| format!("parsing {}", lock_path.display()))?
            .inputs();
        let unknown: Vec<&str> = names
            .iter()
            .filter(|n| !known.contains(n))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            let mut known = known;
            known.sort();
            bail!(
                "--update {}: not an input of flake.lock (its inputs: {})",
                unknown.join(", "),
                known.join(", ")
            );
        }
    }
    if !args.dry_run
        && let Some(link) = &args.built_link
    {
        remove_link(link)?;
    }
    let given_old_lock = args.old_lock.as_deref().map(Lock::load).transpose()?;
    let pins_before = match std::fs::read(&pins_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", pins_path.display())),
    };
    let (created_pins, indexed) = ensure_pins(&pins_path, git.as_ref(), args.dry_run)?;
    guard::arm(guard::Restore {
        lock_path: lock_path.clone(),
        lock: orig_lock.clone(),
        pins_path: pins_path.clone(),
        pins: if created_pins { None } else { pins_before },
        untrack: indexed,
        flake: ctx.flake.clone(),
    });

    // Any error from here on is restored by main, after it is printed.
    let mut last_good = std::collections::HashMap::new();
    let last_good_pins: std::collections::HashMap<_, _> = targets
        .iter()
        .filter_map(|t| state::load_last_good_pins(&ctx.flake, t).map(|e| (t.clone(), e)))
        .collect();
    let old_lock = match given_old_lock {
        Some(l) => l,
        None => {
            for t in &targets {
                if let Some(l) = state::load_last_good(&ctx.flake, Some(t)) {
                    last_good.insert(t.clone(), l);
                }
            }
            match state::load_last_good(&ctx.flake, None) {
                Some(l) => l,
                None => Lock::from_bytes(orig_lock.clone())?,
            }
        }
    };

    let nix = Nix::new(&ctx.flake);
    if let Some(limit) = args.timeout {
        guard::deadline(limit);
    }
    if let Some(inputs) = update {
        nix.flake_update(inputs)?;
    }
    inputs::settle(&nix, &lock_path, &pins_path, &cfg.input_deny)?;

    let host_names = nix.output_names("nixosConfigurations");
    let home_names = nix.output_names("homeConfigurations");
    let flake_targets = host_names.as_ref().zip(home_names.as_ref()).map(|(h, m)| {
        h.iter()
            .map(|n| format!("host {n}"))
            .chain(m.iter().map(|n| format!("home {n}")))
            .collect::<Vec<_>>()
    });
    // flake.lock is the whole flake's. A run may only roll an input back if it
    // speaks for the whole flake: it builds a fleet, or nothing is left out.
    let covers_flake = args.hold_local
        || flake_targets.as_ref().is_some_and(|all| {
            all.iter()
                .all(|t| targets.iter().any(|mine| mine.to_string() == *t))
        });
    let engine_with = |may_roll_back: bool| -> Result<Engine> {
        Ok(Engine {
            nix: Nix::new(&ctx.flake),
            pins_path: pins_path.clone(),
            new_lock: Lock::load(&lock_path)?,
            old_lock: old_lock.clone(),
            last_good: last_good.clone(),
            last_good_pins: last_good_pins.clone(),
            targets: targets.clone(),
            guarded: guarded.clone(),
            local: local.clone(),
            pins_rel: format!(
                "{}{PINS}",
                git.as_ref().map(|g| g.prefix()).unwrap_or_default()
            ),
            deny: cfg.deny.clone(),
            system: args.system.clone(),
            max_rounds: args.max_rounds,
            max_new_pins: args.max_new_pins,
            warn_after_days: cfg.warn_after_days,
            today: date::today(),
            donors: cfg.donors.clone(),
            input_deny: cfg.input_deny.clone(),
            package_sets: cfg
                .package_sets
                .iter()
                .cloned()
                .chain([cfg.input.clone().unwrap_or_else(|| "nixpkgs".into())])
                .collect(),
            flake_targets: flake_targets.clone(),
            host_names: host_names.clone(),
            home_names: home_names.clone(),
            may_roll_back,
            retry_same_after_days: cfg.retry_same_derivation_after_days,
            copies: Default::default(),
            lost: Default::default(),
        })
    };
    log::set_dry_run(args.dry_run);
    let result = run_rolling_back(
        &engine_with,
        &lock_path,
        &pins_path,
        args.only.is_empty() && covers_flake,
    );
    log::set_dry_run(false);
    guard::built();
    let (engine, report) = result?;

    if report.local_failure.is_some() {
        guard::restore()?;
        log::change("nothing committed or activated");
        return Ok(ExitCode::from(1));
    }

    if args.dry_run {
        guard::restore_quietly()?;
        log::change(&format!("dry run: {}", tally(&report.changes)));
        if args.fail_on_hold && report.held > 0 {
            return Ok(ExitCode::from(3));
        }
        return Ok(ExitCode::SUCCESS);
    }

    // From here the run's results stand or fall together: any error before
    // the commit puts flake.lock and pins.json back.
    let settled = (|| -> Result<Closing> {
        if let Some(dir) = &args.gc_roots {
            update_roots(
                dir,
                &ctx.flake,
                &report.built,
                &cfg.targets(),
                args.only.is_empty(),
            )?;
            log::detail(&format!(
                "rooted {} in {}",
                engine::plural(report.built.len(), "system"),
                dir.display()
            ));
        }
        guard::settle(|| {
            let commit = (cfg.commit || args.commit) && !args.no_commit;
            let Some(git) = &git else {
                return Ok(Closing::NoRepository { commit });
            };
            if !git.changed(&[&lock_path, &pins_path])? {
                return Ok(Closing::Unchanged);
            }
            if !commit {
                return Ok(Closing::Uncommitted);
            }
            {
                let title = if git.changed(&[&lock_path])? {
                    "eppur: update flake.lock"
                } else {
                    "eppur: update pins"
                };
                let mut message = title.to_string();
                if !report.changes.is_empty() {
                    message.push_str("\n\n");
                    let lines: Vec<String> = report.changes.iter().map(Change::to_string).collect();
                    message.push_str(&lines.join("\n"));
                }
                if git.commit(&[&lock_path, &pins_path], &message)? {
                    return Ok(Closing::Committed(match git.head() {
                        Some(h) => format!("{h} {title:?}"),
                        None => format!("{title:?}"),
                    }));
                }
                Ok(Closing::Unchanged)
            }
        })
    })();
    let closing = settled?;
    let done = format!(
        "done: {} built, {}; {closing}",
        report.built.len(),
        tally(&report.changes)
    );
    // Files left changed are worth a line under --quiet too.
    if matches!(closing, Closing::Uncommitted) {
        log::change(&done);
    } else {
        log::info(&done);
    }

    // Only now, with the results kept, are these locks known to build.
    for (target, _) in &report.built {
        state::save_last_good(&ctx.flake, Some(target), &engine.new_lock)?;
    }
    for (target, entry) in &report.built_pins {
        state::save_last_good_pins(&ctx.flake, target, entry)?;
    }
    if !report.built.is_empty() {
        state::save_last_good(&ctx.flake, None, &engine.new_lock)?;
    }

    let built = match report.local_drv.as_deref().map(nix::out_path) {
        Some(Ok(path)) => Some(path),
        Some(Err(e)) => {
            log::info(&format!("cannot find the built system: {e:#}"));
            None
        }
        None => None,
    };
    if let (Some(link), Some(drv)) = (&args.built_link, &report.local_drv) {
        nix::add_root(drv, link)?;
    }

    let code = match &local {
        Some(target) if report.local_held => {
            if how != Activation::None {
                log::warn(&format!("not activating: {target} is held"));
            }
            ExitCode::SUCCESS
        }
        Some(target) => activate(target, &ctx.flake, built.as_deref(), how, &cfg.switcher)?,
        None if how != Activation::None => {
            log::warn(&format!(
                "not activating: {} was not built in this run",
                cfg.local()
            ));
            ExitCode::SUCCESS
        }
        None => ExitCode::SUCCESS,
    };
    if args.fail_on_hold && report.held > 0 && code == ExitCode::SUCCESS {
        return Ok(ExitCode::from(3));
    }
    Ok(code)
}

/// `nix flake update` that leaves a pinned flake input alone until there is
/// something new to try, so the lock only changes when something upstream did:
/// a fleet's "nothing changed" check goes on working while an input is pinned.
pub fn update(at: &Where) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    ctx.announce();
    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;
    let lock_path = ctx.flake.join("flake.lock");
    let pins_path = ctx.pins_path();
    let deny = ctx
        .config
        .as_ref()
        .map_or_else(Vec::new, |c| c.input_deny.clone());
    let lock =
        std::fs::read(&lock_path).with_context(|| format!("reading {}", lock_path.display()))?;
    let pins = match std::fs::read(&pins_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", pins_path.display())),
    };
    guard::arm(guard::Restore {
        lock_path: lock_path.clone(),
        lock,
        pins_path: pins_path.clone(),
        pins,
        untrack: false,
        flake: ctx.flake.clone(),
    });
    let nix = Nix::new(&ctx.flake);
    nix.flake_update(&[])?;
    inputs::settle(&nix, &lock_path, &pins_path, &deny)?;
    guard::settle(|| Ok(()))?;
    Ok(ExitCode::SUCCESS)
}

pub fn merge(at: &Where, other: &Path) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    ctx.announce();
    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;
    let bytes = std::fs::read(other).with_context(|| format!("reading {}", other.display()))?;
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", other.display()))?;
    if raw.get("nodes").is_some() {
        bail!(
            "{} is a flake.lock, not a pins.json; merge takes another pins.json",
            other.display()
        );
    }
    if raw.get("version").is_none() {
        bail!("{} is not an eppur pins file (no version)", other.display());
    }
    let incoming =
        PinsFile::parse(&bytes).with_context(|| format!("parsing {}", other.display()))?;
    let pins_path = ctx.pins_path();
    ensure_pins(&pins_path, Git::open(&ctx.flake).as_ref(), false)?;
    let mut file = PinsFile::load(&pins_path)?;
    let lock_sha256 = Lock::load(&ctx.flake.join("flake.lock"))
        .ok()
        .map(|l| l.sha256());
    let mut taken = 0;
    for (kind, name, entry) in incoming.entries() {
        let target = Target::new(kind, name);
        if file.entry(kind, name) == Some(entry) {
            continue;
        }
        if file.merge_for_lock(kind, name, entry.clone(), lock_sha256.as_deref()) {
            taken += 1;
            log::detail(&format!("took {target} from {}", other.display()));
        } else {
            log::info(&format!(
                "kept {target}: the entry here comes from a newer lock"
            ));
        }
    }
    if file.merge_inputs(&incoming) {
        taken += 1;
        log::detail(&format!(
            "took the flake input pins from {}",
            other.display()
        ));
    }
    file.save(&pins_path)?;
    log::change(&format!(
        "merged {} entr{} from {} into {}",
        taken,
        if taken == 1 { "y" } else { "ies" },
        other.display(),
        pins_path.display()
    ));
    Ok(ExitCode::SUCCESS)
}

fn reason_words(reason: Reason) -> &'static str {
    match reason {
        Reason::Build => "failed to build",
        Reason::Eval => "failed to evaluate",
        Reason::Manual => "pinned by hand",
    }
}

pub fn status(at: &Where, json: bool) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    let pins_path = ctx.pins_path();
    let file = PinsFile::load(&pins_path)?;
    let today = date::today();
    let warn = ctx.config.as_ref().map_or(14, |c| c.warn_after_days);
    if json {
        let retry = ctx
            .config
            .as_ref()
            .map_or(7, |c| c.retry_same_derivation_after_days);
        println!("{}", crate::summary::summary(&file, today, warn, retry));
        return Ok(ExitCode::SUCCESS);
    }
    println!("flake {}", ctx.flake.display());
    if !pins_path.exists() {
        println!("no pins.json in {}", ctx.flake.display());
        return Ok(ExitCode::SUCCESS);
    }

    struct Row {
        target: String,
        what: String,
        reason: String,
        since: String,
        note: Option<String>,
    }
    let mut rows: Vec<Row> = Vec::new();
    let (mut pins, mut held, mut stale) = (0, 0, 0);
    let mut since = |date_text: &str, manual: bool| {
        let age = date::parse(date_text).map(|d| today - d);
        let old = !manual && age.is_none_or(|a| a > warn);
        if old {
            stale += 1;
        }
        format!(
            "{date_text} ({}){}",
            crate::summary::age_text(age),
            if old { " STALE" } else { "" }
        )
    };
    for (kind, name, entry) in file.entries() {
        let target = Target::new(kind, name).to_string();
        if let Some(h) = &entry.held {
            held += 1;
            rows.push(Row {
                target: target.clone(),
                what: "-".into(),
                reason: format!("whole {} held", kind.noun()),
                since: since(&h.since, false),
                note: Some(h.reason.clone()),
            });
        }
        for (attr, sets) in entry.packages() {
            pins += 1;
            let pin = sets[0].1;
            let inputs: Vec<&str> = sets.iter().map(|(i, _)| *i).collect();
            let queued = sets.iter().any(|(_, p)| p.checked_nar_hash.is_empty())
                && pin.reason != Reason::Manual;
            let revs: std::collections::BTreeSet<String> = sets
                .iter()
                .map(|(_, p)| {
                    let version = p
                        .version
                        .as_ref()
                        .map_or(String::new(), |v| format!("{v} "));
                    match &p.donor {
                        Some(d) => {
                            format!("{version}from {d} at {}", engine::describe(&p.locked))
                        }
                        None => format!("{version}at {}", engine::describe(&p.locked)),
                    }
                })
                .collect();
            rows.push(Row {
                target: target.clone(),
                what: format!("{attr} ({})", inputs.join(", ")),
                reason: format!(
                    "{}{}",
                    crate::summary::cause(pin),
                    if queued { ", retry queued" } else { "" }
                ),
                since: format!(
                    "{}  {}",
                    since(&pin.since, pin.reason == Reason::Manual),
                    revs.into_iter().collect::<Vec<_>>().join(", ")
                ),
                note: None,
            });
        }
    }
    let input_pins = file.inputs.len();
    for (input, pin) in &file.inputs {
        rows.push(Row {
            target: "flake".into(),
            what: format!("input {input}"),
            reason: format!(
                "{}{}",
                crate::summary::input_cause(pin),
                if pin.retrying { ", retrying" } else { "" }
            ),
            since: format!(
                "{}  at {}",
                since(&pin.since, false),
                engine::describe(&pin.locked)
            ),
            note: None,
        });
    }
    if rows.is_empty() {
        println!("no pins and nothing held");
        return Ok(ExitCode::SUCCESS);
    }
    let width = |f: fn(&Row) -> &str, title: &str| {
        rows.iter()
            .map(|r| f(r).len())
            .chain([title.len()])
            .max()
            .unwrap_or(0)
    };
    let (wt, ww, wr) = (
        width(|r| &r.target, "TARGET"),
        width(|r| &r.what, "PACKAGE (SETS)"),
        width(|r| &r.reason, "REASON"),
    );
    println!(
        "{:wt$}  {:ww$}  {:wr$}  SINCE",
        "TARGET", "PACKAGE (SETS)", "REASON"
    );
    for r in &rows {
        println!(
            "{:wt$}  {:ww$}  {:wr$}  {}",
            r.target, r.what, r.reason, r.since
        );
        if let Some(note) = &r.note {
            println!("    {note}");
        }
    }
    println!(
        "{}, {}{held} held, {stale} stale (older than {warn} days).",
        engine::plural(pins, "pinned package"),
        if input_pins > 0 {
            format!("{}, ", engine::plural(input_pins, "pinned flake input"))
        } else {
            String::new()
        }
    );
    Ok(ExitCode::SUCCESS)
}

pub fn why(at: &Where, pick: &Pick, attr: &str) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    let file = PinsFile::load(&ctx.pins_path())?;
    if let Some(pin) = file.inputs.get(attr)
        && !file
            .entries()
            .any(|(_, _, e)| e.packages().contains_key(attr))
    {
        println!("flake input {attr} is held back in flake.lock, for the whole flake");
        println!("since:     {}", pin.since);
        println!("kept at:   {}", engine::describe(&pin.locked));
        println!("cause:     {}", crate::summary::input_cause(pin));
        println!("failed on: {}", pin.targets.join(", "));
        if pin.retrying {
            println!("retrying:  the lock has a newer revision");
        } else {
            println!("retried:   once upstream has a new revision");
        }
        if let Some(drv) = &pin.failed_drv
            && let Some(tail) = nix::log_tail(drv, 20)
        {
            println!("\nend of the build log of {drv}:\n{tail}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let target = ctx.target(pick, Some(&file))?;
    let Some(sets) = file
        .entry(target.kind, &target.name)
        .and_then(|e| e.packages().remove(attr).filter(|s| !s.is_empty()))
    else {
        let held = file
            .entry(target.kind, &target.name)
            .and_then(|e| e.held.as_ref())
            .map(|h| format!("; {target} is held since {}: {}", h.since, h.reason))
            .unwrap_or_default();
        bail!(
            "{attr} is not pinned on {target}{}{held}",
            ctx.pinned_elsewhere(&file, &target, attr)
        );
    };
    println!("{target}: {attr}");
    let mut failures: Vec<&Pin> = Vec::new();
    for (_, pin) in &sets {
        if !failures.iter().any(|p| p.same_failure(pin) || *p == *pin) {
            failures.push(pin);
        }
    }
    let inputs: Vec<String> = sets
        .iter()
        .map(|(i, p)| {
            format!(
                "{i} at {} (a revision from {}{}){}",
                engine::describe(&p.locked),
                p.locked["lastModified"]
                    .as_i64()
                    .map_or("?".into(), |t| date::format(t / 86_400)),
                p.donor
                    .as_ref()
                    .map_or(String::new(), |d| format!(", of donor {d}")),
                p.version
                    .as_ref()
                    .map_or(String::new(), |v| format!(", version {v}"))
            )
        })
        .collect();
    println!("  pinned in: {}", inputs.join(", "));
    for pin in failures {
        println!("  reason:    {}", reason_words(pin.reason));
        println!("  since:     {}", pin.since);
        if let Some(tried) = &pin.tried {
            println!("  tried:     {tried}");
        }
        let retry = ctx
            .config
            .as_ref()
            .map_or(7, |c| c.retry_same_derivation_after_days);
        if pin.reason == Reason::Build
            && pin.root_drv.is_some()
            && retry > 0
            && let Some(last) = date::parse(pin.tried.as_deref().unwrap_or(&pin.since))
        {
            println!(
                "  retried:   once its derivation changes, or from {}",
                date::format(last + retry)
            );
        }
        if let Some(error) = &pin.error {
            println!("  error:     {error}");
        }
        if let Some(drv) = &pin.failed_drv {
            println!("  failed:    {drv}");
        }
        if !pin.trail.is_empty() {
            println!("  trail:     {}", pin.trail.join(" -> "));
        }
        if let Some(drv) = &pin.failed_drv
            && let Some(log) = nix::log_tail(drv, 20)
        {
            println!("  build log (last lines):");
            for line in log.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn pin(
    at: &Where,
    pick: &Pick,
    attr: &str,
    input: Option<&str>,
    old_lock: Option<&Path>,
) -> Result<ExitCode> {
    if !crate::resolve::is_attr_name(attr) {
        bail!("{attr:?} is not a top-level attribute name");
    }
    let ctx = Ctx::open(at, false)?;
    ctx.announce();
    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;
    let target = ctx.target(pick, None)?;
    // The default is the target's own input, which may differ from this
    // machine's.
    let (input, from) = match input {
        Some(i) => (i.to_string(), "--input"),
        None => (
            Nix::new(&ctx.flake)
                .eval_raw(&format!("{}.input", target.eppur_config()))
                .ok()
                .or_else(|| ctx.config.as_ref().and_then(|c| c.input.clone()))
                .unwrap_or_else(|| "nixpkgs".to_string()),
            "programs.eppur.input",
        ),
    };
    let input = input.as_str();
    let lock = Lock::load(&ctx.flake.join("flake.lock"))?;
    let Some(current) = lock.locked(input) else {
        let mut known = lock.inputs();
        known.sort();
        bail!(
            "{input} (from {from}) is not an input of flake.lock (its inputs: {}){}",
            known.join(", "),
            if from == "--input" {
                ""
            } else {
                "; pass --input or set programs.eppur.input"
            }
        );
    };
    let locked = match old_lock {
        Some(path) => Lock::load(path)?
            .locked(input)
            .with_context(|| format!("{input} is not an input of {}", path.display()))?,
        None => current,
    };
    let checked = lock.nar_hash(input).unwrap_or_default();
    let system = Nix::new(&ctx.flake)
        .eval_raw(&target.system_attr())
        .unwrap_or_else(|_| default_system());
    if !crate::resolve::evaluates_at(&locked, &system, attr)? {
        bail!(
            "{attr} is not a package in {input} at {}; nothing was pinned",
            engine::describe(&locked)
        );
    }
    let pins_path = ctx.pins_path();
    ensure_pins(&pins_path, Git::open(&ctx.flake).as_ref(), false)?;
    let mut file = PinsFile::load(&pins_path)?;
    let overlays = engine::choose_overlays(&Nix::new(&ctx.flake), &target, input, attr, &locked);
    let at_rev = engine::describe_dated(&locked);
    let entry = file.entry_mut(target.kind, &target.name);
    if let Some(old) = entry.get(input, attr) {
        log::change(&format!(
            "{target}: {attr} ({input}) has been {} since {}; re-pinning it by hand",
            if old.reason == Reason::Manual {
                "pinned by hand".to_string()
            } else {
                format!("pinned because it {}", reason_words(old.reason))
            },
            old.since
        ));
    }
    entry.insert(
        input,
        attr,
        Pin {
            locked,
            since: date::format(date::today()),
            reason: Reason::Manual,
            failed_drv: None,
            trail: Vec::new(),
            error: None,
            overlays,
            checked_nar_hash: checked,
            root_drv: None,
            tried: None,
            donor: None,
            leaf: false,
            version: None,
        },
    );
    file.save(&pins_path)?;
    log::change(&format!(
        "pinned {target}: {attr} ({input}) at {at_rev} by hand"
    ));
    Ok(ExitCode::SUCCESS)
}

/// The inputs `attr` is pinned in on `target`, or the one `input` names;
/// an error naming where it is pinned instead.
fn pinned_inputs(
    ctx: &Ctx,
    file: &PinsFile,
    target: &Target,
    attr: &str,
    input: Option<&str>,
) -> Result<Vec<String>> {
    let pinned = file
        .entry(target.kind, &target.name)
        .map(|e| e.inputs_pinning(attr))
        .unwrap_or_default();
    if pinned.is_empty() {
        bail!(
            "{attr} is not pinned on {target}{}",
            ctx.pinned_elsewhere(file, target, attr)
        );
    }
    match input {
        Some(i) if !pinned.iter().any(|p| p == i) => bail!(
            "{attr} is not pinned in {i} on {target} (it is pinned in {})",
            pinned.join(", ")
        ),
        Some(i) => Ok(vec![i.to_string()]),
        None => Ok(pinned),
    }
}

pub fn retry(at: &Where, pick: &Pick, attr: &str, input: Option<&str>) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    ctx.announce();
    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;
    let pins_path = ctx.pins_path();
    let mut file = PinsFile::load(&pins_path)?;
    let target = ctx.target(pick, Some(&file))?;
    let inputs = pinned_inputs(&ctx, &file, &target, attr, input)?;
    let entry = file.entry_mut(target.kind, &target.name);
    let manual: Vec<&String> = inputs
        .iter()
        .filter(|i| {
            entry
                .get(i, attr)
                .is_some_and(|p| p.reason == Reason::Manual)
        })
        .collect();
    if !manual.is_empty() {
        bail!(
            "{attr} ({}) on {target} is pinned by hand and never retried; remove it with 'eppur clear {attr}{}'",
            manual
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            ctx.flag_for(&target)
        );
    }
    for i in &inputs {
        if let Some(pin) = entry.pins.get_mut(i).and_then(|s| s.get_mut(attr)) {
            pin.checked_nar_hash.clear();
        }
    }
    file.save(&pins_path)?;
    log::change(&format!(
        "{target}: retry queued for {attr} ({})",
        inputs.join(", ")
    ));
    Ok(ExitCode::SUCCESS)
}

pub fn clear_input(at: &Where, input: &str) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    ctx.announce();
    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;
    let pins_path = ctx.pins_path();
    let mut file = PinsFile::load(&pins_path)?;
    if file.inputs.remove(input).is_none() {
        bail!("flake input {input} is not pinned");
    }
    file.touch_inputs();
    file.save(&pins_path)?;
    log::change(&format!("unpinned input {input}"));
    Ok(ExitCode::SUCCESS)
}

pub fn clear(
    at: &Where,
    pick: &Pick,
    attr: Option<&str>,
    input: Option<&str>,
    held: bool,
) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    ctx.announce();
    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;
    let pins_path = ctx.pins_path();
    let mut file = PinsFile::load(&pins_path)?;
    let target = ctx.target(pick, Some(&file))?;
    if held {
        if file
            .entry_mut(target.kind, &target.name)
            .held
            .take()
            .is_none()
        {
            bail!("{target} is not held");
        }
        file.save(&pins_path)?;
        log::change(&format!("released {target}"));
        return Ok(ExitCode::SUCCESS);
    }
    let Some(attr) = attr else {
        bail!("name an attr to clear, or pass --held");
    };
    let inputs = pinned_inputs(&ctx, &file, &target, attr, input)?;
    let entry = file.entry_mut(target.kind, &target.name);
    for i in &inputs {
        entry.remove(i, attr);
    }
    file.save(&pins_path)?;
    log::change(&format!(
        "unpinned {target}: {attr} ({})",
        inputs.join(", ")
    ));
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(
            parse_duration("12h"),
            Ok(std::time::Duration::from_secs(43_200))
        );
        assert_eq!(
            parse_duration("45min"),
            Ok(std::time::Duration::from_secs(2_700))
        );
        assert_eq!(
            parse_duration("30m"),
            Ok(std::time::Duration::from_secs(1_800))
        );
        assert_eq!(
            parse_duration("90s"),
            Ok(std::time::Duration::from_secs(90))
        );
        assert_eq!(
            parse_duration("1d"),
            Ok(std::time::Duration::from_secs(86_400))
        );
        assert_eq!(
            parse_duration("3600"),
            Ok(std::time::Duration::from_secs(3_600))
        );
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("1h30min").is_err());
    }
}
