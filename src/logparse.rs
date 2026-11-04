use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A derivation that ran and failed: a broken package.
    Build { drv: String },
    /// A derivation that could not run here (wrong platform, no builder, a
    /// timeout, out of memory): not something a pin can fix.
    Infra { drv: String, reason: String },
    /// An attribute that no longer evaluates: missing, removed, or marked
    /// broken. Pinning it to the last good nixpkgs can fix that.
    Eval { attr: String },
    /// An attribute nixpkgs refuses for a reason that is a decision, not a
    /// breakage: marked insecure, unfree, or unsupported. Pinning around that
    /// would bypass nixpkgs' guard or the user's own settings.
    Refused { attr: String, reason: String },
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
static CANNOT_BUILD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Cannot build '(/[^']+\.drv)'\.\s*Reason: ([^\n]*)").unwrap());
static BUILDER_FAILED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"builder for '(/[^']+\.drv)' failed( due to signal [^\n;]*)?").unwrap()
});
static OLD_TIMEOUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:building of|builder for) '(/[^']+\.drv)' timed out[^\n]*").unwrap()
});
static OLD_DEPENDENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"dependenc(?:y|ies) of derivation '(/[^']+\.drv)' failed to build").unwrap()
});
static HASH_MISMATCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"hash mismatch in fixed-output derivation '(/[^']+\.drv)'").unwrap()
});
static MISSING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"attribute '([^']+)' missing").unwrap());
static REFUSED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Refusing to evaluate package '([^']+)' in (\S+?)(?::\d+)? because ((?s).*)")
        .unwrap()
});
static BY_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/pkgs/by-name/[^/]+/([^/]+)/package\.nix$").unwrap());
static REMOVED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"'([^']+)' has been (?:removed|renamed|replaced)").unwrap());
static VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+?)-[0-9]").unwrap());
static TRACE_OPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"while evaluating the option `(?:systemd\.(?:user\.)?)?services\."?([A-Za-z0-9_+-]+)"?"#,
    )
    .unwrap()
});
static TRACE_DRV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"while evaluating derivation '([^']+)'").unwrap());

pub fn strip_ansi(s: &str) -> String {
    ANSI.replace_all(s, "").into_owned()
}

pub fn strip_version(name: &str) -> &str {
    VERSION
        .captures(name)
        .and_then(|c| c.get(1))
        .map_or(name, |m| m.as_str())
}

/// The line of a nix error that says what went wrong. Messages open with a
/// bare "error:" and a trace of "while evaluating" frames (each led by an ellipsis); a failed assertion
/// lists its reasons as "- " items after "Failed assertions:".
pub fn headline(msg: &str) -> String {
    let msg = strip_ansi(msg);
    let lines: Vec<&str> = msg
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let informative = |l: &&str| {
        !l.starts_with('\u{2026}')
            && !l.starts_with("at ")
            && !l.contains('|')
            && *l != "error:"
            && !l.starts_with("(stack trace truncated")
    };
    if let Some(i) = lines.iter().position(|l| l.ends_with("Failed assertions:"))
        && let Some(item) = lines[i + 1..].iter().find(|l| l.starts_with("- "))
    {
        return item.trim_start_matches("- ").to_string();
    }
    let line = lines
        .iter()
        .rev()
        .find(|l| l.starts_with("error: ") && l.len() > "error: ".len())
        .or_else(|| lines.iter().rev().find(|l| informative(l)))
        .copied()
        .unwrap_or("");
    let line = line.strip_prefix("error: ").unwrap_or(line);
    line.trim_end_matches("Definition values:")
        .trim_end()
        .to_string()
}

