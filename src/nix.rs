use crate::graph::Graph;
use crate::log;
use crate::logparse;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};

pub struct Nix {
    flake: String,
}

pub struct BuildOutcome {
    pub success: bool,
    pub errors: Vec<String>,
}

fn command() -> Command {
    let mut c = Command::new("nix");
    c.args(["--extra-experimental-features", "nix-command flakes"]);
    c.args(["--option", "warn-dirty", "false"]);
    c
}

fn tail(stderr: &[u8], lines: usize) -> String {
    let text = logparse::strip_ansi(&String::from_utf8_lossy(stderr));
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn checked(what: &str, out: Output) -> Result<Vec<u8>> {
    if !out.status.success() {
        bail!("{what} failed:\n{}", tail(&out.stderr, 20));
    }
    Ok(out.stdout)
}

/// One nix-output-monitor view on a terminal, fed by every nix command of a
/// round, so evaluation and build show as one session the way nh shows them.
/// Off a terminal, or with --quiet, there is none, and error messages go to
/// stderr instead, where the journal picks them up.
pub struct Monitor {
    nom: Option<Child>,
    pipe: Option<ChildStdin>,
}

impl Monitor {
    pub fn start() -> Self {
        let nom = (std::io::stderr().is_terminal() && !log::is_quiet())
            .then(|| {
                Command::new("nom")
                    .arg("--json")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .ok()
            })
            .flatten();
        let mut m = Self { nom, pipe: None };
        m.pipe = m.nom.as_mut().and_then(|n| n.stdin.take());
        m
    }

    fn active(&self) -> bool {
        self.pipe.is_some()
    }

    /// Run `cmd` with internal-json logging, feeding its stderr to nom and
    /// collecting its error messages and stdout.
    fn run(&mut self, mut cmd: Command) -> Result<(ExitStatus, Vec<u8>, Vec<String>)> {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning nix")?;
        crate::guard::track(child.id());

        let mut stdout = child.stdout.take().expect("piped stdout");
        let collect = std::thread::spawn(move || {
            let mut out = Vec::new();
            stdout.read_to_end(&mut out).map(|_| out)
        });

        let mut reader = BufReader::new(child.stderr.take().expect("piped stderr"));
        let mut errors = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                break;
            }
            if let Some(pipe) = &mut self.pipe
                && pipe.write_all(&buf).is_err()
            {
                self.pipe = None;
            }
            let line = String::from_utf8_lossy(&buf);
            if !self.active()
                && let Some(text) = logparse::activity_text(line.trim_end())
            {
                log::nix_progress(&text);
            }
            if let Some(msg) = logparse::error_message(line.trim_end()) {
                if !self.active() {
                    log::nix_error(&msg);
                }
                errors.push(msg);
            }
        }

        let status = child.wait().context("waiting for nix");
        crate::guard::untrack(child.id());
        crate::guard::halt_if_interrupted();
        if crate::guard::timed_out() {
            bail!("stopped: the run took longer than --timeout");
        }
        let status = status?;
        let out = collect.join().expect("stdout reader")?;
        Ok((status, out, errors))
    }

    pub fn finish(mut self) {
        self.pipe = None;
        if let Some(mut n) = self.nom.take() {
            let _ = n.wait();
        }
    }
}

impl Nix {
    pub fn new(flake: &Path) -> Self {
        Self {
            flake: flake.display().to_string(),
        }
    }

    fn installable(&self, attr: &str) -> String {
        format!("{}#{}", self.flake, attr)
    }

