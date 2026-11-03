use crate::graph::{Graph, Node};
use crate::logparse::strip_version;
use crate::nix::{self, Nix};
use anyhow::{Result, bail};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

pub const DEFAULT_DENY: &[&str] = &[
    "linux*",
    "linuxPackages*",
    "linuxKernel*",
    "glibc*",
    "systemd*",
    "nix",
    "nixVersions*",
    "stdenv*",
    "gcc*",
    "bash",
    "bashInteractive",
    "coreutils*",
    "nixos-*",
];

static ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_+-]*$").unwrap());

pub fn is_attr_name(s: &str) -> bool {
    ATTR.is_match(s)
}

pub fn glob(pattern: &str, s: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => s.starts_with(prefix),
        None => s == pattern,
    }
}

pub fn denied(attr: &str, extra: &[String]) -> bool {
    DEFAULT_DENY
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
        .any(|p| glob(p, attr))
}

/// Attribute names a derivation may be known under: its pname, its name
/// without the version, and for versioned attributes such as nodejs_22 or
/// postgresql_16 the pname with the version's major part. Every candidate is
/// confirmed against the drv path before it is used.
pub fn candidate_names(node: &Node) -> Vec<String> {
    let base = strip_version(&node.name);
    let pname = node.pname.as_deref().unwrap_or(base);
    let version = node
        .name
        .strip_prefix(base)
        .and_then(|v| v.strip_prefix('-'))
        .unwrap_or("");
    let major: String = version.chars().take_while(|c| c.is_ascii_digit()).collect();
    let mut names = vec![pname.to_string(), base.to_string()];
    if !major.is_empty() {
        names.push(format!("{pname}_{major}"));
        names.push(format!("{pname}{major}"));
    }
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if is_attr_name(&name) && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

fn nix_list(names: &BTreeSet<String>) -> String {
    let items: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    format!("[ {} ]", items.join(" "))
}

#[derive(Default)]
pub struct Matcher {
    by_drv: HashMap<String, BTreeSet<(String, String)>>,
}

impl Matcher {
    /// Match every node in `nodes` against the top-level attrs of each package
    /// set, by guessing the attr name from the drv and confirming the drvPath.
    pub fn build(
        nix: &Nix,
        graph: &Graph,
        nodes: &HashSet<String>,
        sets_attr: &str,
        inputs: &[String],
    ) -> Result<Self> {
        let names: BTreeSet<String> = nodes
            .iter()
            .filter_map(|p| graph.nodes.get(p))
            .flat_map(candidate_names)
            .collect();
        let mut m = Matcher::default();
        let inputs: BTreeSet<String> = inputs.iter().filter(|i| is_attr_name(i)).cloned().collect();
        if names.is_empty() || inputs.is_empty() {
            return Ok(m);
        }

        // One evaluation for every package set: the host's configuration is
        // evaluated once instead of once per set.
        let apply = format!(
            r#"sets: builtins.listToAttrs (map (input: {{ name = input; value = let set = sets.${{input}} or {{ }}; in builtins.listToAttrs (map (n: {{ name = n; value = let r = builtins.tryEval (if builtins.hasAttr n set then (let v = builtins.getAttr n set; in if builtins.isAttrs v && (v.type or null) == "derivation" then v.drvPath else null) else null); in if r.success then r.value else null; }}) {names}); }}) {inputs})"#,
            names = nix_list(&names),
            inputs = nix_list(&inputs)
        );
        let result = nix.eval_apply(sets_attr, &apply)?;
        for (input, found) in result.as_object().into_iter().flatten() {
            for (name, drv) in found.as_object().into_iter().flatten() {
                if let Some(drv) = drv.as_str().filter(|d| nodes.contains(*d)) {
                    m.by_drv
                        .entry(drv.to_string())
                        .or_default()
                        .insert((input.clone(), name.clone()));
                }
            }
        }
        Ok(m)
    }

    pub fn matches(&self, drv: &str) -> bool {
        self.by_drv.contains_key(drv)
    }

    pub fn attrs(&self, drv: &str) -> impl Iterator<Item = &(String, String)> {
        self.by_drv.get(drv).into_iter().flatten()
    }
}

/// Whether `attr` evaluates to a derivation in the nixpkgs at `locked`.
pub fn evaluates_at(locked: &Value, system: &str, attr: &str) -> Result<bool> {
    if !is_attr_name(attr)
        || !system
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("refusing to evaluate unexpected name {attr:?} for {system:?}");
    }
    let json = serde_json::to_string(locked)?;
    if json.contains("''") || json.contains("${") {
        bail!("locked node cannot be quoted safely");
    }
    let expr = format!(
        r#"let p = import (builtins.fetchTree (builtins.fromJSON ''{json}'')) {{ system = "{system}"; config.allowUnfree = true; }}; r = builtins.tryEval (if builtins.hasAttr "{attr}" p then (builtins.getAttr "{attr}" p).drvPath else null); in r.success && r.value != null"#
    );
    Ok(nix::eval_expr(&expr)?.as_bool().unwrap_or(false))
}

