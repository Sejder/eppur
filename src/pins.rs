use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub const VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Nixos,
    Home,
}

impl Kind {
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Nixos => "host",
            Kind::Home => "home",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    Build,
    Eval,
    Manual,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Build => "build",
            Reason::Eval => "eval",
            Reason::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pin {
    pub locked: Value,
    pub since: String,
    pub reason: Reason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_drv: Option<String>,
    /// Derivation names from the failed drv up to the pinned package.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trail: Vec<String>,
    /// For an evaluation failure, what nix said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether the machine's own overlays apply to the pinned copy; missing
    /// for pins made before eppur checked, which get none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlays: Option<bool>,
    pub checked_nar_hash: String,
    /// For a build failure, the pinned package's own drv when it failed: an
    /// input that moves without changing it is not retried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_drv: Option<String>,
    /// The day the package was last built and failed, when that was after
    /// `since`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tried: Option<String>,
    /// The donor input `locked` comes from, when it is not the pinned input's
    /// own history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub donor: Option<String>,
    /// Whether no other package in the system depends on the pinned one, so
    /// a donor's copy may replace it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub leaf: bool,
    /// The version of the package the pin provides, for status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Whether nixpkgs refuses the package as insecure: the pin keeps the
    /// vulnerable copy this machine already built, instead of holding the host.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
}

impl Pin {
    /// Whether two pins came from one failure: the same package failing the
    /// same way, pinned in several package sets that provide it.
    pub fn same_failure(&self, other: &Pin) -> bool {
        self.reason != Reason::Manual
            && self.reason == other.reason
            && self.failed_drv == other.failed_drv
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    pub since: String,
    pub reason: String,
    /// The whole flake.lock this host last built with.
    pub lock: Value,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    #[serde(default)]
    pub lock_sha256: String,
    #[serde(default)]
    pub lock_last_modified: i64,
    /// The newest lock this entry was tried against: the same as the lock it
    /// was computed from, except for a held entry. Merge ranks entries by it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub checked_last_modified: i64,
    #[serde(default)]
    pub pins: BTreeMap<String, BTreeMap<String, Pin>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<Held>,
}

impl Entry {
    /// How recent the newest lock this entry reflects is.
    pub fn recency(&self) -> i64 {
        self.lock_last_modified.max(self.checked_last_modified)
    }

    pub fn get(&self, input: &str, attr: &str) -> Option<&Pin> {
        self.pins.get(input)?.get(attr)
    }

    pub fn insert(&mut self, input: &str, attr: &str, pin: Pin) {
        self.pins
            .entry(input.to_string())
            .or_default()
            .insert(attr.to_string(), pin);
    }

    pub fn remove(&mut self, input: &str, attr: &str) -> Option<Pin> {
        let set = self.pins.get_mut(input)?;
        let pin = set.remove(attr);
        if set.is_empty() {
            self.pins.remove(input);
        }
        pin
    }

    /// The inputs that pin `attr`, for commands that take only an attr name.
    pub fn inputs_pinning(&self, attr: &str) -> Vec<String> {
        self.pins
            .iter()
            .filter(|(_, set)| set.contains_key(attr))
            .map(|(input, _)| input.clone())
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &Pin)> {
        self.pins.iter().flat_map(|(input, set)| {
            set.iter()
                .map(move |(attr, pin)| (input.as_str(), attr.as_str(), pin))
        })
    }

    /// The pins grouped by package: one package pinned in several package
    /// sets is one entry here, with every input it is pinned in.
    pub fn packages(&self) -> BTreeMap<&str, Vec<(&str, &Pin)>> {
        let mut out: BTreeMap<&str, Vec<(&str, &Pin)>> = BTreeMap::new();
        for (input, attr, pin) in self.iter() {
            out.entry(attr).or_default().push((input, pin));
        }
        out
    }