    /// Update the lock. Under --quiet its list of updated inputs is kept back
    /// unless the update fails.
    pub fn flake_update(&self, inputs: &[String]) -> Result<()> {
        let quiet = log::is_quiet();
        let mut child = command()
            .args(["flake", "update", "--flake", &self.flake])
            .args(inputs)
            .stdin(Stdio::null())
            .stderr(if quiet {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .spawn()
            .context("spawning nix flake update")?;
        crate::guard::track(child.id());
        let mut stderr = Vec::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_end(&mut stderr);
        }
        let status = child.wait().context("waiting for nix flake update");
        crate::guard::untrack(child.id());
        crate::guard::halt_if_interrupted();
        if crate::guard::timed_out() {
            bail!("stopped: the run took longer than --timeout");
        }
        if !status?.success() {
            if quiet {
                eprint!("{}", String::from_utf8_lossy(&stderr));
            }
            bail!("nix flake update failed");
        }
        Ok(())
    }

    /// Run a nix command so an interrupt or --timeout stops it too.
    fn tracked(&self, mut cmd: Command, what: &str) -> Result<Output> {
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {what}"))?;
        let id = child.id();
        crate::guard::track(id);
        let out = child.wait_with_output();
        crate::guard::untrack(id);
        crate::guard::halt_if_interrupted();
        if crate::guard::timed_out() {
            bail!("stopped: the run took longer than --timeout");
        }
        out.with_context(|| format!("waiting for {what}"))
    }

    /// Lock these inputs at these flake references, leaving every other input
    /// where it is.
    pub fn lock_override(&self, inputs: &[(String, String)]) -> Result<()> {
        let mut cmd = command();
        cmd.args(["flake", "lock", &self.flake]);
        for (name, flake_ref) in inputs {
            cmd.args(["--override-input", name, flake_ref]);
        }
        checked("nix flake lock", self.tracked(cmd, "nix flake lock")?)?;
        Ok(())
    }

    /// The drv of `attr` as flake.lock has it, or with one input at another
    /// revision, without touching flake.lock; Err with nix's message when it
    /// does not evaluate.
    pub fn drv_path_with(
        &self,
        attr: &str,
        input: Option<(&str, &str)>,
    ) -> Result<Result<String, String>> {
        let mut cmd = command();
        cmd.args(["eval", "--raw", "--no-write-lock-file"]);
        if let Some((name, flake_ref)) = input {
            cmd.args(["--override-input", name, flake_ref]);
        }
        cmd.arg(self.installable(&format!("{attr}.drvPath")));
        let out = self.tracked(cmd, "nix eval")?;
        if out.status.success() {
            Ok(Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()))
        } else {
            Ok(Err(logparse::strip_ansi(&String::from_utf8_lossy(
                &out.stderr,
            ))))
        }
    }

    /// Every host and home the flake has, e.g. "host a", or None when its
    /// outputs do not evaluate.
    pub fn flake_targets(&self) -> Option<Vec<String>> {
        let mut targets = Vec::new();
        for (noun, output) in [
            ("host", "nixosConfigurations"),
            ("home", "homeConfigurations"),
        ] {
            match self.eval_apply(output, "builtins.attrNames") {
                Ok(names) => {
                    let names: Vec<String> = serde_json::from_value(names).ok()?;
                    targets.extend(names.iter().map(|n| format!("{noun} {n}")));
                }
                Err(e) if e.to_string().contains("does not provide attribute") => {}
                Err(_) => return None,
            }
        }
        Some(targets)
    }

    /// Whether these derivations build. Not an error when they do not: this
    /// is a test, and its failures are not reported as the run's.
    pub fn builds(&self, drvs: &[String]) -> Result<bool> {
        let mut cmd = command();
        cmd.args(["build", "--no-link"]);
        cmd.args(drvs.iter().map(|d| format!("{d}^*")));
        Ok(self.tracked(cmd, "nix build")?.status.success())
    }

    /// The drvPath of `attr`, or the evaluation error messages.
    pub fn drv_path(
        &self,
        attr: &str,
        monitor: &mut Monitor,
    ) -> Result<Result<String, Vec<String>>> {
        let mut cmd = command();
        cmd.args(["eval", "--raw", "--log-format", "internal-json"])
            .arg(self.installable(&format!("{attr}.drvPath")));
        let (status, out, errors) = monitor.run(cmd)?;
        if status.success() {
            Ok(Ok(String::from_utf8_lossy(&out).trim().to_string()))
        } else if errors.is_empty() {
            Ok(Err(vec![format!("nix eval {attr} failed")]))
        } else {
            Ok(Err(errors))
        }
    }

    /// Build already evaluated derivations in one `--keep-going` run.
    pub fn build_drvs(&self, drvs: &[String], monitor: &mut Monitor) -> Result<BuildOutcome> {
        let mut cmd = command();
        cmd.args([
            "build",
            "--no-link",
            "--keep-going",
            "--log-format",
            "internal-json",
        ]);
        if monitor.active() {
            cmd.arg("-v");
        }
        cmd.args(drvs.iter().map(|d| format!("{d}^*")));
        let (status, _, errors) = monitor.run(cmd)?;
        Ok(BuildOutcome {
            success: status.success(),
            errors,
        })
    }

    pub fn show_drvs(&self, drvs: &[String]) -> Result<Graph> {
        let out = command()
            .args(["derivation", "show", "--recursive"])
            .args(drvs)
            .stdin(Stdio::null())
            .output()
            .context("spawning nix derivation show")?;
        let store = crate::graph::store_dir_of(drvs.first().map_or("", String::as_str));
        Graph::from_show_json(&checked("nix derivation show", out)?, &store)
    }

