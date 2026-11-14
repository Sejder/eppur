use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::IgnoredAny;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

#[derive(Debug, Default)]
pub struct Node {
    pub name: String,
    pub pname: Option<String>,
    pub inputs: Vec<String>,
    pub parents: Vec<String>,
    /// A fixed-output derivation, i.e. a download.
    pub fixed_output: bool,
    /// Store paths it takes as sources, such as a flake's source tree.
    pub srcs: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub nodes: HashMap<String, Node>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ascent {
    Roots(BTreeSet<String>),
    Unpinnable { via: String },
    TooLarge,
}

#[derive(Deserialize)]
struct NewDoc {
    derivations: HashMap<String, DrvJson>,
}

#[derive(Deserialize)]
struct DrvJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    env: EnvJson,
    #[serde(default, rename = "structuredAttrs")]
    structured_attrs: Option<AttrsJson>,
    #[serde(default)]
    outputs: Option<HashMap<String, OutputJson>>,
    #[serde(default)]
    inputs: Option<InputsJson>,
    #[serde(default, rename = "inputDrvs")]
    input_drvs: Option<HashMap<String, IgnoredAny>>,
    #[serde(default, rename = "inputSrcs")]
    input_srcs: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct EnvJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pname: Option<String>,
    #[serde(default, rename = "outputHash")]
    output_hash: Option<String>,
    #[serde(default, rename = "__json")]
    json: Option<String>,
}