    /// The inputs whose pin of `attr` came from the same failure as the one
    /// in `input`, `input` included: they are retried and released together.
    pub fn same_failure(&self, input: &str, attr: &str) -> Vec<String> {
        let Some(pin) = self.get(input, attr) else {
            return Vec::new();
        };
        self.iter()
            .filter(|(i, a, p)| *a == attr && (*i == input || pin.same_failure(p)))
            .map(|(i, _, _)| i.to_string())
            .collect()
    }
}

/// A flake input held back in flake.lock, for the whole flake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputPin {
    /// The flake.lock node's `locked` of the revision it is held at.
    pub locked: Value,
    /// The narHash of the revision that failed.
    pub failed_nar_hash: String,
    pub since: String,
    pub reason: Reason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_drv: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The targets it failed on, e.g. "host home": a retry only counts as
    /// passed once each of them built.
    pub targets: Vec<String>,
    /// The lock has a revision other than `locked` for a build to try: a newer
    /// one, or the failed one when `locked` could not be put back.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retrying: bool,
}

impl InputPin {
    pub fn good_nar_hash(&self) -> Option<&str> {
        self.locked["narHash"].as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinsFile {
    pub version: u32,
    #[serde(default)]
    pub hosts: BTreeMap<String, Entry>,
    #[serde(default)]
    pub homes: BTreeMap<String, Entry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, InputPin>,
    /// When `inputs` last changed, in seconds: it is one decision about one
    /// lock, so `eppur merge` takes the newer map whole, an emptied one too.
    #[serde(default, rename = "inputsChanged", skip_serializing_if = "is_zero")]
    pub inputs_changed: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl Default for PinsFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            hosts: BTreeMap::new(),
            homes: BTreeMap::new(),
            inputs: BTreeMap::new(),
            inputs_changed: 0,
        }
    }
}

impl PinsFile {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let json: Value = serde_json::from_slice(bytes)?;
        match json["version"].as_u64() {
            Some(v) if v == VERSION as u64 => Ok(serde_json::from_value(json)?),
            Some(v) => bail!("pins file version {v} is not supported (expected {VERSION})"),
            None if json.as_object().is_some_and(|o| o.is_empty()) => Ok(Self::default()),
            None => bail!("not an eppur pins file (no version)"),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Self::parse(&bytes).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("pins serialize");
        out.push(b'\n');
        out
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &self.to_bytes())
    }

    fn section(&self, kind: Kind) -> &BTreeMap<String, Entry> {
        match kind {
            Kind::Nixos => &self.hosts,
            Kind::Home => &self.homes,
        }
    }

    fn section_mut(&mut self, kind: Kind) -> &mut BTreeMap<String, Entry> {
        match kind {
            Kind::Nixos => &mut self.hosts,
            Kind::Home => &mut self.homes,
        }
    }

    pub fn entry(&self, kind: Kind, name: &str) -> Option<&Entry> {
        self.section(kind).get(name)
    }

    pub fn entry_mut(&mut self, kind: Kind, name: &str) -> &mut Entry {
        self.section_mut(kind).entry(name.to_string()).or_default()
    }

    pub fn set(&mut self, kind: Kind, name: &str, entry: Entry) {
        self.section_mut(kind).insert(name.to_string(), entry);
    }

    pub fn entries(&self) -> impl Iterator<Item = (Kind, &str, &Entry)> {
        self.hosts
            .iter()
            .map(|(n, e)| (Kind::Nixos, n.as_str(), e))
            .chain(self.homes.iter().map(|(n, e)| (Kind::Home, n.as_str(), e)))
    }

    /// Mark `inputs` as changed now. Always moves forward, so a change made
    /// within the same second still wins a merge.
    pub fn touch_inputs(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        self.inputs_changed = now.max(self.inputs_changed + 1);
    }

    /// Take the other file's input pins if they changed later than these.
    /// Returns whether they were taken.
    pub fn merge_inputs(&mut self, other: &PinsFile) -> bool {
        if other.inputs_changed <= self.inputs_changed {
            return false;
        }
        self.inputs = other.inputs.clone();
        self.inputs_changed = other.inputs_changed;
        true
    }

    /// Put `entry` in place unless the file already holds one computed from a
    /// newer lock: how `eppur merge` combines two files, e.g. a fleet's
    /// published pins with a checkout's. Pins made by hand here that the
    /// incoming entry lacks are kept. Returns whether the entry was taken.
    #[cfg(test)]
    pub fn merge(&mut self, kind: Kind, name: &str, entry: Entry) -> bool {
        self.merge_for_lock(kind, name, entry, None)
    }

    /// `merge` for a checkout whose flake.lock has this sha256. An entry made
    /// for that very lock beats one that was not, even if that one looks
    /// newer: rolling back a flake input lowers a lock's lastModified, so the
    /// later decision can rank as the older one.
    pub fn merge_for_lock(
        &mut self,
        kind: Kind,
        name: &str,
        mut entry: Entry,
        lock_sha256: Option<&str>,
    ) -> bool {
        let section = self.section_mut(kind);
        if let Some(existing) = section.get(name) {
            let for_this_lock = lock_sha256
                .is_some_and(|sha| entry.lock_sha256 == sha && existing.lock_sha256 != sha);
            // A tie keeps the entry here: the same lock, and a local change
            // (a clear or a retry) is deliberate.
            if existing.recency() >= entry.recency() && !for_this_lock {
                return false;
            }
            for (input, attr, pin) in existing.iter() {
                if pin.reason == Reason::Manual && entry.get(input, attr).is_none() {
                    entry.insert(input, attr, pin.clone());
                }
            }
        }
        section.insert(name.to_string(), entry);
        true
    }
}

