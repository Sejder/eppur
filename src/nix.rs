use crate::graph::Graph;
use crate::log;
use crate::logparse;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

/// How many evaluations run at once: each holds a whole system in memory.
const MAX_PARALLEL_EVALS: usize = 4;

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

/// Stop here when the run is being interrupted or is past its --timeout.
pub fn check_stopped() -> Result<()> {
    crate::guard::halt_if_interrupted();
    if crate::guard::past_deadline() {
        bail!("stopped: the run took longer than --timeout");
    }
    Ok(())
}

/// Start a nix command so an interrupt or --timeout stops it too. Nothing new
/// starts past the deadline: the deadline only stops what runs when it passes.
fn spawn(cmd: &mut Command, what: &str) -> Result<Child> {
    check_stopped()?;
    let mut child = cmd.spawn().with_context(|| format!("spawning {what}"))?;
    crate::guard::track(child.id());
    // Stopping may have started just before it was tracked, and missed it.
    if crate::guard::past_deadline() || crate::guard::interrupted() {
        let _ = child.kill();
        let _ = child.wait();
        crate::guard::untrack(child.id());
        check_stopped()?;
    }
    Ok(child)
}

/// After a command from `spawn` ended, however it ended.
fn finished(id: u32) -> Result<()> {
    crate::guard::untrack(id);
    check_stopped()
}

/// Run a nix command to its end and collect its output.
fn run(mut cmd: Command, what: &str) -> Result<Output> {
    let child = spawn(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
        what,
    )?;
    let id = child.id();
    let out = child.wait_with_output();
    finished(id)?;
    out.with_context(|| format!("waiting for {what}"))
}

/// Run a nix command with internal-json logging, handing each line of its
/// stderr to `on_line`, and collect its stdout and error messages.
fn run_logged(
    mut cmd: Command,
    mut on_line: impl FnMut(&[u8]),
) -> Result<(ExitStatus, Vec<u8>, Vec<String>)> {
    let mut child = spawn(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
        "nix",
    )?;
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
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end();
        if logparse::is_ignored_error(line) {
            continue;
        }
        on_line(&buf);
        if let Some(msg) = logparse::error_message(line) {
            errors.push(msg);
        }
    }

    let status = child.wait().context("waiting for nix");
    finished(child.id())?;
    let status = status?;
    let out = collect.join().expect("stdout reader")?;
    Ok((status, out, errors))
}

/// `f` over `items` with at most MAX_PARALLEL_EVALS at once, results in the
/// order of `items`.
pub fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if items.len() < 2 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let done = std::sync::Mutex::new(Vec::with_capacity(items.len()));
    std::thread::scope(|scope| {
        for _ in 0..MAX_PARALLEL_EVALS.min(items.len()) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let result = f(item);
                    done.lock().expect("a worker panicked").push((i, result));
                }
            });
        }
    });
    let mut done = done.into_inner().expect("a worker panicked");
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