/// The attributes of a derivation built with structured attrs, which `nix
/// derivation show` prints as an object (or, in older layouts, as the JSON
/// string in `env.__json`).
#[derive(Deserialize, Default)]
struct AttrsJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pname: Option<String>,
    #[serde(default, rename = "outputHash")]
    output_hash: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct OutputJson {
    #[serde(default)]
    hash: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct InputsJson {
    #[serde(default)]
    drvs: HashMap<String, IgnoredAny>,
    #[serde(default)]
    srcs: Vec<String>,
}

/// The store directory a store path lives in, e.g. /nix/store for
/// /nix/store/...-hello.drv; newer `nix derivation show` output names
/// derivations relative to it.
pub fn store_dir_of(path: &str) -> String {
    match path.rfind('/') {
        Some(i) if i > 0 => path[..i].to_string(),
        _ => "/nix/store".to_string(),
    }
}

fn full_path(store: &str, p: String) -> String {
    if p.starts_with('/') {
        p
    } else {
        format!("{store}/{p}")
    }
}

impl Graph {
    /// Accepts both `nix derivation show` layouts: the versioned one with
    /// `derivations` and store-relative keys, and the older full-path map.
    pub fn from_show_json(bytes: &[u8], store: &str) -> Result<Self> {
        let drvs = match serde_json::from_slice::<NewDoc>(bytes) {
            Ok(doc) => doc.derivations,
            Err(_) => serde_json::from_slice::<HashMap<String, DrvJson>>(bytes)
                .context("parsing nix derivation show output")?,
        };

        let mut graph = Graph::default();
        for (path, drv) in drvs {
            let (inputs, srcs): (Vec<String>, Vec<String>) = match (drv.inputs, drv.input_drvs) {
                (Some(i), _) => (i.drvs.into_keys().collect(), i.srcs),
                (None, Some(i)) => (i.into_keys().collect(), drv.input_srcs.unwrap_or_default()),
                (None, None) => (Vec::new(), drv.input_srcs.unwrap_or_default()),
            };
            let attrs = drv
                .structured_attrs
                .or_else(|| {
                    drv.env
                        .json
                        .as_deref()
                        .and_then(|j| serde_json::from_str::<AttrsJson>(j).ok())
                })
                .unwrap_or_default();
            let fixed_output = drv
                .outputs
                .as_ref()
                .is_some_and(|o| o.values().any(|o| o.hash.is_some()))
                || drv.env.output_hash.is_some()
                || attrs.output_hash.is_some();
            let node = Node {
                name: drv.name.or(drv.env.name).or(attrs.name).unwrap_or_default(),
                pname: drv.env.pname.or(attrs.pname),
                inputs: inputs.into_iter().map(|i| full_path(store, i)).collect(),
                parents: Vec::new(),
                fixed_output,
                srcs: srcs.into_iter().map(|p| full_path(store, p)).collect(),
            };
            graph.nodes.insert(full_path(store, path), node);
        }
        graph.link();
        Ok(graph)
    }

    fn link(&mut self) {
        let edges: Vec<(String, String)> = self
            .nodes
            .iter()
            .flat_map(|(p, n)| n.inputs.iter().map(move |i| (i.clone(), p.clone())))
            .collect();
        for (child, parent) in edges {
            if let Some(n) = self.nodes.get_mut(&child) {
                n.parents.push(parent);
            }
        }
        for n in self.nodes.values_mut() {
            n.parents.sort();
            n.parents.dedup();
        }
    }

    /// Every derivation `top` depends on, itself included: one host's closure.
    pub fn closure(&self, top: &str) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut stack = vec![top.to_string()];
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            if let Some(n) = self.nodes.get(&p) {
                stack.extend(n.inputs.iter().cloned());
            }
        }
        seen
    }

    fn parents_within<'a>(
        &'a self,
        p: &str,
        within: &'a HashSet<String>,
    ) -> impl Iterator<Item = &'a String> {
        self.nodes
            .get(p)
            .into_iter()
            .flat_map(|n| n.parents.iter())
            .filter(move |q| within.contains(*q))
    }

    pub fn ancestors(
        &self,
        start: &str,
        within: &HashSet<String>,
        cap: usize,
    ) -> Option<HashSet<String>> {
        let mut seen = HashSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            if seen.len() > cap {
                return None;
            }
            stack.extend(self.parents_within(&p, within).cloned());
        }
        Some(seen)
    }

    /// Derivation names on a shortest path from `from` up to `to`.
    pub fn trail(&self, from: &str, to: &str, within: &HashSet<String>) -> Vec<String> {
        let mut prev: HashMap<String, String> = HashMap::new();
        let mut queue = VecDeque::from([from.to_string()]);
        let mut seen = HashSet::from([from.to_string()]);
        while let Some(p) = queue.pop_front() {
            if p == to {
                let mut path = vec![p.clone()];
                let mut cur = p;
                while let Some(q) = prev.get(&cur) {
                    path.push(q.clone());
                    cur = q.clone();
                }
                path.reverse();
                return path.iter().map(|d| self.display_name(d)).collect();
            }
            for q in self.parents_within(&p, within) {
                if seen.insert(q.clone()) {
                    prev.insert(q.clone(), p.clone());
                    queue.push_back(q.clone());
                }
            }
        }
        Vec::new()
    }

    /// Whether `drv` builds a package (it has a pname and is not a download),
    /// as opposed to configuration glue such as a unit file or system-path.
    pub fn is_package(&self, drv: &str) -> bool {
        self.nodes
            .get(drv)
            .is_some_and(|n| n.pname.is_some() && !n.fixed_output)
    }

    pub fn display_name(&self, drv: &str) -> String {
        match self.nodes.get(drv) {
            Some(n) if !n.name.is_empty() => n.name.clone(),
            _ => drv.to_string(),
        }
    }

    /// Walk up from a failed derivation, inside one host's closure, to the
    /// packages that should be pinned.
    ///
    /// A root is a matched derivation (one that is a top-level attribute of a
    /// package set) with at least one unmatched parent: it is consumed directly by
    /// configuration glue, not only by other packages. Pinning it swaps it in
    /// `final`, so matched parents pick up the pinned copy. Reaching the top of
    /// the closure without passing a root means the failure is in the
    /// configuration itself and cannot be pinned.
    pub fn find_roots(
        &self,
        start: &str,
        within: &HashSet<String>,
        cap: usize,
        is_match: impl Fn(&str) -> bool,
    ) -> Ascent {
        let mut roots = BTreeSet::new();
        let mut seen = HashSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            if seen.len() > cap {
                return Ascent::TooLarge;
            }
            let parents: Vec<&String> = self.parents_within(&p, within).collect();
            if is_match(&p) {
                if parents.is_empty() || parents.iter().any(|q| !is_match(q)) {
                    roots.insert(p);
                } else {
                    stack.extend(parents.into_iter().cloned());
                }
            } else if parents.is_empty() {
                return Ascent::Unpinnable { via: p };
            } else {
                stack.extend(parents.into_iter().cloned());
            }
        }
        Ascent::Roots(roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &str) -> String {
        format!("/nix/store/{p}")
    }

    fn parse(json: serde_json::Value) -> Graph {
        Graph::from_show_json(&serde_json::to_vec(&json).unwrap(), "/nix/store").unwrap()
    }

    // toplevel -> system-path -> {obs, vlc}; obs -> libfoo; vlc -> libfoo;
    // toplevel -> etc -> unit -> libbar; obs-src fetch under obs.
    fn graph() -> Graph {
        parse(serde_json::json!({
            "version": 4,
            "derivations": {
                "t-toplevel.drv": { "name": "nixos-system", "env": {}, "inputs": { "drvs": { "sp-system-path.drv": {}, "e-etc.drv": {} }, "srcs": [] } },
                "sp-system-path.drv": { "name": "system-path", "env": {}, "inputs": { "drvs": { "o-obs-studio-31.drv": {}, "v-vlc-3.drv": {} } } },
                "o-obs-studio-31.drv": { "name": "obs-studio-31", "env": { "pname": "obs-studio" }, "inputs": { "drvs": { "l-libfoo-1.drv": {}, "s-obs-src.drv": {} } } },
                "v-vlc-3.drv": { "name": "vlc-3", "env": { "pname": "vlc" }, "inputs": { "drvs": { "l-libfoo-1.drv": {} } } },
                "l-libfoo-1.drv": { "name": "libfoo-1", "env": { "pname": "libfoo" }, "inputs": { "drvs": {} } },
                "s-obs-src.drv": { "name": "obs-src", "env": {}, "inputs": { "drvs": {} } },
                "e-etc.drv": { "name": "etc", "env": {}, "inputs": { "drvs": { "u-unit.drv": {} } } },
                "u-unit.drv": { "name": "unit-foo.service", "env": {}, "inputs": { "drvs": { "b-libbar-1.drv": {} } } },
                "b-libbar-1.drv": { "name": "libbar-1", "env": { "pname": "libbar" }, "inputs": { "drvs": {} } }
            }
        }))
    }

    fn all(g: &Graph) -> HashSet<String> {
        g.nodes.keys().cloned().collect()
    }

    fn matched(names: &[&str]) -> impl Fn(&str) -> bool {
        let set: HashSet<String> = names.iter().map(|n| s(n)).collect();
        move |p| set.contains(p)
    }

    #[test]
    fn parses_new_format() {
        let g = graph();
        let obs = &g.nodes[&s("o-obs-studio-31.drv")];
        assert_eq!(obs.pname.as_deref(), Some("obs-studio"));
        assert_eq!(obs.parents, vec![s("sp-system-path.drv")]);
        assert_eq!(g.nodes[&s("l-libfoo-1.drv")].parents.len(), 2);
    }

    #[test]
    fn structured_attrs_carry_pname_and_fixed_output() {
        let g = parse(serde_json::json!({
            "version": 4,
            "derivations": {
                "p-hello-2.12.drv": { "name": "hello-2.12", "env": { "out": "/nix/store/x-hello-2.12" },
                    "outputs": { "out": { "path": "x-hello-2.12" } },
                    "structuredAttrs": { "pname": "hello", "version": "2.12", "env": {} },
                    "inputs": { "drvs": { "f-hello-2.12.tar.gz.drv": { "outputs": ["out"] }, "c-stdenv-linux.drv": {} }, "srcs": [] } },
                "f-hello-2.12.tar.gz.drv": { "name": "hello-2.12.tar.gz", "env": { "out": "/nix/store/y-hello-2.12.tar.gz" },
                    "outputs": { "out": { "hash": "sha256-AAAA", "method": "flat" } },
                    "structuredAttrs": { "pname": "hello-tar", "urls": [] },
                    "inputs": { "drvs": {}, "srcs": [] } },
                "c-stdenv-linux.drv": { "name": "stdenv-linux", "env": { "pname": "stdenv-linux", "out": "/nix/store/z-stdenv-linux" },
                    "outputs": { "out": { "path": "z-stdenv-linux" } }, "structuredAttrs": null,
                    "inputs": { "drvs": {}, "srcs": [] } },
                "o-old-fetch.drv": { "name": "old-fetch", "env": { "outputHash": "sha256-BBBB", "pname": "old" },
                    "outputs": { "out": { "path": "w-old" } }, "structuredAttrs": null,
                    "inputs": { "drvs": {} } },
                "j-json.drv": { "name": "json-1", "env": { "__json": "{\"pname\":\"jsonpkg\",\"outputHash\":\"sha256-CCCC\"}" },
                    "inputs": { "drvs": {} } }
            }
        }));
        let hello = &g.nodes[&s("p-hello-2.12.drv")];
        assert_eq!(hello.pname.as_deref(), Some("hello"));
        assert!(!hello.fixed_output);
        assert!(g.is_package(&s("p-hello-2.12.drv")));
        let fetch = &g.nodes[&s("f-hello-2.12.tar.gz.drv")];
        assert!(fetch.fixed_output);
        assert!(!g.is_package(&s("f-hello-2.12.tar.gz.drv")));
        let classic = &g.nodes[&s("c-stdenv-linux.drv")];
        assert_eq!(classic.pname.as_deref(), Some("stdenv-linux"));
        assert!(!classic.fixed_output);
        assert!(g.nodes[&s("o-old-fetch.drv")].fixed_output);
        let json = &g.nodes[&s("j-json.drv")];
        assert_eq!(json.pname.as_deref(), Some("jsonpkg"));
        assert!(json.fixed_output);
    }

    #[test]
    fn other_store_dirs() {
        assert_eq!(store_dir_of("/nix/store/abc-hello.drv"), "/nix/store");
        assert_eq!(store_dir_of("/opt/store/abc-hello.drv"), "/opt/store");
        let g = Graph::from_show_json(
            &serde_json::to_vec(&serde_json::json!({
                "version": 4,
                "derivations": { "a-app.drv": { "name": "app", "env": {}, "inputs": { "drvs": { "b-lib.drv": {} } } },
                                 "b-lib.drv": { "name": "lib", "env": {}, "inputs": { "drvs": {} } } }
            }))
            .unwrap(),
            "/opt/store",
        )
        .unwrap();
        assert_eq!(
            g.nodes["/opt/store/b-lib.drv"].parents,
            vec!["/opt/store/a-app.drv".to_string()]
        );
    }

    #[test]
    fn parses_old_format() {
        let g = parse(serde_json::json!({
            "/nix/store/a-app.drv": { "name": "app", "env": { "pname": "app" }, "inputDrvs": { "/nix/store/b-lib.drv": ["out"] } },
            "/nix/store/b-lib.drv": { "env": { "name": "lib" }, "inputDrvs": {} }
        }));
        assert_eq!(g.nodes[&s("b-lib.drv")].parents, vec![s("a-app.drv")]);
        assert_eq!(g.nodes[&s("b-lib.drv")].name, "lib");
    }

    #[test]
    fn library_failure_pins_the_installed_apps() {
        let g = graph();
        let is_match = matched(&["o-obs-studio-31.drv", "v-vlc-3.drv", "l-libfoo-1.drv"]);
        assert_eq!(
            g.find_roots(&s("l-libfoo-1.drv"), &all(&g), 1000, is_match),
            Ascent::Roots([s("o-obs-studio-31.drv"), s("v-vlc-3.drv")].into())
        );
    }

    #[test]
    fn unnamed_dependency_resolves_upwards() {
        let g = graph();
        let is_match = matched(&["o-obs-studio-31.drv", "v-vlc-3.drv"]);
        assert_eq!(
            g.find_roots(&s("s-obs-src.drv"), &all(&g), 1000, is_match),
            Ascent::Roots([s("o-obs-studio-31.drv")].into())
        );
        assert_eq!(
            g.trail(&s("s-obs-src.drv"), &s("o-obs-studio-31.drv"), &all(&g)),
            vec!["obs-src".to_string(), "obs-studio-31".to_string()]
        );
    }

    #[test]
    fn package_used_by_glue_is_its_own_root() {
        let g = graph();
        let is_match = matched(&["b-libbar-1.drv"]);
        assert_eq!(
            g.find_roots(&s("b-libbar-1.drv"), &all(&g), 1000, is_match),
            Ascent::Roots([s("b-libbar-1.drv")].into())
        );
    }

    #[test]
    fn glue_failure_is_unpinnable() {
        let g = graph();
        let is_match = matched(&["o-obs-studio-31.drv"]);
        assert_eq!(
            g.find_roots(&s("u-unit.drv"), &all(&g), 1000, is_match),
            Ascent::Unpinnable {
                via: s("t-toplevel.drv")
            }
        );
    }

    #[test]
    fn caps_large_ascents() {
        let g = graph();
        let a = all(&g);
        assert_eq!(
            g.find_roots(&s("l-libfoo-1.drv"), &a, 2, |_| false),
            Ascent::TooLarge
        );
        assert!(g.ancestors(&s("l-libfoo-1.drv"), &a, 2).is_none());
        assert_eq!(g.ancestors(&s("l-libfoo-1.drv"), &a, 100).unwrap().len(), 5);
    }

    #[test]
    fn closure_limits_the_ascent_to_one_host() {
        // A second host whose top holds only vlc: libfoo there must resolve to
        // vlc alone, never to obs from the other host.
        let g = parse(serde_json::json!({
            "version": 4,
            "derivations": {
                "a-top.drv": { "name": "top-a", "env": {}, "inputs": { "drvs": { "o-obs-studio-31.drv": {} } } },
                "b-top.drv": { "name": "top-b", "env": {}, "inputs": { "drvs": { "v-vlc-3.drv": {} } } },
                "o-obs-studio-31.drv": { "name": "obs-studio-31", "env": { "pname": "obs-studio" }, "inputs": { "drvs": { "l-libfoo-1.drv": {} } } },
                "v-vlc-3.drv": { "name": "vlc-3", "env": { "pname": "vlc" }, "inputs": { "drvs": { "l-libfoo-1.drv": {} } } },
                "l-libfoo-1.drv": { "name": "libfoo-1", "env": { "pname": "libfoo" }, "inputs": { "drvs": {} } }
            }
        }));
        let b = g.closure(&s("b-top.drv"));
        assert_eq!(b.len(), 3);
        assert!(!g.closure(&s("a-top.drv")).contains(&s("v-vlc-3.drv")));
        let is_match = matched(&["o-obs-studio-31.drv", "v-vlc-3.drv", "l-libfoo-1.drv"]);
        assert_eq!(
            g.find_roots(&s("l-libfoo-1.drv"), &b, 1000, is_match),
            Ascent::Roots([s("v-vlc-3.drv")].into())
        );
        assert_eq!(
            g.trail(&s("l-libfoo-1.drv"), &s("v-vlc-3.drv"), &b),
            vec!["libfoo-1".to_string(), "vlc-3".to_string()]
        );
    }
}
