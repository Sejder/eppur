//! tack's side of the lock. Its pins live in .tack/pins.toml and are locked
//! in .tack/pins.lock.json, which eppur reads as a flake.lock with one root
//! input per pin. eppur reads tack's files itself and runs the tack command
//! only to change them: to update, and to freeze a pin it holds back where a
//! plain `tack update` leaves it, and unfreeze it once it builds again.

use crate::lock::Lock;
use crate::locker;
use crate::log;
use crate::pins::{PinsFile, write_atomic};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// The attributes of a lock entry fetchTree takes, as tack's own resolver
/// keeps them; the rest (public, patched, signedBy, tag, version) are tack's.
const FETCH_TREE_ATTRS: &[&str] = &[
    "type",
    "owner",
    "repo",
    "host",
    "url",
    "id",
    "ref",
    "rev",
    "narHash",
    "lastModified",
    "revCount",
    "submodules",
    "shallow",
    "allRefs",
    "name",
    "lfs",
    "exportIgnore",
    "verifyCommit",
    "keytype",
    "publicKey",
    "publicKeys",
    "dirtyRev",
    "dirtyShortRev",
    "unpack",
    "treeHash",
    "path",
];

/// The root node of the flake.lock a pins.lock.json is read as; `$` keeps it
/// apart from pin names.
pub const ROOT: &str = "$root";
const DECLARED: &str = "$declared";

/// Lock entries seen in this run, by pin and narHash, as tack wrote them:
/// putting a pin back to a revision writes the entry tack wrote for it, with
/// its signer, tag and patched tree and in tack's own layout, so a lock put
/// back where it was is the same file again.
static SEEN: Mutex<Option<HashMap<(String, String), String>>> = Mutex::new(None);

/// The top-level entries of a JSON object in file order, each as its text.
struct Ordered(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Entries;
        impl<'de> serde::de::Visitor<'de> for Entries {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Ordered, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, Box<RawValue>>()? {
                    entries.push(entry);
                }
                Ok(Ordered(entries))
            }
        }
        d.deserialize_map(Entries)
    }
}

fn ordered(bytes: &[u8]) -> Result<Vec<(String, String)>> {
    let Ordered(entries) = serde_json::from_slice(bytes)?;
    Ok(entries
        .into_iter()
        .map(|(k, v)| (k, v.get().to_string()))
        .collect())
}

/// Remember the pin entries of a pins.lock.json read in this run.
pub fn remember(bytes: &[u8]) {
    let Ok(entries) = ordered(bytes) else {
        return;
    };
    let Ok(mut seen) = SEEN.lock() else {
        return;
    };
    let seen = seen.get_or_insert_with(HashMap::new);
    for (name, text) in entries.into_iter().filter(|(n, _)| is_pin(n)) {
        let hash = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["narHash"].as_str().map(str::to_string));
        if let Some(hash) = hash {
            seen.insert((name, hash), text);
        }
    }
}

fn seen(name: &str, locked: &Value) -> Option<String> {
    let hash = locked["narHash"].as_str()?;
    SEEN.lock()
        .ok()?
        .as_ref()?
        .get(&(name.to_string(), hash.to_string()))
        .cloned()
}