/// A nix string literal for `text`, whatever it contains.
pub fn nix_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A nix expression for a JSON value.
pub fn nix_json(value: &Value) -> Result<String> {
    Ok(format!(
        "(builtins.fromJSON {})",
        nix_string(&serde_json::to_string(value)?)
    ))
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
    fn run(&mut self, cmd: Command) -> Result<(ExitStatus, Vec<u8>, Vec<String>)> {
        run_logged(cmd, |line| self.line(line, true))
    }

    /// A line of a nix command's stderr: for nom on a terminal, else its
    /// progress and, when `errors`, its error messages go to stderr.
    fn line(&mut self, line: &[u8], errors: bool) {
        if let Some(pipe) = &mut self.pipe
            && pipe.write_all(line).is_err()
        {
            self.pipe = None;
        }
        if self.active() {
            return;
        }
        let text = String::from_utf8_lossy(line);
        if let Some(progress) = logparse::activity_text(text.trim_end()) {
            log::nix_progress(&progress);
        }
        if errors && let Some(msg) = logparse::error_message(text.trim_end()) {
            log::nix_error(&msg);
        }
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
        let mut cmd = command();
        cmd.args(["flake", "update", "--flake", &self.flake])
            .args(inputs)
            .stdin(Stdio::null())
            .stderr(if quiet {
                Stdio::piped()
            } else {
                Stdio::inherit()
            });
        let mut child = spawn(&mut cmd, "nix flake update")?;
        let mut stderr = Vec::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_end(&mut stderr);
        }
        let status = child.wait().context("waiting for nix flake update");
        finished(child.id())?;
        if !status?.success() {
            if quiet {
                eprint!("{}", String::from_utf8_lossy(&stderr));
            }
            bail!("nix flake update failed");
        }
        Ok(())
    }

    /// Lock these inputs at these flake references, leaving every other input
    /// where it is.
    pub fn lock_override(&self, inputs: &[(String, String)]) -> Result<()> {
        let mut cmd = command();
        cmd.args(["flake", "lock", &self.flake]);
        for (name, flake_ref) in inputs {
            cmd.args(["--override-input", name, flake_ref]);
        }
        checked("nix flake lock", run(cmd, "nix flake lock")?)?;
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
        let out = run(cmd, "nix eval")?;
        if out.status.success() {
            Ok(Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()))
        } else {
            Ok(Err(logparse::strip_ansi(&String::from_utf8_lossy(
                &out.stderr,
            ))))
        }
    }

    /// The names in a flake output such as nixosConfigurations, none when the
    /// flake has no such output; None when it does not evaluate.
    pub fn output_names(&self, output: &str) -> Option<Vec<String>> {
        match self.eval_apply(output, "builtins.attrNames") {
            Ok(names) => serde_json::from_value(names).ok(),
            Err(e) if e.to_string().contains("does not provide attribute") => Some(Vec::new()),
            Err(_) => None,
        }
    }

    /// Whether these derivations build. Not an error when they do not: this
    /// is a test, and its failures are not reported as the run's.
    pub fn builds(&self, drvs: &[String]) -> Result<bool> {
        let mut cmd = command();
        cmd.args(["build", "--no-link"]);
        cmd.args(drvs.iter().map(|d| format!("{d}^*")));
        Ok(run(cmd, "nix build")?.status.success())
    }

    /// The drvPath of each of `attrs`, or its evaluation error messages, in
    /// the order of `attrs`. They evaluate side by side, on one nom view. Off a
    /// terminal a command's errors are printed when it ends, so that those of
    /// different targets do not run together.
    pub fn drv_paths(
        &self,
        attrs: &[String],
        monitor: &mut Monitor,
    ) -> Result<Vec<Result<String, Vec<String>>>> {
        enum Event {
            Line(Vec<u8>),
            Errors(Vec<String>),
        }
        let (send, receive) = mpsc::channel();
        let evaluate = |attr: &String, send: &mpsc::Sender<Event>| {
            let mut cmd = command();
            cmd.args(["eval", "--raw", "--log-format", "internal-json"])
                .arg(self.installable(&format!("{attr}.drvPath")));
            let (status, out, errors) = run_logged(cmd, |line| {
                let _ = send.send(Event::Line(line.to_vec()));
            })?;
            let _ = send.send(Event::Errors(errors.clone()));
            Ok(if status.success() {
                Ok(String::from_utf8_lossy(&out).trim().to_string())
            } else if errors.is_empty() {
                Err(vec![format!("nix eval {attr} failed")])
            } else {
                Err(errors)
            })
        };
        std::thread::scope(|scope| {
            let workers = scope.spawn(move || {
                parallel_map(attrs, |attr| evaluate(attr, &send.clone()))
                    .into_iter()
                    .collect::<Result<Vec<_>>>()
            });
            for event in receive {
                match event {
                    Event::Line(line) => monitor.line(&line, false),
                    Event::Errors(errors) if !monitor.active() => {
                        for msg in &errors {
                            log::nix_error(msg);
                        }
                    }
                    Event::Errors(_) => {}
                }
            }
            workers.join().expect("an evaluation thread panicked")
        })
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
        let mut cmd = command();
        cmd.args(["derivation", "show", "--recursive"]).args(drvs);
        let out = run(cmd, "nix derivation show")?;
        let store = crate::graph::store_dir_of(drvs.first().map_or("", String::as_str));
        Graph::from_show_json(&checked("nix derivation show", out)?, &store)
    }

    pub fn eval_apply(&self, attr: &str, apply: &str) -> Result<Value> {
        let mut cmd = command();
        cmd.args(["eval", "--json"])
            .arg(self.installable(attr))
            .args(["--apply", apply]);
        let bytes = checked(&format!("nix eval {attr}"), run(cmd, "nix eval")?)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// The store path of the flake's source as nix copies it now.
    pub fn source_path(&self) -> Result<String> {
        let mut cmd = command();
        cmd.args(["flake", "metadata", "--json", &self.flake]);
        let out = run(cmd, "nix flake metadata")?;
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
        let mut cmd = command();
        cmd.args(["eval", "--raw", "--show-trace"])
            .arg(self.installable(attr));
        let out = run(cmd, "nix eval")?;
        Ok(logparse::strip_ansi(&String::from_utf8_lossy(&out.stderr)))
    }

    pub fn eval_raw(&self, attr: &str) -> Result<String> {
        let mut cmd = command();
        cmd.args(["eval", "--raw"]).arg(self.installable(attr));
        let bytes = checked(&format!("nix eval {attr}"), run(cmd, "nix eval")?)?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_string())
    }
}

