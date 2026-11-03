use crate::pins::Kind;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

/// What the NixOS or Home Manager module writes to /etc/nixheal.json or
/// ~/.config/nixheal.json.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub kind: Kind,
    #[serde(rename = "self")]
    pub self_name: String,
    #[serde(default)]
    pub flake: Option<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub homes: Vec<String>,
    /// The flake input this machine's pkgs come from.
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default = "default_warn_after_days")]
    pub warn_after_days: i64,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub quiet: bool,
    /// Commit flake.lock and pins.json after a run, as --commit does.
    #[serde(default)]
    pub commit: bool,
    /// How to activate: "nh" (the default) or "native", which is
    /// nixos-rebuild for hosts and home-manager for homes.
    #[serde(default = "default_switcher")]
    pub switcher: String,
    /// Flake inputs a failed package may be pinned to when they have a newer
    /// version of it that is already built.
    #[serde(default)]
    pub donors: Vec<String>,
    /// Flake inputs never to roll back when they break a build. A trailing *
    /// matches a prefix, so "*" turns rolling back off.
    #[serde(default)]
    pub input_deny: Vec<String>,
    /// The package sets besides `input`, never rolled back.
    #[serde(default)]
    pub package_sets: Vec<String>,
    /// Retry a package whose drv the update did not change only once it was
    /// last tried this many days ago; 0 retries it whenever its input moves.
    #[serde(default = "default_retry_same_after_days")]
    pub retry_same_derivation_after_days: i64,
}

fn default_switcher() -> String {
    "nh".into()
}

fn default_warn_after_days() -> i64 {
    14
}

fn default_retry_same_after_days() -> i64 {
    7
}

pub const ROOT_PREFIX: &str = "nixheal-";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub kind: Kind,
    pub name: String,
}

impl Target {
    pub fn new(kind: Kind, name: &str) -> Self {
        Self {
            kind,
            name: name.to_string(),
        }
    }

    fn base(&self) -> String {
        match self.kind {
            Kind::Nixos => format!("nixosConfigurations.\"{}\"", self.name),
            Kind::Home => format!("homeConfigurations.\"{}\"", self.name),
        }
    }

    pub fn output(&self) -> String {
        match self.kind {
            Kind::Nixos => format!("{}.config.system.build.toplevel", self.base()),
            Kind::Home => format!("{}.activationPackage", self.base()),
        }
    }

    /// The system the configuration is built for, e.g. aarch64-linux for a Pi.
    pub fn system_attr(&self) -> String {
        format!("{}.pkgs.stdenv.hostPlatform.system", self.base())
    }

    /// The name of this target's GC root under --gc-roots. The prefix marks
    /// the roots nixheal made, the only ones it ever removes there.
    pub fn root_name(&self) -> String {
        match self.kind {
            Kind::Nixos => format!("{ROOT_PREFIX}host-{}", self.name),
            Kind::Home => format!("{ROOT_PREFIX}home-{}", self.name),
        }
    }

    pub fn nixheal_config(&self) -> String {
        format!("{}.config.programs.nixheal", self.base())
    }

    pub fn package_sets(&self) -> String {
        format!("{}.config.programs.nixheal.packageSets", self.base())
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind.noun(), self.name)
    }
}

impl Config {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn local(&self) -> Target {
        Target::new(self.kind, &self.self_name)
    }

    pub fn targets(&self) -> Vec<Target> {
        self.hosts
            .iter()
            .map(|h| Target::new(Kind::Nixos, h))
            .chain(self.homes.iter().map(|h| Target::new(Kind::Home, h)))
            .collect()
    }
}

fn user_config() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("nixheal.json"))
}

/// Where the config in effect is, and whether it was asked for: an explicit
/// path, then $NIXHEAL_CONFIG, then the Home Manager file, then the NixOS one.
pub struct Located {
    pub path: PathBuf,
    /// Named by --config or $NIXHEAL_CONFIG, so it must work.
    pub explicit: bool,
    /// Where the path came from, for error messages.
    pub from: &'static str,
}

pub fn locate(explicit: Option<&Path>) -> Option<Located> {
    if let Some(p) = explicit {
        return Some(Located {
            path: p.to_path_buf(),
            explicit: true,
            from: "--config",
        });
    }
    if let Some(p) = std::env::var_os("NIXHEAL_CONFIG") {
        return Some(Located {
            path: PathBuf::from(p),
            explicit: true,
            from: "$NIXHEAL_CONFIG",
        });
    }
    [user_config(), Some(PathBuf::from("/etc/nixheal.json"))]
        .into_iter()
        .flatten()
        .find(|p| p.exists())
        .map(|path| Located {
            path,
            explicit: false,
            from: "the default locations",
        })
}

pub fn read(path: &Path) -> Result<Config> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Config::parse(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// What `nixheal` with no config at all says.
pub fn missing() -> anyhow::Error {
    anyhow::anyhow!(
        "no nixheal config: set programs.nixheal.enable and hosts (or homes) and switch once, which writes /etc/nixheal.json (~/.config/nixheal.json for Home Manager), or pass --config FILE"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_module_output() {
        let c = Config::parse(
            br#"{"kind":"nixos","self":"laptop","flake":"/home/alice/nixos","hosts":["laptop","pi"],"homes":["alice"],"inputs":["nixpkgs","nixpkgs-unstable"],"warnAfterDays":7,"deny":["cuda*"]}"#,
        )
        .unwrap();
        assert_eq!(c.local(), Target::new(Kind::Nixos, "laptop"));
        assert_eq!(c.targets().len(), 3);
        assert_eq!(c.warn_after_days, 7);
        assert_eq!(c.targets()[2], Target::new(Kind::Home, "alice"));
    }

    #[test]
    fn defaults() {
        let c = Config::parse(br#"{"kind":"home","self":"alice"}"#).unwrap();
        assert_eq!(c.warn_after_days, 14);
        assert!(c.targets().is_empty());
        assert!(c.flake.is_none());
    }

    #[test]
    fn target_attrs() {
        let h = Target::new(Kind::Nixos, "laptop");
        assert_eq!(
            h.output(),
            "nixosConfigurations.\"laptop\".config.system.build.toplevel"
        );
        assert_eq!(
            h.package_sets(),
            "nixosConfigurations.\"laptop\".config.programs.nixheal.packageSets"
        );
        assert_eq!(h.to_string(), "host laptop");
        assert_eq!(h.root_name(), "nixheal-host-laptop");
        assert_eq!(
            Target::new(Kind::Home, "laptop").root_name(),
            "nixheal-home-laptop"
        );
        let m = Target::new(Kind::Home, "alice@wsl");
        assert_eq!(
            m.output(),
            "homeConfigurations.\"alice@wsl\".activationPackage"
        );
    }
}