/// Replace `path` with `bytes` in one rename. The temporary file gets a fresh
/// name and is created exclusively, so a symlink planted in its place is never
/// followed. A replaced file keeps its owner and mode, so root rewriting a
/// user's flake.lock leaves it the user's.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    // A symlinked file (pins.json kept elsewhere) is written where it points,
    // so the link stays a link.
    let resolved;
    let path = if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        resolved = path
            .canonicalize()
            .with_context(|| format!("resolving {}", path.display()))?;
        resolved.as_path()
    } else {
        path
    };

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let before = std::fs::metadata(path).ok();
    let mode = before
        .as_ref()
        .map_or(0o644, |m| m.permissions().mode() & 0o7777);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());

    for attempt in 0..32 {
        let tmp = path.with_file_name(format!(
            ".{name}.{}.{nanos}.{attempt}.tmp",
            std::process::id()
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", tmp.display())),
        };
        let written = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("writing {}", tmp.display()));
        }
        if let Some(m) = &before {
            let _ = std::os::unix::fs::chown(&tmp, Some(m.uid()), Some(m.gid()));
        }
        return std::fs::rename(&tmp, path)
            .with_context(|| format!("replacing {}", path.display()));
    }
    anyhow::bail!(
        "could not create a temporary file next to {}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pin(since: &str) -> Pin {
        Pin {
            locked: json!({ "type": "github", "rev": "abc", "narHash": "sha256-X" }),
            since: since.into(),
            reason: Reason::Build,
            failed_drv: Some("/nix/store/aaa-libfoo-1.2.drv".into()),
            trail: vec!["libfoo-1.2".into(), "obs-studio-31".into()],
            error: None,
            overlays: None,
            checked_nar_hash: "sha256-Y".into(),
            root_drv: None,
            tried: None,
            donor: None,
            leaf: false,
            version: None,
            insecure: false,
        }
    }

    fn entry(lm: i64) -> Entry {
        Entry {
            lock_sha256: format!("{lm}"),
            lock_last_modified: lm,
            ..Default::default()
        }
    }

    #[test]
    fn round_trip_and_stable_order() {
        let mut f = PinsFile::default();
        let e = f.entry_mut(Kind::Nixos, "laptop");
        e.insert("nixpkgs-unstable", "zed-editor", pin("2026-10-20"));
        e.insert("nixpkgs", "hello", pin("2026-10-22"));
        f.entry_mut(Kind::Home, "alice").held = Some(Held {
            since: "2026-10-27".into(),
            reason: "linux fails".into(),
            lock: json!({ "nodes": {} }),
        });
        let bytes = f.to_bytes();
        assert_eq!(PinsFile::parse(&bytes).unwrap(), f);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"version\": 2"));
        assert!(text.find("\"nixpkgs\"").unwrap() < text.find("\"nixpkgs-unstable\"").unwrap());
        assert!(text.contains("\"checkedNarHash\""));
        assert!(text.contains("\"lockLastModified\""));
        assert_eq!(f.entry(Kind::Nixos, "laptop").unwrap().iter().count(), 2);
    }

    #[test]
    fn rejects_other_versions() {
        assert!(PinsFile::parse(br#"{"version":3}"#).is_err());
        assert!(PinsFile::parse(br#"{"lockSha256":"x","pins":{}}"#).is_err());
        assert_eq!(PinsFile::parse(b"{}").unwrap(), PinsFile::default());
    }

    fn input_pin() -> InputPin {
        InputPin {
            locked: serde_json::json!({ "narHash": "sha256-good" }),
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
    fn a_file_without_input_pins_serializes_as_before() {
        let bytes = PinsFile::default().to_bytes();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains("inputs"), "{text}");
        assert_eq!(PinsFile::parse(&bytes).unwrap(), PinsFile::default());
    }

    #[test]
    fn input_pins_round_trip() {
        let mut f = PinsFile::default();
        f.inputs.insert("atlas".into(), input_pin());
        f.touch_inputs();
        assert_eq!(PinsFile::parse(&f.to_bytes()).unwrap(), f);
    }

    #[test]
    fn merge_takes_the_later_input_pins_whole() {
        let mut here = PinsFile::default();
        here.inputs.insert("atlas".into(), input_pin());
        here.touch_inputs();

        let mut older = PinsFile::default();
        older.inputs.insert("other".into(), input_pin());
        older.inputs_changed = here.inputs_changed - 1;
        assert!(!here.merge_inputs(&older));
        assert!(here.inputs.contains_key("atlas"));
        assert!(!here.merge_inputs(&here.clone()));

        // A later file that emptied the map, e.g. after a clear, wins.
        let mut emptied = here.clone();
        emptied.inputs.clear();
        emptied.touch_inputs();
        assert!(here.merge_inputs(&emptied));
        assert!(here.inputs.is_empty());
    }

    #[test]
    fn an_entry_for_the_current_lock_wins_though_it_looks_older() {
        let mut older = entry(5);
        older.lock_sha256 = "current".into();
        let mut here = PinsFile::default();
        here.set(Kind::Nixos, "laptop", entry(10));
        assert!(!here.merge(Kind::Nixos, "laptop", older.clone()));
        assert!(!here.merge_for_lock(Kind::Nixos, "laptop", older.clone(), Some("other")));
        assert!(here.merge_for_lock(Kind::Nixos, "laptop", older.clone(), Some("current")));
        assert_eq!(here.entry(Kind::Nixos, "laptop"), Some(&older));
        // Once it is here, it is the same lock, and a local change stays.
        let mut local = older.clone();
        local.checked_last_modified = 1;
        assert!(!here.merge_for_lock(Kind::Nixos, "laptop", local, Some("current")));
    }

    #[test]
    fn merge_keeps_the_newer_lock() {
        let mut f = PinsFile::default();
        assert!(f.merge(Kind::Nixos, "laptop", entry(10)));
        assert!(!f.merge(Kind::Nixos, "laptop", entry(5)));
        assert_eq!(
            f.entry(Kind::Nixos, "laptop").unwrap().lock_last_modified,
            10
        );
        assert!(!f.merge(Kind::Nixos, "laptop", entry(10)));
        assert!(f.merge(Kind::Nixos, "laptop", entry(20)));
        assert_eq!(
            f.entry(Kind::Nixos, "laptop").unwrap().lock_last_modified,
            20
        );
    }

    #[test]
    fn merge_leaves_other_hosts_alone() {
        let mut f = PinsFile::default();
        f.set(Kind::Nixos, "pi", entry(3));
        f.set(Kind::Home, "laptop", entry(99));
        f.merge(Kind::Nixos, "laptop", entry(7));
        assert_eq!(f.entry(Kind::Nixos, "pi").unwrap().lock_last_modified, 3);
        assert_eq!(
            f.entry(Kind::Home, "laptop").unwrap().lock_last_modified,
            99
        );
        assert_eq!(
            f.entry(Kind::Nixos, "laptop").unwrap().lock_last_modified,
            7
        );
    }

    #[test]
    fn write_atomic_keeps_mode_and_ignores_planted_temp_names() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("eppur-wa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("flake.lock");
        std::fs::write(&target, b"old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let decoy = dir.join("decoy");
        std::fs::write(&decoy, b"untouched").unwrap();
        std::os::unix::fs::symlink(&decoy, dir.join(".flake.lock.tmp")).unwrap();
        write_atomic(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read(&decoy).unwrap(), b"untouched");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn merge_keeps_manual_pins_made_here() {
        let mut f = PinsFile::default();
        let mut mine = entry(5);
        let mut manual = pin("2026-10-01");
        manual.reason = Reason::Manual;
        mine.insert("nixpkgs", "hello", manual);
        mine.insert("nixpkgs", "obs-studio", pin("2026-10-01"));
        f.set(Kind::Nixos, "laptop", mine);
        assert!(f.merge(Kind::Nixos, "laptop", entry(9)));
        let e = f.entry(Kind::Nixos, "laptop").unwrap();
        assert_eq!(e.get("nixpkgs", "hello").unwrap().reason, Reason::Manual);
        assert!(e.get("nixpkgs", "obs-studio").is_none());
        assert_eq!(e.lock_last_modified, 9);
    }

    #[test]
    fn one_failure_in_several_sets_is_one_package() {
        let mut e = entry(1);
        e.insert("nixpkgs", "cloudflared", pin("2026-10-28"));
        e.insert("nixpkgs-unstable", "cloudflared", pin("2026-10-28"));
        let mut other = pin("2026-10-28");
        other.failed_drv = Some("/nix/store/bbb-other.drv".into());
        e.insert("nixpkgs-small", "cloudflared", other);
        assert_eq!(e.packages().len(), 1);
        assert_eq!(e.packages()["cloudflared"].len(), 3);
        assert_eq!(
            e.same_failure("nixpkgs", "cloudflared"),
            ["nixpkgs", "nixpkgs-unstable"]
        );
        assert_eq!(
            e.same_failure("nixpkgs-small", "cloudflared"),
            ["nixpkgs-small"]
        );
    }

    #[test]
    fn write_atomic_writes_through_a_symlink() {
        let dir = std::env::temp_dir().join(format!("eppur-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.json");
        let link = dir.join("pins.json");
        std::fs::write(&real, b"old").unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"new");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_drops_empty_inputs() {
        let mut e = Entry::default();
        e.insert("nixpkgs", "hello", pin("2026-10-22"));
        assert_eq!(e.inputs_pinning("hello"), vec!["nixpkgs"]);
        assert!(e.remove("nixpkgs", "hello").is_some());
        assert!(e.pins.is_empty());
        assert!(e.inputs_pinning("hello").is_empty());
    }
}