    pub fn eval_apply(&self, attr: &str, apply: &str) -> Result<Value> {
        let out = command()
            .args(["eval", "--json"])
            .arg(self.installable(attr))
            .args(["--apply", apply])
            .stdin(Stdio::null())
            .output()
            .context("spawning nix eval")?;
        let bytes = checked(&format!("nix eval {attr}"), out)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// The store path of the flake's source as nix copies it now.
    pub fn source_path(&self) -> Result<String> {
        let out = command()
            .args(["flake", "metadata", "--json", &self.flake])
            .stdin(Stdio::null())
            .output()
            .context("spawning nix flake metadata")?;
        let json: Value = serde_json::from_slice(&checked("nix flake metadata", out)?)?;
        json["path"]
            .as_str()
            .map(str::to_string)
            .context("nix flake metadata has no path")
    }

    /// The full error of evaluating `attr`, with every frame of its trace:
    /// what nix shows by default is cut short before the packages an error
    /// passed through. Not shown to the user.
    pub fn eval_trace(&self, attr: &str) -> Result<String> {
        let out = command()
            .args(["eval", "--raw", "--show-trace"])
            .arg(self.installable(attr))
            .stdin(Stdio::null())
            .output()
            .context("spawning nix eval")?;
        Ok(logparse::strip_ansi(&String::from_utf8_lossy(&out.stderr)))
    }

    pub fn eval_raw(&self, attr: &str) -> Result<String> {
        let out = command()
            .args(["eval", "--raw"])
            .arg(self.installable(attr))
            .stdin(Stdio::null())
            .output()
            .context("spawning nix eval")?;
        let bytes = checked(&format!("nix eval {attr}"), out)?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_string())
    }
}

/// Keep a built derivation alive with a GC root at `link`.
pub fn add_root(drv: &str, link: &Path) -> Result<()> {
    let out = command()
        .args(["build", "--out-link"])
        .arg(link)
        .arg(format!("{drv}^out"))
        .stdin(Stdio::null())
        .output()
        .context("spawning nix build")?;
    checked(&format!("rooting {drv}"), out).map(|_| ())
}

/// Whether every output of `drv` is in the store, i.e. it really built.
pub fn is_built(drv: &str) -> Result<bool> {
    let out = command()
        .args(["path-info", "--offline"])
        .arg(format!("{drv}^*"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawning nix path-info")?;
    Ok(out.success())
}

/// The `out` path of a built derivation, read from the drv itself.
pub fn out_path(drv: &str) -> Result<String> {
    let out = command()
        .args(["derivation", "show", drv])
        .stdin(Stdio::null())
        .output()
        .context("spawning nix derivation show")?;
    let json: Value = serde_json::from_slice(&checked("nix derivation show", out)?)?;
    let drvs = json.get("derivations").unwrap_or(&json);
    let path = drvs
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(|d| d["outputs"]["out"]["path"].as_str())
        .with_context(|| format!("{drv} has no out path"))?;
    Ok(if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{path}", crate::graph::store_dir_of(drv))
    })
}

/// The substituters nix uses here, as store URLs.
fn substituters() -> &'static [String] {
    static SUBSTITUTERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    SUBSTITUTERS.get_or_init(|| {
        let out = command()
            .args(["config", "show", "substituters"])
            .stdin(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    })
}

/// Whether `out` is in the store already or one of the substituters has it,
/// so that using it builds nothing.
pub fn available(out: &str) -> bool {
    let valid = |store: Option<&str>| {
        let mut cmd = command();
        cmd.arg("path-info");
        if let Some(s) = store {
            cmd.args(["--store", s]);
        }
        cmd.arg(out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    valid(None) || substituters().iter().any(|s| valid(Some(s)))
}

/// A flake.lock node's `locked` as a flake reference for the command line.
pub fn flake_ref(locked: &Value) -> Result<String> {
    let json = serde_json::to_string(locked)?;
    if json.contains("''") || json.contains("${") {
        bail!("locked node cannot be quoted safely");
    }
    let expr = format!("builtins.flakeRefToString (builtins.fromJSON ''{json}'')");
    eval_expr(&expr)?
        .as_str()
        .map(str::to_string)
        .context("builtins.flakeRefToString did not return a string")
}

pub fn eval_expr(expr: &str) -> Result<Value> {
    let out = command()
        .args(["eval", "--json", "--expr", expr])
        .stdin(Stdio::null())
        .output()
        .context("spawning nix eval")?;
    let bytes = checked("nix eval --expr", out)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// The last lines of a derivation's build log, if nix kept one.
pub fn log_tail(drv: &str, lines: usize) -> Option<String> {
    let out = command()
        .args(["log", drv])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = logparse::strip_ansi(&String::from_utf8_lossy(&out.stdout));
    let kept: Vec<&str> = text.lines().filter(|l| !l.starts_with("@nix ")).collect();
    Some(kept[kept.len().saturating_sub(lines)..].join("\n"))
}