/// The nix expression that turns the package set `p` into `attr`'s drv, out
/// path and version, or null when it is not a derivation there.
pub fn copy_expr(attr: &str) -> String {
    format!(
        r#"let r = builtins.tryEval (if builtins.hasAttr "{attr}" p then let a = builtins.getAttr "{attr}" p; in {{ drv = a.drvPath; out = a.outPath; version = (builtins.parseDrvName a.name).version; }} else null); in if r.success then r.value else null"#
    )
}

/// `attr`'s drv, out path and version in the nixpkgs at `locked`, imported
/// plainly for `system`; None when it is not a derivation there.
pub fn copy_at(locked: &Value, system: &str, attr: &str) -> Result<Option<Value>> {
    if !is_attr_name(attr)
        || !system
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("refusing to evaluate unexpected name {attr:?} for {system:?}");
    }
    let json = serde_json::to_string(locked)?;
    if json.contains("''") || json.contains("${") {
        bail!("locked node cannot be quoted safely");
    }
    let expr = format!(
        r#"let p = import (builtins.fetchTree (builtins.fromJSON ''{json}'')) {{ system = "{system}"; config.allowUnfree = true; }}; in {}"#,
        copy_expr(attr)
    );
    let v = nix::eval_expr(&expr)?;
    Ok((!v.is_null()).then_some(v))
}

/// The next component of a version string after `*i`, as nix splits them:
/// a run of digits or a run of other characters, separated by . and -.
fn next_component<'a>(s: &'a str, i: &mut usize) -> &'a str {
    let b = s.as_bytes();
    while *i < b.len() && (b[*i] == b'.' || b[*i] == b'-') {
        *i += 1;
    }
    let start = *i;
    if *i < b.len() && b[*i].is_ascii_digit() {
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
    } else {
        while *i < b.len() && !(b[*i].is_ascii_digit() || b[*i] == b'.' || b[*i] == b'-') {
            *i += 1;
        }
    }
    &s[start..*i]
}

fn component_lt(c1: &str, c2: &str) -> bool {
    let n1 = c1.parse::<u64>().ok();
    let n2 = c2.parse::<u64>().ok();
    match (n1, n2) {
        (Some(a), Some(b)) => a < b,
        _ if c1.is_empty() && n2.is_some() => true,
        _ if c1 == "pre" && c2 != "pre" => true,
        _ if c2 == "pre" => false,
        (_, Some(_)) => true,
        (Some(_), _) => false,
        _ => c1 < c2,
    }
}

/// builtins.compareVersions: how version `a` orders against `b`.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        let c1 = next_component(a, &mut i);
        let c2 = next_component(b, &mut j);
        if component_lt(c1, c2) {
            return std::cmp::Ordering::Less;
        }
        if component_lt(c2, c1) {
            return std::cmp::Ordering::Greater;
        }
    }
    std::cmp::Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_as_nix_orders_them() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("2026.10.1", "2026.10.3"), Less);
        assert_eq!(compare_versions("2026.10.3", "2026.10.3"), Equal);
        assert_eq!(compare_versions("2026.10.10", "2026.10.3"), Greater);
        assert_eq!(compare_versions("1.0", "1.0.1"), Less);
        assert_eq!(compare_versions("2.3a", "2.3.1"), Less);
        assert_eq!(compare_versions("1.0pre1", "1.0"), Less);
        assert_eq!(compare_versions("1.0-rc1", "1.0-rc2"), Less);
        assert_eq!(
            compare_versions("0-unstable-2026-10-01", "0-unstable-2026-09-30"),
            Greater
        );
        assert_eq!(compare_versions("", "1"), Less);
    }

    #[test]
    fn denylist() {
        assert!(denied("linux_6_12", &[]));
        assert!(denied("linuxPackages_latest", &[]));
        assert!(denied("systemd", &[]));
        assert!(denied("nix", &[]));
        assert!(!denied("nix-output-monitor", &[]));
        assert!(!denied("obs-studio", &[]));
        assert!(denied("obs-studio", &["obs-*".to_string()]));
    }

    #[test]
    fn candidates_from_pname_and_name() {
        let node = Node {
            name: "obs-studio-31.0.1".into(),
            pname: Some("obs-studio".into()),
            ..Default::default()
        };
        assert_eq!(
            candidate_names(&node),
            vec!["obs-studio", "obs-studio_31", "obs-studio31"]
        );

        let node = Node {
            name: "hello-2.12".into(),
            pname: None,
            ..Default::default()
        };
        assert_eq!(candidate_names(&node), vec!["hello", "hello_2", "hello2"]);

        let node = Node {
            name: "python3.13-requests-2.32".into(),
            pname: Some("requests".into()),
            ..Default::default()
        };
        assert_eq!(
            candidate_names(&node),
            vec!["requests", "requests_2", "requests2"]
        );

        let node = Node {
            name: "nodejs-22.11.0".into(),
            pname: Some("nodejs".into()),
            ..Default::default()
        };
        assert!(candidate_names(&node).contains(&"nodejs_22".to_string()));

        let node = Node {
            name: "unit-foo.service".into(),
            pname: None,
            ..Default::default()
        };
        assert!(candidate_names(&node).is_empty());
    }

    #[test]
    fn nix_list_quotes() {
        let names: BTreeSet<String> = ["b".to_string(), "a-1".to_string()].into();
        assert_eq!(nix_list(&names), r#"[ "a-1" "b" ]"#);
    }
}