/// What of a lock entry fetchTree takes.
pub fn strip(entry: &Value) -> Value {
    let Some(obj) = entry.as_object() else {
        return entry.clone();
    };
    Value::Object(
        obj.iter()
            .filter(|(k, _)| FETCH_TREE_ATTRS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

/// Whether a lock key names a pin, not one of tack's own entries.
fn is_pin(name: &str) -> bool {
    !name.starts_with('$') && !name.starts_with('_')
}

/// A pins.lock.json read as a flake.lock, or None when `json` is not one.
/// Each pin is a root input whose node's `locked` is what fetchTree takes and
/// whose `original` is the url pins.toml declares, so a pin that moved to
/// another url is not rolled back across it. A pin that only follows
/// another's input (it has no url of its own) is marked so.
pub fn as_flake_lock(json: &Value) -> Option<Value> {
    let obj = json.as_object()?;
    if obj.contains_key("nodes") || obj.contains_key("version") {
        return None;
    }
    let pins: Vec<(&String, &Value)> = obj.iter().filter(|(k, _)| is_pin(k)).collect();
    if !pins.iter().all(|(_, v)| v["type"].is_string()) {
        return None;
    }
    let declared = obj.get(DECLARED).and_then(Value::as_object);
    let mut nodes = Map::new();
    let mut inputs = Map::new();
    for (name, entry) in pins {
        let mut node = Map::new();
        node.insert("locked".into(), strip(entry));
        if let Some(url) = declared.and_then(|d| d.get(name.as_str())) {
            node.insert("original".into(), json!({ "type": "tack", "url": url }));
        } else if declared.is_some() {
            node.insert("tackFollows".into(), Value::Bool(true));
        }
        inputs.insert(name.clone(), Value::String(name.clone()));
        nodes.insert(name.clone(), Value::Object(node));
    }
    nodes.insert(ROOT.into(), json!({ "inputs": inputs }));
    Some(json!({ "nodes": nodes, "root": ROOT, "version": 7 }))
}

/// pins.toml as far as eppur needs it.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Pins {
    #[serde(default)]
    pub pins: BTreeMap<String, Pin>,
    #[serde(default)]
    pub groups: BTreeSet<String>,
    /// Pins other pins' inputs follow: a base they share, such as nixpkgs.
    #[serde(default)]
    pub followed: BTreeSet<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Pin {
    /// "true", or the commit it is frozen at.
    #[serde(default)]
    pub frozen: Option<String>,
    /// It names signers, so `tack verify` has something to check.
    #[serde(default)]
    pub signed: bool,
    #[serde(default)]
    pub patched: bool,
}

/// Read in nix, whose fromTOML is the TOML eppur needs and nothing more. It
/// flattens groups and applies their defaults the way tack's resolver does.
const PINS_EXPR: &str = r#"
toml:
let
  inherit (builtins) attrNames attrValues all any concatLists filter foldl' head isAttrs isList isString mapAttrs split;
  inputs = toml.inputs or { };
  isGroup = e: isAttrs e && !(e ? url) && all isAttrs (attrValues e) && any (p: p ? url) (attrValues e);
  flat = foldl' (
    acc: name:
    let e = inputs.${name}; in
    acc // (if isGroup e then mapAttrs (_: v: v // { group = name; }) e else { ${name} = e; })
  ) { } (attrNames inputs);
  defaults = toml.groups or { };
  merged = mapAttrs (_: p: (if p ? group then defaults.${p.group} or { } else { }) // p) flat;
  first = s: head (split "/" s);
  followsOf = p: if isAttrs (p.follows or null) then map first (filter isString (attrValues p.follows)) else [ ];
  allFollow = toml.all_follow or { };
  allTargets = attrValues (mapAttrs (k: v: if isList v then k else if isString v then first v else null) allFollow);
in
{
  pins = mapAttrs (_: p: {
    frozen = if !(p ? frozen) || p.frozen == false then null else if p.frozen == true then "true" else p.frozen;
    signed = (p.signers or [ ]) != [ ];
    patched = (p.patches or [ ]) != [ ];
  }) merged;
  groups = filter (n: isGroup inputs.${n}) (attrNames inputs)
    ++ attrNames defaults
    ++ concatLists (map (p: if p ? group then [ p.group ] else [ ]) (attrValues merged));
  followed = concatLists (map followsOf (attrValues merged)) ++ filter isString allTargets;
}
"#;

impl Pins {
    pub fn read(flake: &Path) -> Result<Self> {
        let path = locker::tack_dir(flake).join("pins.toml");
        let expr = format!(
            "({PINS_EXPR}) (builtins.fromTOML (builtins.readFile {}))",
            crate::nix::nix_string(&path.to_string_lossy())
        );
        let value = crate::nix::eval_expr_impure(&expr)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_value(value)?)
    }

    pub fn frozen(&self, name: &str) -> Option<&str> {
        self.pins.get(name)?.frozen.as_deref()
    }

    /// The pins and groups, as tack's commands take them.
    pub fn names(&self) -> Vec<String> {
        self.pins
            .keys()
            .chain(&self.groups)
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

fn program() -> String {
    std::env::var("EPPUR_TACK").unwrap_or_else(|_| "tack".into())
}

fn installed() -> bool {
    Command::new(program())
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run tack in the flake; its output goes to the log as details.
pub fn run(flake: &Path, args: &[&str]) -> Result<()> {
    let what = format!("tack {}", args.first().copied().unwrap_or_default());
    let mut cmd = Command::new(program());
    cmd.args(args).current_dir(flake);
    let out = match crate::nix::run_command(cmd, &what) {
        Ok(out) => out,
        Err(e)
            if e.chain().any(|c| {
                c.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            bail!(
                "{what}: tack is not installed; set programs.eppur.tack.package, or put tack on PATH"
            )
        }
        Err(e) => return Err(e),
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        let tail: Vec<&str> = text.lines().rev().take(10).collect();
        bail!(
            "{what} failed ({}):\n{}",
            out.status,
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        log::detail(&format!("tack: {line}"));
    }
    Ok(())
}

/// The output of a tack command that only reads, such as `tack look`.
pub fn output(flake: &Path, args: &[&str]) -> Result<String> {
    let what = format!("tack {}", args.first().copied().unwrap_or_default());
    let mut cmd = Command::new(program());
    cmd.args(args).current_dir(flake);
    let out = crate::nix::run_command(cmd, &what).with_context(|| {
        format!("{what}: is tack installed? set programs.eppur.tack.package, or put tack on PATH")
    })?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        bail!("{what} failed ({}):\n{}", out.status, text.trim_end());
    }
    Ok(text)
}

/// The pins eppur froze, still frozen where eppur froze them: those it may
/// unfreeze. A pin frozen by hand, or frozen again by hand since, is left.
fn ours<'a>(file: &'a PinsFile, pins: &Pins) -> Vec<&'a str> {
    file.inputs
        .iter()
        .filter(|(name, pin)| {
            pin.tack_frozen.is_some() && pins.frozen(name) == pin.tack_frozen.as_deref()
        })
        .map(|(name, _)| name.as_str())
        .collect()
}

/// `tack update`, of `names` (pins or groups) or every pin. The pins eppur
/// holds back are unfrozen first, so the update can bring their upstream's
/// new revision for the build to try; settling the lock puts back those that
/// came back with the revision that failed, and `sync_freezes` freezes what
/// is still held once the run is done. Pins frozen by hand stay frozen.
pub fn update(flake: &Path, pins_path: &Path, names: &[String]) -> Result<()> {
    let pins = Pins::read(flake)?;
    let file = PinsFile::load(pins_path)?;
    let ours = ours(&file, &pins);
    if !ours.is_empty() {
        let mut args = vec!["unfreeze"];
        args.extend(ours.iter().copied());
        run(flake, &args)?;
    }
    let mut args = vec!["update"];
    args.extend(names.iter().map(String::as_str));
    run(flake, &args)
}

fn read_raw(lock_path: &Path) -> Result<Map<String, Value>> {
    let bytes =
        std::fs::read(lock_path).with_context(|| format!("reading {}", lock_path.display()))?;
    let json: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", lock_path.display()))?;
    match json {
        Value::Object(map) => Ok(map),
        _ => bail!("{} is not a tack lock", lock_path.display()),
    }
}

/// `value` pretty-printed as an entry of the lock, indented as tack does.
fn entry_text(value: &Value) -> String {
    serde_json::to_string_pretty(value)
        .unwrap_or_default()
        .replace('\n', "\n  ")
}

/// The entry to write for pin `name` at `locked`: the one tack wrote for that
/// revision when this run saw it, or else `locked` with what tack keeps
/// about where the pin comes from.
fn entry_for(name: &str, locked: &Value, current: Option<&str>) -> String {
    if let Some(text) = seen(name, locked) {
        return text;
    }
    let mut entry = locked.clone();
    let current: Option<Value> = current.and_then(|t| serde_json::from_str(t).ok());
    if let (Some(obj), Some(current)) = (entry.as_object_mut(), current)
        && current["type"] == obj["type"]
        && let Some(public) = current.get("public")
    {
        obj.insert("public".into(), public.clone());
    }
    entry_text(&entry)
}

/// The lock with these pins at these revisions, every other entry left as
/// tack wrote it, in its place.
fn with_entries(lock_path: &Path, wanted: &[(String, Value)]) -> Result<Vec<u8>> {
    let bytes =
        std::fs::read(lock_path).with_context(|| format!("reading {}", lock_path.display()))?;
    let mut entries =
        ordered(&bytes).with_context(|| format!("parsing {}", lock_path.display()))?;
    for (name, locked) in wanted {
        match entries.iter_mut().rev().find(|(n, _)| n == name) {
            Some((_, text)) => *text = entry_for(name, locked, Some(text)),
            None => entries.push((name.clone(), entry_for(name, locked, None))),
        }
    }
    let body: Vec<String> = entries
        .iter()
        .map(|(k, v)| format!("  {}: {v}", Value::String(k.clone())))
        .collect();
    Ok(format!("{{\n{}\n}}\n", body.join(",\n")).into_bytes())
}

/// Lock these pins at these revisions, leaving every other pin where it is.
pub fn set(lock_path: &Path, wanted: &[(String, Value)]) -> Result<()> {
    let bytes = with_entries(lock_path, wanted)?;
    crate::guard::critical(|| write_atomic(lock_path, &bytes))
}

/// Run `f` with pin `name` locked at `locked`, then put the lock back. tack
/// has no --override-input: the flake reads its pins from the lock in the
/// checkout, so the lock itself changes for as long as `f` evaluates. The
/// run's guard restores it should the run be stopped meanwhile.
pub fn with_pin_at<T>(
    lock_path: &Path,
    name: &str,
    locked: &Value,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let original =
        std::fs::read(lock_path).with_context(|| format!("reading {}", lock_path.display()))?;
    let changed = with_entries(lock_path, &[(name.to_string(), locked.clone())])?;
    crate::guard::critical(|| write_atomic(lock_path, &changed))?;
    let result = f();
    crate::guard::critical(|| write_atomic(lock_path, &original))?;
    result
}

/// How `tack freeze` records a pin at the revision the lock has: its commit
/// for a git, GitHub or GitLab pin, plain true for a tarball.
fn freeze_value(lock: &Lock, name: &str) -> String {
    let locked = lock.locked(name).unwrap_or(Value::Null);
    match (locked["type"].as_str(), locked["rev"].as_str()) {
        (Some("github" | "gitlab" | "git"), Some(rev)) => rev.to_string(),
        _ => "true".into(),
    }
}

/// Make pins.toml say what pins.json holds back: freeze each held pin at the
/// revision it is held at, so a `tack update` by hand leaves it there too,
/// and unfreeze the pins eppur froze that `before` held and the run let go.
/// A pin frozen by hand is never touched, nor recorded as eppur's, and a pin
/// whose next revision is still to be tried is left unfrozen until a build
/// has judged it. Returns one line per pin whose freeze changed since
/// `before`: an update unfreezes held pins first, and freezing one again
/// where it was is no change.
pub fn sync_freezes(flake: &Path, pins_path: &Path, before: &PinsFile) -> Result<Vec<String>> {
    let lock_path = locker::Locker::Tack.lock_path(flake);
    let lock = Lock::load(&lock_path)?;
    let pins = Pins::read(flake)?;
    let mut file = PinsFile::load(pins_path)?;
    let mut freeze = Vec::new();
    for (name, pin) in &file.inputs {
        if !pins.pins.contains_key(name) || pin.retrying {
            continue;
        }
        let wanted = freeze_value(&lock, name);
        match (pin.tack_frozen.as_deref(), pins.frozen(name)) {
            (_, None) => freeze.push(name.clone()),
            (Some(ours), Some(now)) if ours == now && now != wanted => freeze.push(name.clone()),
            _ => {}
        }
    }
    let unfreeze: Vec<&str> = ours(before, &pins)
        .into_iter()
        .filter(|name| !file.inputs.contains_key(*name))
        .collect();
    let released: Vec<&str> = before
        .inputs
        .iter()
        .filter(|(name, pin)| {
            pin.tack_frozen.is_some()
                && !file.inputs.contains_key(*name)
                && (unfreeze.contains(&name.as_str()) || pins.frozen(name).is_none())
        })
        .map(|(name, _)| name.as_str())
        .collect();
    let mut lines: Vec<String> = released
        .iter()
        .map(|n| format!("unfroze tack pin {n}"))
        .collect();
    if (!unfreeze.is_empty() || !freeze.is_empty()) && !installed() {
        let names: Vec<&str> = unfreeze
            .iter()
            .copied()
            .chain(freeze.iter().map(String::as_str))
            .collect();
        log::warn(&format!(
            "tack is not installed, so pins.toml does not show what eppur holds back ({}); set programs.eppur.tack.package",
            names.join(", ")
        ));
        return Ok(Vec::new());
    }
    if !unfreeze.is_empty() {
        let mut args = vec!["unfreeze"];
        args.extend(unfreeze.iter().copied());
        run(flake, &args)?;
    }
    if !freeze.is_empty() {
        let mut args = vec!["freeze"];
        args.extend(freeze.iter().map(String::as_str));
        run(flake, &args)?;
        let after = Pins::read(flake)?;
        for name in &freeze {
            let value = after.frozen(name).map(str::to_string);
            let was = before.inputs.get(name).and_then(|p| p.tack_frozen.clone());
            if value != was {
                lines.push(format!(
                    "froze tack pin {name} at {}",
                    match value.as_deref() {
                        Some("true") | None => "its locked revision".to_string(),
                        Some(rev) => rev.chars().take(12).collect(),
                    }
                ));
            }
            if let Some(pin) = file.inputs.get_mut(name) {
                pin.tack_frozen = value;
            }
        }
        crate::guard::critical(|| file.save(pins_path))?;
    }
    Ok(lines)
}

/// Unfreeze pin `name` if eppur froze it there, for `eppur clear
/// --flake-input`. True when it did.
pub fn release(flake: &Path, file: &PinsFile, name: &str) -> Result<bool> {
    let pins = Pins::read(flake)?;
    if !ours(file, &pins).contains(&name) {
        return Ok(false);
    }
    run(flake, &["unfreeze", name])?;
    Ok(true)
}

/// `tack verify` for a lock that is not the committed one, when a pin names
/// signers: a lock changed outside `tack update` (by hand, or synced from
/// elsewhere) has not had its commits checked, and one nobody trusted signed
/// stops the run before anything builds. Only commits new since the
/// committed lock are checked.
pub fn verify_changed(flake: &Path) -> Result<()> {
    let lock_path = locker::Locker::Tack.lock_path(flake);
    let Some(git) = crate::git::Git::open(flake) else {
        return Ok(());
    };
    if !git.tracked(&lock_path) || git.head().is_none() || !git.changed(&[&lock_path])? {
        return Ok(());
    }
    if !Pins::read(flake)?.pins.values().any(|p| p.signed) {
        return Ok(());
    }
    run(flake, &["verify", "--base", "HEAD"]).context(
        "the lock has a commit the signers pins.toml names did not sign; nothing was built",
    )?;
    log::info("tack verify: the changed pins are signed");
    Ok(())
}

/// Rebuild the patched trees of pins this machine's store lacks: a lock
/// made elsewhere names them by store path, and the flake does not evaluate
/// without them.
pub fn materialize(flake: &Path) -> Result<()> {
    let lock_path = locker::Locker::Tack.lock_path(flake);
    let map = read_raw(&lock_path)?;
    let missing: Vec<&str> = map
        .iter()
        .filter(|(name, _)| is_pin(name))
        .filter(|(_, entry)| {
            entry["patched"]["path"]
                .as_str()
                .is_some_and(|p| !Path::new(p).exists())
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    log::info(&format!(
        "building the patched trees of {} for this machine",
        missing.join(", ")
    ));
    let mut args = vec!["materialize"];
    args.extend(missing.iter().copied());
    run(flake, &args)
}

/// The files of the tack project eppur may change, as git paths: the lock,
/// pins.toml, and the patches `tack update` drops once they land upstream.
pub fn files(flake: &Path) -> Vec<PathBuf> {
    let dir = locker::tack_dir(flake);
    let mut files = vec![dir.join("pins.lock.json"), dir.join("pins.toml")];
    let patches = dir.join("patches");
    let tracked = crate::git::Git::open(flake).is_some_and(|g| g.tracked(&patches));
    if patches.exists() || tracked {
        files.push(patches);
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"{
      "$declared": { "nixpkgs": "gh:nixos/nixpkgs/nixos-unstable", "tool": "gh:alice/tool" },
      "nixpkgs": { "type": "github", "owner": "nixos", "repo": "nixpkgs", "rev": "aaa", "narHash": "sha256-A", "lastModified": 5, "public": true },
      "tool": { "type": "tarball", "url": "https://example.org/t.tar.gz", "narHash": "sha256-T", "patched": { "path": "/nix/store/x-source", "narHash": "sha256-P" }, "tag": "v1" },
      "hyprlang": { "type": "github", "owner": "hyprwm", "repo": "hyprlang", "rev": "ccc", "narHash": "sha256-H" },
      "_nested": { "x": 1 }
    }"#;

    fn lock() -> Lock {
        Lock::from_bytes(LOCK.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn reads_as_a_flake_lock() {
        let l = lock();
        let mut inputs = l.inputs();
        inputs.sort();
        assert_eq!(inputs, ["hyprlang", "nixpkgs", "tool"]);
        assert_eq!(l.nar_hash("nixpkgs").as_deref(), Some("sha256-A"));
        let locked = l.locked("nixpkgs").unwrap();
        assert!(locked.get("public").is_none());
        assert_eq!(locked["rev"], "aaa");
        let tool = l.locked("tool").unwrap();
        assert!(tool.get("patched").is_none() && tool.get("tag").is_none());
        assert_eq!(tool["url"], "https://example.org/t.tar.gz");
    }

    #[test]
    fn a_pin_without_a_url_of_its_own_is_not_direct() {
        let l = lock();
        assert!(l.is_direct("nixpkgs"));
        assert!(!l.is_direct("hyprlang"));
    }

    #[test]
    fn original_is_the_declared_url() {
        let l = lock();
        assert_eq!(
            l.original("nixpkgs").unwrap()["url"],
            "gh:nixos/nixpkgs/nixos-unstable"
        );
        assert_ne!(l.original("nixpkgs"), l.original("tool"));
    }

    #[test]
    fn hash_is_of_the_file_itself() {
        let l = lock();
        assert_eq!(l.bytes(), LOCK.as_bytes());
        assert_eq!(l.last_modified(), 5);
    }

    #[test]
    fn the_flake_lock_shape_round_trips() {
        let again = Lock::from_json(lock().json()).unwrap();
        assert_eq!(again.locked("nixpkgs"), lock().locked("nixpkgs"));
        assert_eq!(again.original("tool"), lock().original("tool"));
        assert!(!again.is_direct("hyprlang"));
    }

    #[test]
    fn other_json_is_not_a_tack_lock() {
        assert!(as_flake_lock(&json!({ "version": 2, "hosts": {} })).is_none());
        assert!(as_flake_lock(&json!({ "nodes": {} })).is_none());
        assert!(as_flake_lock(&json!({ "a": { "no": "type" } })).is_none());
        assert!(as_flake_lock(&json!({})).is_some());
    }

    #[test]
    fn putting_back_a_seen_revision_writes_tacks_own_entry() {
        let _ = lock();
        let locked = json!({ "type": "tarball", "url": "https://example.org/t.tar.gz", "narHash": "sha256-T" });
        let entry: Value = serde_json::from_str(&entry_for("tool", &locked, None)).unwrap();
        assert_eq!(entry["tag"], "v1");
        assert_eq!(entry["patched"]["narHash"], "sha256-P");
    }

    #[test]
    fn an_unseen_revision_keeps_where_the_pin_comes_from() {
        let locked = json!({ "type": "github", "owner": "o", "repo": "r", "rev": "zzz", "narHash": "sha256-unseen" });
        let current = r#"{ "type": "github", "owner": "o", "repo": "r", "rev": "yyy", "narHash": "sha256-Y", "public": true }"#;
        let entry: Value =
            serde_json::from_str(&entry_for("other", &locked, Some(current))).unwrap();
        assert_eq!(entry["public"], true);
        assert_eq!(entry["rev"], "zzz");
    }

    #[test]
    fn a_pin_put_back_where_it_was_gives_the_same_file() {
        let dir = std::env::temp_dir().join(format!("eppur-tack-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pins.lock.json");
        let before = "{\n  \"$declared\": {\n    \"b\": \"gh:o/b\",\n    \"a\": \"gh:o/a\"\n  },\n  \"b\": {\n    \"type\": \"github\",\n    \"owner\": \"o\",\n    \"repo\": \"b\",\n    \"rev\": \"old\",\n    \"narHash\": \"sha256-old-b\"\n  },\n  \"a\": {\n    \"type\": \"github\",\n    \"owner\": \"o\",\n    \"repo\": \"a\",\n    \"rev\": \"one\",\n    \"narHash\": \"sha256-a\"\n  }\n}\n";
        std::fs::write(&path, before).unwrap();
        let old = Lock::from_bytes(before.as_bytes().to_vec()).unwrap();
        let moved = before
            .replace("\"old\"", "\"new\"")
            .replace("sha256-old-b", "sha256-new-b");
        std::fs::write(&path, &moved).unwrap();
        set(&path, &[("b".into(), old.locked("b").unwrap())]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let unseen = json!({ "type": "github", "owner": "o", "repo": "b", "rev": "x", "narHash": "sha256-x" });
        set(&path, &[("b".into(), unseen)]).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.starts_with("{\n  \"$declared\": {\n    \"b\""),
            "{after}"
        );
        assert!(
            after.contains("  \"b\": {\n    \"narHash\": \"sha256-x\""),
            "{after}"
        );
        assert!(
            after.ends_with("\"narHash\": \"sha256-a\"\n  }\n}\n"),
            "{after}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn freezes_record_a_commit_or_true() {
        let l = lock();
        assert_eq!(freeze_value(&l, "nixpkgs"), "aaa");
        assert_eq!(freeze_value(&l, "tool"), "true");
    }
}