/// Every derivation `drv` depends on, itself included, without parsing the
/// derivations themselves.
pub fn drv_closure(drv: &str) -> Result<std::collections::HashSet<String>> {
    let mut cmd = Command::new("nix-store");
    cmd.args(["--query", "--requisites", drv]);
    let out = checked("nix-store --query", run(cmd, "nix-store --query")?)?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter(|p| p.ends_with(".drv"))
        .map(str::to_string)
        .collect())
}

/// The store paths of these locked inputs' sources, in their order.
pub fn source_paths(locked: &[Value]) -> Result<Vec<String>> {
    let expr = format!(
        "map (l: (builtins.fetchTree l).outPath) {}",
        nix_json(&Value::from(locked.to_vec()))?
    );
    Ok(serde_json::from_value(eval_expr(&expr)?)?)
}

/// Keep a built derivation alive with a GC root at `link`.
pub fn add_root(drv: &str, link: &Path) -> Result<()> {
    let mut cmd = command();
    cmd.args(["build", "--out-link"])
        .arg(link)
        .arg(format!("{drv}^out"));
    checked(&format!("rooting {drv}"), run(cmd, "nix build")?).map(|_| ())
}

/// Root a path that is already in the store under `link`.
pub fn add_path_root(path: &Path, link: &Path) -> Result<()> {
    let mut cmd = command();
    cmd.args(["build", "--out-link"]).arg(link).arg(path);
    checked(
        &format!("rooting {}", path.display()),
        run(cmd, "nix build")?,
    )
    .map(|_| ())
}

/// Whether every output of `drv` is in the store, i.e. it really built.
pub fn is_built(drv: &str) -> Result<bool> {
    let mut cmd = command();
    cmd.args(["path-info", "--offline"]).arg(format!("{drv}^*"));
    Ok(run(cmd, "nix path-info")?.status.success())
}

/// The `out` path of a built derivation, read from the drv itself.
pub fn out_path(drv: &str) -> Result<String> {
    let mut cmd = command();
    cmd.args(["derivation", "show", drv]);
    let json: Value = serde_json::from_slice(&checked(
        "nix derivation show",
        run(cmd, "nix derivation show")?,
    )?)?;
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
        let mut cmd = command();
        cmd.args(["config", "show", "substituters"]);
        match run(cmd, "nix config show") {
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
        cmd.arg(out);
        run(cmd, "nix path-info").is_ok_and(|o| o.status.success())
    };
    valid(None) || substituters().iter().any(|s| valid(Some(s)))
}

/// A flake.lock node's `locked` as a flake reference for the command line.
pub fn flake_ref(locked: &Value) -> Result<String> {
    let expr = format!("builtins.flakeRefToString {}", nix_json(locked)?);
    eval_expr(&expr)?
        .as_str()
        .map(str::to_string)
        .context("builtins.flakeRefToString did not return a string")
}

pub fn eval_expr(expr: &str) -> Result<Value> {
    let mut cmd = command();
    cmd.args(["eval", "--json", "--expr", expr]);
    let bytes = checked("nix eval --expr", run(cmd, "nix eval")?)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// The last lines of a derivation's build log, if nix kept one.
pub fn log_tail(drv: &str, lines: usize) -> Option<String> {
    let mut cmd = command();
    cmd.args(["log", drv]);
    let out = run(cmd, "nix log").ok()?;
    if !out.status.success() {
        return None;
    }
    let text = logparse::strip_ansi(&String::from_utf8_lossy(&out.stdout));
    let kept: Vec<&str> = text.lines().filter(|l| !l.starts_with("@nix ")).collect();
    Some(kept[kept.len().saturating_sub(lines)..].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nix_strings_escape_what_nix_would_read() {
        assert_eq!(nix_string("plain"), r#""plain""#);
        assert_eq!(nix_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(nix_string("${x} ''"), r#""\${x} ''""#);
    }

    #[test]
    fn parallel_map_keeps_the_order() {
        let items: Vec<usize> = (0..20).collect();
        let doubled = parallel_map(&items, |i| i * 2);
        assert_eq!(doubled, items.iter().map(|i| i * 2).collect::<Vec<_>>());
    }
}