/// Packages an evaluation error passed through on its way down, innermost
/// first: services from option paths (`systemd.services.open-webui...` names
/// open-webui) and derivations. When the package that broke is not pinnable
/// itself, e.g. a Python library, one of these usually is.
pub fn eval_context(messages: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for msg in messages {
        let msg = strip_ansi(msg);
        let mut found: Vec<(usize, String)> = TRACE_OPTION
            .captures_iter(&msg)
            .map(|c| (c.get(0).unwrap().start(), c[1].to_string()))
            .chain(
                TRACE_DRV
                    .captures_iter(&msg)
                    .map(|c| (c.get(0).unwrap().start(), strip_version(&c[1]).to_string())),
            )
            .collect();
        found.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        for (_, name) in found {
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// The "building '...'" and "copying path '...'" lines nix prints for its
/// builds and substitutions, from internal-json activity starts.
pub fn activity_text(line: &str) -> Option<String> {
    let json: Value = serde_json::from_str(line.strip_prefix("@nix ")?).ok()?;
    if json["action"] != "start" || !matches!(json["type"].as_u64(), Some(100) | Some(105)) {
        return None;
    }
    json["text"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(strip_ansi)
}

/// Whether a line of a nix command's internal-json stderr is an "error
/// (ignored)": nix carried on without it, as when commands running side by side
/// find its evaluation cache busy. Not a failure, and not worth showing.
pub fn is_ignored_error(line: &str) -> bool {
    error_message(line).is_some_and(|m| m.trim_start().starts_with("error (ignored)"))
}

/// The text of an error-level message from a `--log-format internal-json` stderr line.
pub fn error_message(line: &str) -> Option<String> {
    let json: Value = serde_json::from_str(line.strip_prefix("@nix ")?).ok()?;
    if json["action"] != "msg" || json["level"] != 0 {
        return None;
    }
    json["msg"].as_str().map(strip_ansi)
}

/// Whether a "Cannot build" reason means the build ran and failed, as opposed
/// to a dependency failing or the build not being able to run here.
fn reason_is_build_failure(reason: &str) -> bool {
    (reason.contains("builder failed") && !reason.contains("due to signal"))
        || reason.contains("hash mismatch")
}

/// Whether a refusal is only "marked broken", which a pin can fix, as opposed
/// to insecure, unfree or unsupported, which are decisions to respect.
fn refusal_is_breakage(why: &str) -> bool {
    let why = why.to_lowercase();
    why.contains("broken")
        && !why.contains("insecure")
        && !why.contains("unfree")
        && !why.contains("unsupported")
}

fn is_dependency_failure(reason: &str) -> bool {
    reason.contains("dependency failed") || reason.contains("dependencies failed")
}

pub fn parse(messages: &[String]) -> Vec<Failure> {
    let mut out = Vec::new();
    let mut push = |f: Failure| {
        if !out.contains(&f) {
            out.push(f);
        }
    };

    for msg in messages {
        let msg = strip_ansi(msg);

        let mut found_build = false;
        for c in HASH_MISMATCH.captures_iter(&msg) {
            found_build = true;
            push(Failure::Build {
                drv: c[1].to_string(),
            });
        }
        for c in CANNOT_BUILD.captures_iter(&msg) {
            found_build = true;
            let reason = c[2].trim().trim_end_matches('.').to_string();
            if is_dependency_failure(&reason) {
                continue;
            }
            if reason_is_build_failure(&reason) {
                push(Failure::Build {
                    drv: c[1].to_string(),
                });
            } else {
                push(Failure::Infra {
                    drv: c[1].to_string(),
                    reason,
                });
            }
        }
        for c in BUILDER_FAILED.captures_iter(&msg) {
            found_build = true;
            match c.get(2) {
                Some(signal) => push(Failure::Infra {
                    drv: c[1].to_string(),
                    reason: format!("builder failed{}", signal.as_str().trim_end_matches('.')),
                }),
                None => push(Failure::Build {
                    drv: c[1].to_string(),
                }),
            }
        }
        for c in OLD_TIMEOUT.captures_iter(&msg) {
            found_build = true;
            push(Failure::Infra {
                drv: c[1].to_string(),
                reason: "timed out".into(),
            });
        }
        if OLD_DEPENDENTS.is_match(&msg) {
            found_build = true;
        }
        if found_build {
            continue;
        }

        if let Some(c) = REFUSED.captures(&msg) {
            let attr = BY_NAME
                .captures(&c[2])
                .map(|b| b[1].to_string())
                .unwrap_or_else(|| strip_version(&c[1]).to_string());
            let why = c[3].to_string();
            if refusal_is_breakage(&why) {
                push(Failure::Eval { attr });
            } else {
                let reason = why
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches(':')
                    .to_string();
                push(Failure::Refused { attr, reason });
            }
        } else if let Some(c) = REMOVED.captures(&msg) {
            push(Failure::Eval {
                attr: c[1].to_string(),
            });
        } else if let Some(c) = MISSING.captures(&msg) {
            push(Failure::Eval {
                attr: c[1].to_string(),
            });
        }
    }
    out
}

/// Every derivation nix reports as not built, for whatever reason, including
/// the ones that only failed because a dependency did.
pub fn failed_drvs(messages: &[String]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for msg in messages {
        let msg = strip_ansi(msg);
        for re in [
            &*CANNOT_BUILD,
            &*BUILDER_FAILED,
            &*HASH_MISMATCH,
            &*OLD_TIMEOUT,
            &*OLD_DEPENDENTS,
        ] {
            for c in re.captures_iter(&msg) {
                out.insert(c[1].to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignored_errors_are_told_apart() {
        let line = |msg: &str| {
            format!(
                "@nix {}",
                serde_json::json!({ "action": "msg", "level": 0, "msg": msg })
            )
        };
        assert!(is_ignored_error(&line(
            "error (ignored): SQLite database 'x.sqlite' is busy"
        )));
        assert!(!is_ignored_error(&line("error: attribute 'x' missing")));
        assert!(!is_ignored_error("plain text"));
    }

    fn fixture_messages(text: &str) -> Vec<String> {
        text.lines().filter_map(error_message).collect()
    }

    #[test]
    fn new_build_failure_format_skips_dependents() {
        let msgs = fixture_messages(include_str!("../tests/fixtures/build-failure.jsonl"));
        assert_eq!(msgs.len(), 2);
        assert_eq!(
            parse(&msgs),
            vec![Failure::Build {
                drv: "/nix/store/w7w5jmhzl1x5489mv05b6ai9994bvzyx-libfoo-1.2.drv".into()
            }]
        );
    }

    #[test]
    fn hash_mismatch_is_a_build_failure() {
        let msgs = fixture_messages(include_str!("../tests/fixtures/hash-mismatch.jsonl"));
        let parsed = parse(&msgs);
        assert_eq!(parsed.len(), 1, "{parsed:?}");
        assert!(matches!(&parsed[0], Failure::Build { drv } if drv.ends_with("-fetched-1.0.drv")));
        let all = failed_drvs(&msgs);
        assert!(all.iter().any(|d| d.ends_with("-fetched-1.0.drv")));
        assert!(all.iter().any(|d| d.ends_with("-app-1.0.drv")));
    }

    #[test]
    fn platform_mismatch_is_infrastructure() {
        let msgs = fixture_messages(include_str!("../tests/fixtures/platform-mismatch.jsonl"));
        let parsed = parse(&msgs);
        assert_eq!(parsed.len(), 1, "{parsed:?}");
        assert!(
            matches!(&parsed[0], Failure::Infra { drv, reason } if drv.ends_with("-foreign-1.0.drv") && reason == "platform mismatch")
        );
    }

    #[test]
    fn signal_kills_are_infrastructure() {
        let msgs = vec!["error: Cannot build '/nix/store/abc-big-1.0.drv'.\n       Reason: builder failed due to signal 9 (Killed).".to_string()];
        assert_eq!(
            parse(&msgs),
            vec![Failure::Infra {
                drv: "/nix/store/abc-big-1.0.drv".into(),
                reason: "builder failed due to signal 9 (Killed)".into()
            }]
        );
    }

    #[test]
    fn other_stores_are_recognised() {
        let msgs = vec![
            "error: builder for '/opt/store/abc-foo-1.0.drv' failed with exit code 2".to_string(),
        ];
        assert_eq!(
            parse(&msgs),
            vec![Failure::Build {
                drv: "/opt/store/abc-foo-1.0.drv".into()
            }]
        );
    }

    #[test]
    fn old_build_failure_format() {
        let msgs = vec![
            "error: builder for '/nix/store/abc-foo-1.0.drv' failed with exit code 2".to_string(),
            "error: 1 dependencies of derivation '/nix/store/def-bar-1.0.drv' failed to build"
                .to_string(),
        ];
        assert_eq!(
            parse(&msgs),
            vec![Failure::Build {
                drv: "/nix/store/abc-foo-1.0.drv".into()
            }]
        );
        let all = failed_drvs(&msgs);
        assert!(all.contains("/nix/store/abc-foo-1.0.drv"));
        assert!(all.contains("/nix/store/def-bar-1.0.drv"));
    }

    #[test]
    fn old_signal_and_timeout_formats_are_infrastructure() {
        let msgs = vec![
            "error: builder for '/nix/store/abc-big-1.0.drv' failed due to signal 9 (Killed)"
                .to_string(),
            "error: building of '/nix/store/abc-slow-1.0.drv' timed out after 3600 seconds of silence"
                .to_string(),
        ];
        assert_eq!(
            parse(&msgs),
            vec![
                Failure::Infra {
                    drv: "/nix/store/abc-big-1.0.drv".into(),
                    reason: "builder failed due to signal 9 (Killed)".into()
                },
                Failure::Infra {
                    drv: "/nix/store/abc-slow-1.0.drv".into(),
                    reason: "timed out".into()
                },
            ]
        );
        assert_eq!(failed_drvs(&msgs).len(), 2);
    }

    #[test]
    fn eval_missing_attribute() {
        let msgs = fixture_messages(include_str!("../tests/fixtures/eval-missing.jsonl"));
        assert_eq!(
            parse(&msgs),
            vec![Failure::Eval {
                attr: "nodejs_18".into()
            }]
        );
    }

    #[test]
    fn eval_refused_prefers_by_name_directory() {
        let msgs = vec![
            "error: Refusing to evaluate package 'hello-2.12.3' in /nix/store/x-source/pkgs/by-name/he/hello/package.nix:16 because it has problems:\n- broken: This package is broken.".to_string(),
            "error: Refusing to evaluate package 'cuda-merged-12.9' in /nix/store/x-source/pkgs/development/cuda-modules/packages/cudatoolkit.nix:81 because it has an unfree license (\u{2018}CUDA EULA\u{2019})".to_string(),
            "error: Refusing to evaluate package 'hello-2.12.3' in /nix/store/x-source/pkgs/by-name/he/hello/package.nix:16 because it is marked as insecure".to_string(),
        ];
        assert_eq!(
            parse(&msgs),
            vec![
                Failure::Eval {
                    attr: "hello".into()
                },
                Failure::Refused {
                    attr: "cuda-merged".into(),
                    reason: "it has an unfree license (\u{2018}CUDA EULA\u{2019})".into()
                },
                Failure::Refused {
                    attr: "hello".into(),
                    reason: "it is marked as insecure".into()
                },
            ]
        );
    }

    #[test]
    fn eval_removed_alias() {
        let msgs = vec![
            "error: '_0verkill' has been removed due to lack of maintenance".to_string(),
            "error: '_1password' has been renamed to/replaced by '_1password-cli'".to_string(),
        ];
        assert_eq!(
            parse(&msgs),
            vec![
                Failure::Eval {
                    attr: "_0verkill".into()
                },
                Failure::Eval {
                    attr: "_1password".into()
                },
            ]
        );
    }

    #[test]
    fn activity_lines() {
        let started: Vec<String> = include_str!("../tests/fixtures/build-failure.jsonl")
            .lines()
            .filter_map(activity_text)
            .collect();
        assert!(started.contains(
            &"building '/nix/store/w7w5jmhzl1x5489mv05b6ai9994bvzyx-libfoo-1.2.drv'".to_string()
        ));
        assert!(
            started
                .iter()
                .all(|t| t.starts_with("building ") || t.starts_with("copying path "))
        );
        assert_eq!(
            activity_text(r#"@nix {"action":"start","id":1,"level":4,"parent":0,"text":"copying path '/nix/store/x-a' from 'https://cache.nixos.org'","type":100}"#).as_deref(),
            Some("copying path '/nix/store/x-a' from 'https://cache.nixos.org'")
        );
        assert_eq!(
            activity_text(
                r#"@nix {"action":"start","id":2,"level":0,"parent":0,"text":"","type":102}"#
            ),
            None
        );
    }

    #[test]
    fn ignores_non_error_lines() {
        assert_eq!(
            error_message(
                r#"@nix {"action":"msg","level":3,"msg":"these 2 derivations will be built:"}"#
            ),
            None
        );
        assert_eq!(error_message("plain text"), None);
        assert!(parse(&["error: something unrelated".to_string()]).is_empty());
    }

    #[test]
    fn headline_skips_the_trace() {
        let assertion = "error:\n       \u{2026} while calling the 'head' builtin\n         at /nix/store/x/lib/attrsets.nix:1711:13:\n         1710|           if x then\n\n       error:\n       Failed assertions:\n       - The option definition `boot.initrd.systemd.enable' in `/x/boot-unlock.nix' no longer has any effect; please remove it.\n       Scripted initrd has been removed.\n";
        assert_eq!(
            headline(assertion),
            "The option definition `boot.initrd.systemd.enable' in `/x/boot-unlock.nix' no longer has any effect; please remove it."
        );
        let plain = "error:\n       \u{2026} while evaluating the attribute 'x'\n\n       (stack trace truncated; use '--show-trace' to show the full, detailed trace)\n\n       error: undefined variable 'foo'\n";
        assert_eq!(headline(plain), "undefined variable 'foo'");
        assert_eq!(headline("error: something broke"), "something broke");
        let option = "error:\n       \u{2026} while evaluating the attribute 'config'\n\n       error: The option `services.gone' does not exist. Definition values:\n       - In `/nix/store/x-source/hosts/b.nix': true\n";
        assert_eq!(
            headline(option),
            "The option `services.gone' does not exist."
        );
    }

    #[test]
    fn eval_context_innermost_first() {
        let msg = "error:\n       \u{2026} while evaluating the option `system.build.toplevel':\n       \u{2026} while evaluating the option `warnings':\n       \u{2026} while evaluating the option `systemd.services.open-webui.serviceConfig':\n       \u{2026} while evaluating derivation 'open-webui-0.12.3'\n       error: Refusing to evaluate package 'python3.14-posthog-7.71.0' in /nix/store/x-source/pkgs/development/python-modules/posthog/default.nix:80 because it has problems:\n       - broken: This package is broken.\n".to_string();
        assert_eq!(
            eval_context(std::slice::from_ref(&msg)),
            vec!["open-webui".to_string()]
        );
        assert_eq!(
            parse(&[msg]),
            vec![Failure::Eval {
                attr: "python3.14-posthog".into()
            }]
        );
        let quoted = "\u{2026} while evaluating the option `services.\"nginx\".virtualHosts':\n\u{2026} while evaluating the option `systemd.user.services.foo.serviceConfig':".to_string();
        assert_eq!(
            eval_context(&[quoted]),
            vec!["foo".to_string(), "nginx".to_string()]
        );
    }

    #[test]
    fn version_stripping() {
        assert_eq!(strip_version("obs-studio-31.0.1"), "obs-studio");
        assert_eq!(strip_version("hello"), "hello");
        assert_eq!(
            strip_version("python3.13-requests-2.32"),
            "python3.13-requests"
        );
    }
}
