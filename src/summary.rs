use crate::config::Target;
use crate::date;
use crate::engine::describe;
use crate::logparse::strip_version;
use crate::pins::{InputPin, Pin, PinsFile, Reason};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

struct Item {
    what: String,
    kind: &'static str,
    days: Option<i64>,
    /// The label after the age: for a pin what happened and what it
    /// provides, for a hold why.
    detail: String,
    /// Structured fields for scripts, next to the display strings.
    fields: Value,
}

/// The name of a derivation from its store path, e.g. "cloudflared-2026.10.3".
pub fn drv_name(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    let base = base.strip_suffix(".drv").unwrap_or(base);
    base.split_once('-').map_or(base, |(_, n)| n)
}

/// The version part of a derivation name, "" when it has none.
pub fn name_version(name: &str) -> &str {
    name[strip_version(name).len()..].trim_start_matches('-')
}

/// How old something is, as status and the dashboards show it.
pub fn age_text(days: Option<i64>) -> String {
    match days {
        Some(0) => "today".into(),
        Some(d) => format!("{d}d"),
        None => "?".into(),
    }
}

/// What made eppur pin it, e.g. "cloudflared-2026.10.3 failed to build".
pub fn cause(pin: &Pin) -> String {
    match pin.reason {
        Reason::Build => match &pin.failed_drv {
            Some(drv) => format!("{} failed to build", drv_name(drv)),
            None => "failed to build".into(),
        },
        Reason::Eval => "evaluation failed".into(),
        Reason::Manual => "pinned by hand".into(),
    }
}

/// What made eppur pin a flake input.
pub fn input_cause(pin: &InputPin) -> String {
    match (&pin.failed_drv, &pin.error) {
        (Some(drv), _) => format!("{} failed to build", drv_name(drv)),
        (None, Some(error)) if !error.is_empty() => format!("evaluation failed: {error}"),
        _ => "evaluation failed".into(),
    }
}

/// What made eppur pin a flake input, short enough for one row: the error
/// of an evaluation failure is cut to one line.
pub fn input_cause_short(pin: &InputPin) -> String {
    match (&pin.failed_drv, pin.error.as_deref().map(one_line)) {
        (Some(drv), _) => format!("{} failed to build", drv_name(drv)),
        (None, Some(error)) if !error.is_empty() => format!("evaluation failed: {error}"),
        _ => "evaluation failed".into(),
    }
}

/// The targets a flake input pin is for, e.g. "breaks host pi, home alice";
/// "" when none is recorded. The cause is the first of them to fail, so it
/// is kept apart from the list.
pub fn input_targets(pin: &InputPin) -> String {
    if pin.targets.is_empty() {
        String::new()
    } else {
        format!("breaks {}", pin.targets.join(", "))
    }
}

/// Where a flake input stands: kept at the last good revision in flake.lock,
/// or with flake.lock on another revision for the next build to try.
pub fn input_state(pin: &InputPin) -> String {
    if pin.retrying {
        format!(
            "last good {}, flake.lock has another revision to try",
            describe(&pin.locked)
        )
    } else {
        format!("kept at {}", describe(&pin.locked))
    }
}

/// A flake input pin's label after its age: "aws-lc-5.12.0 failed to build;
/// breaks host pi; kept at ff459c63e3bb".
pub fn input_detail(pin: &InputPin) -> String {
    [input_cause_short(pin), input_targets(pin), input_state(pin)]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

static STORE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/nix/store/[0-9a-z]{32}-([A-Za-z0-9+._?=-]+)").unwrap());

/// A reason or error short enough for one row: its first non-empty line, with
/// store paths shortened to their names, cut at 80 characters.
pub fn one_line(text: &str) -> String {
    const MAX: usize = 80;
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let line = STORE_PATH.replace_all(line, |c: &regex::Captures| {
        c[1].strip_suffix(".drv").unwrap_or(&c[1]).to_string()
    });
    if line.chars().count() <= MAX {
        line.into_owned()
    } else {
        let cut: String = line.chars().take(MAX - 3).collect();
        format!("{}...", cut.trim_end())
    }
}

/// What the pin provides: "2026.10.3 from nixpkgs-stable", "0.6.30 at
/// 718f670e8453", or without a recorded version "nixpkgs-stable at
/// 08d3a97586f6" and "718f670e8453"; "" when nothing is known.
pub fn provides(pin: &Pin) -> String {
    let rev = pin.locked["rev"].as_str().map(|_| describe(&pin.locked));
    match (&pin.version, &pin.donor, rev) {
        (Some(v), Some(d), _) => format!("{v} from {d}"),
        (Some(v), None, Some(r)) => format!("{v} at {r}"),
        (Some(v), None, None) => v.clone(),
        (None, Some(d), Some(r)) => format!("{d} at {r}"),
        (None, Some(d), None) => d.clone(),
        (None, None, Some(r)) => r,
        (None, None, None) => String::new(),
    }
}

/// A flat summary of a pins file for dashboards, e.g. a homepage customapi
/// widget: how much is pinned or held, the oldest of it, and one status word.
///
/// status is the most urgent of: STALE (a pin or hold older than
/// warn_after_days), HELD, PINNED, OK.
pub fn summary(
    file: &PinsFile,
    today: i64,
    warn_after_days: i64,
    retry_same_after_days: i64,
) -> Value {
    let mut pins = 0;
    let mut pin_entries = 0;
    let mut held = 0;
    let mut items: Vec<Item> = Vec::new();
    for (kind, name, entry) in file.entries() {
        let target = Target::new(kind, name);
        if let Some(h) = &entry.held {
            held += 1;
            items.push(Item {
                what: format!("{target} held"),
                kind: "held",
                days: date::parse(&h.since).map(|d| today - d),
                detail: match one_line(&h.reason) {
                    r if r.is_empty() => "held".into(),
                    r => format!("held: {r}"),
                },
                fields: json!({
                    "target": name,
                    "target_kind": kind.noun(),
                    "since": h.since,
                    "held_reason": h.reason,
                }),
            });
        }
        for (attr, sets) in entry.packages() {
            pins += 1;
            pin_entries += sets.len();
            let pin = sets[0].1;
            let inputs: Vec<&str> = sets.iter().map(|(i, _)| *i).collect();
            let provided = provides(pin);
            let detail = if provided.is_empty() {
                cause(pin)
            } else {
                format!("{}; pinned to {provided}", cause(pin))
            };
            let failed_version = pin
                .root_drv
                .as_deref()
                .or(pin.failed_drv.as_deref())
                .map(|d| name_version(drv_name(d)))
                .filter(|v| !v.is_empty());
            let retry_from = (pin.reason == Reason::Build
                && pin.root_drv.is_some()
                && retry_same_after_days > 0)
                .then(|| date::parse(pin.tried.as_deref().unwrap_or(&pin.since)))
                .flatten()
                .map(|d| date::format(d + retry_same_after_days));
            let per_set: Vec<Value> = sets
                .iter()
                .map(|(i, p)| {
                    json!({
                        "input": i,
                        "version": p.version,
                        "donor": p.donor,
                        "rev": p.locked["rev"].as_str().map(|_| describe(&p.locked)),
                        "rev_date": p.locked["lastModified"].as_i64().map(|t| date::format(t / 86_400)),
                    })
                })
                .collect();
            items.push(Item {
                what: format!("{target}: {attr}"),
                kind: pin.reason.as_str(),
                days: date::parse(&pin.since).map(|d| today - d),
                detail,
                fields: json!({
                    "target": name,
                    "target_kind": kind.noun(),
                    "inputs": inputs,
                    "attr": attr,
                    "since": pin.since,
                    "cause": cause(pin),
                    "failed": pin.failed_drv.as_deref().map(drv_name),
                    "failed_version": failed_version,
                    "version": pin.version,
                    "donor": pin.donor,
                    "rev": pin.locked["rev"].as_str().map(|_| describe(&pin.locked)),
                    "rev_date": pin.locked["lastModified"].as_i64().map(|t| date::format(t / 86_400)),
                    "tried": pin.tried,
                    "retry_from": retry_from,
                    "error": pin.error,
                    "sets": per_set,
                }),
            });
        }
    }
    for (input, pin) in &file.inputs {
        pins += 1;
        pin_entries += 1;
        items.push(Item {
            what: format!("flake input {input}"),
            kind: "input",
            days: date::parse(&pin.since).map(|d| today - d),
            detail: input_detail(pin),
            fields: json!({
                "input": input,
                "targets": pin.targets,
                "since": pin.since,
                "cause": input_cause(pin),
                "failed": pin.failed_drv.as_deref().map(drv_name),
                "rev": pin.locked["rev"].as_str().map(|_| describe(&pin.locked)),
                "rev_date": pin.locked["lastModified"].as_i64().map(|t| date::format(t / 86_400)),
                "retrying": pin.retrying,
                "error": pin.error,
            }),
        });
    }
    items.sort_by_key(|i| std::cmp::Reverse(i.days.unwrap_or(i64::MAX)));

    let is_stale = |i: &Item| i.kind != "manual" && i.days.is_none_or(|d| d > warn_after_days);
    let stale = items.iter().filter(|i| is_stale(i)).count();
    let status = if stale > 0 {
        "STALE"
    } else if held > 0 {
        "HELD"
    } else if pins > 0 {
        "PINNED"
    } else {
        "OK"
    };
    let age = |i: &Item| age_text(i.days);
    let oldest = items.first();
    let list = if items.is_empty() {
        "nothing pinned".to_string()
    } else {
        items
            .iter()
            .map(|i| format!("{} ({})", i.what, age(i)))
            .collect::<Vec<_>>()
            .join(", ")
    };

    // One row per pin or hold, for a dynamic-list widget; never empty, so the
    // widget shows that all is well instead of nothing.
    let rows: Vec<Value> = if items.is_empty() {
        vec![
            json!({ "name": "nothing pinned", "label": "OK", "kind": "none", "days": 0, "stale": false }),
        ]
    } else {
        items
            .iter()
            .map(|i| {
                let mut label = format!("{}, {}", age(i), i.detail);
                if is_stale(i) {
                    label.push_str(", STALE");
                }
                let mut row = json!({
                    "name": i.what,
                    "label": label,
                    "kind": i.kind,
                    "days": i.days,
                    "stale": is_stale(i),
                });
                if let (Some(row), Some(fields)) = (row.as_object_mut(), i.fields.as_object()) {
                    row.extend(fields.clone());
                }
                row
            })
            .collect()
    };

    json!({
        "status": status,
        "pins": pins,
        "pin_entries": pin_entries,
        "input_pins": file.inputs.len(),
        "held": held,
        "stale": stale,
        "oldest": oldest.map_or("-".to_string(), |i| format!("{}, {}", i.what, age(i))),
        "oldest_days": oldest.and_then(|i| i.days).unwrap_or(0),
        "list": list,
        "items": rows,
        "warn_after_days": warn_after_days,
        "generated": date::format(today),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pins::{Held, Kind, Pin, Reason};

    fn pin(since: &str) -> Pin {
        Pin {
            locked: json!({ "narHash": "sha256-x" }),
            since: since.into(),
            reason: Reason::Eval,
            failed_drv: None,
            trail: Vec::new(),
            error: None,
            overlays: None,
            checked_nar_hash: String::new(),
            root_drv: None,
            tried: None,
            donor: None,
            leaf: false,
            version: None,
        }
    }

    fn today() -> i64 {
        date::parse("2026-10-27").unwrap()
    }

    #[test]
    fn nothing_pinned_is_ok() {
        let mut f = PinsFile::default();
        f.entry_mut(Kind::Nixos, "laptop");
        let s = summary(&f, today(), 14, 7);
        assert_eq!(s["status"], "OK");
        assert_eq!(s["pins"], 0);
        assert_eq!(s["oldest"], "-");
        assert_eq!(s["list"], "nothing pinned");
        assert_eq!(s["items"][0]["name"], "nothing pinned");
        assert_eq!(s["items"][0]["label"], "OK");
    }

    #[test]
    fn fresh_pins_are_pinned_and_oldest_first() {
        let mut f = PinsFile::default();
        f.entry_mut(Kind::Nixos, "home")
            .insert("nixpkgs", "open-webui", pin("2026-10-20"));
        f.entry_mut(Kind::Nixos, "pi")
            .insert("nixpkgs", "sbctl", pin("2026-10-26"));
        let s = summary(&f, today(), 14, 7);
        assert_eq!(s["status"], "PINNED");
        assert_eq!(s["pins"], 2);
        assert_eq!(s["oldest"], "host home: open-webui, 7d");
        assert_eq!(s["oldest_days"], 7);
        assert_eq!(s["list"], "host home: open-webui (7d), host pi: sbctl (1d)");
        assert_eq!(s["items"][0]["name"], "host home: open-webui");
        assert_eq!(s["items"][0]["label"], "7d, evaluation failed");
        assert_eq!(s["items"][1]["name"], "host pi: sbctl");
        assert_eq!(s["items"].as_array().unwrap().len(), 2);

        f.entry_mut(Kind::Nixos, "pi")
            .insert("nixpkgs-unstable", "sbctl", pin("2026-10-26"));
        let s = summary(&f, today(), 14, 7);
        assert_eq!(s["pins"], 2);
        assert_eq!(s["pin_entries"], 3);
        assert_eq!(
            s["items"][1]["inputs"],
            json!(["nixpkgs", "nixpkgs-unstable"])
        );
    }

    #[test]
    fn held_and_stale() {
        let mut f = PinsFile::default();
        f.entry_mut(Kind::Nixos, "home")
            .insert("nixpkgs", "open-webui", pin("2026-10-26"));
        f.entry_mut(Kind::Nixos, "pi").held = Some(Held {
            since: "2026-10-25".into(),
            reason: "linux fails".into(),
            lock: json!({}),
        });
        let s = summary(&f, today(), 14, 7);
        assert_eq!(s["status"], "HELD");
        assert_eq!(s["held"], 1);

        f.entry_mut(Kind::Home, "alice")
            .insert("nixpkgs", "zed-editor", pin("2026-10-01"));
        let s = summary(&f, today(), 14, 7);
        assert_eq!(s["status"], "STALE");
        assert_eq!(s["stale"], 1);
        assert_eq!(s["oldest"], "home alice: zed-editor, 26d");
        assert_eq!(s["items"][0]["label"], "26d, evaluation failed, STALE");
        assert_eq!(s["items"][0]["name"], "home alice: zed-editor");
        assert_eq!(s["items"][1]["name"], "host pi held");
        assert_eq!(s["items"][1]["label"], "2d, held: linux fails");
        assert_eq!(s["items"][1]["held_reason"], "linux fails");
        assert_eq!(s["items"][0]["target"], "alice");
        assert_eq!(s["items"][0]["target_kind"], "home");
        assert_eq!(s["items"][0]["inputs"][0], "nixpkgs");
        assert_eq!(s["items"][0]["attr"], "zed-editor");
    }

    #[test]
    fn a_donor_build_pin_says_what_failed_and_what_it_provides() {
        let mut f = PinsFile::default();
        let mut p = pin("2026-10-26");
        p.reason = Reason::Build;
        p.locked =
            json!({ "rev": "08d3a97586f6aaaa", "lastModified": 1793145600, "narHash": "sha256-s" });
        p.failed_drv =
            Some("/nix/store/ygnpfgx5lwmd0fk0c1sg49qk4v6sfgb5-cloudflared-2026.10.3.drv".into());
        p.root_drv = p.failed_drv.clone();
        p.donor = Some("nixpkgs-stable".into());
        p.version = Some("2026.10.3".into());
        p.tried = Some("2026-10-27".into());
        f.entry_mut(Kind::Nixos, "pi")
            .insert("nixpkgs", "cloudflared", p);
        let s = summary(&f, today(), 14, 7);
        let row = &s["items"][0];
        assert_eq!(
            row["label"],
            "1d, cloudflared-2026.10.3 failed to build; pinned to 2026.10.3 from nixpkgs-stable"
        );
        assert_eq!(row["cause"], "cloudflared-2026.10.3 failed to build");
        assert_eq!(row["failed"], "cloudflared-2026.10.3");
        assert_eq!(row["failed_version"], "2026.10.3");
        assert_eq!(row["version"], "2026.10.3");
        assert_eq!(row["donor"], "nixpkgs-stable");
        assert_eq!(row["rev"], "08d3a97586f6");
        assert_eq!(row["rev_date"], "2026-10-28");
        assert_eq!(row["retry_from"], "2026-11-03");
        assert_eq!(row["sets"][0]["input"], "nixpkgs");
        assert_eq!(row["sets"][0]["donor"], "nixpkgs-stable");
    }

    #[test]
    fn a_version_at_a_revision() {
        let mut f = PinsFile::default();
        let mut p = pin("2026-10-25");
        p.locked = json!({ "rev": "718f670e8453bbbb", "narHash": "sha256-o" });
        p.version = Some("0.6.30".into());
        f.entry_mut(Kind::Nixos, "home")
            .insert("nixpkgs", "open-webui", p);
        let s = summary(&f, today(), 14, 7);
        assert_eq!(
            s["items"][0]["label"],
            "2d, evaluation failed; pinned to 0.6.30 at 718f670e8453"
        );
        assert_eq!(s["items"][0]["retry_from"], Value::Null);
    }

    #[test]
    fn a_pin_without_a_version_names_its_revision() {
        let mut p = pin("2026-10-25");
        p.locked = json!({ "rev": "08d3a97586f6aaaa", "narHash": "sha256-s" });
        assert_eq!(provides(&p), "08d3a97586f6");
        p.donor = Some("nixpkgs-stable".into());
        assert_eq!(provides(&p), "nixpkgs-stable at 08d3a97586f6");
    }

    #[test]
    fn derivation_names_and_versions() {
        assert_eq!(
            drv_name("/nix/store/aaaa-cloudflared-2026.10.3.drv"),
            "cloudflared-2026.10.3"
        );
        assert_eq!(name_version("cloudflared-2026.10.3"), "2026.10.3");
        assert_eq!(name_version("hello-broken"), "");
    }

    #[test]
    fn manual_pins_never_go_stale() {
        let mut f = PinsFile::default();
        let mut p = pin("2026-01-01");
        p.reason = Reason::Manual;
        f.entry_mut(Kind::Nixos, "laptop")
            .insert("nixpkgs", "hello", p);
        let s = summary(&f, today(), 14, 7);
        assert_eq!(s["status"], "PINNED");
        assert_eq!(s["stale"], 0);
        assert_eq!(s["items"][0]["label"], "299d, pinned by hand");
    }

    fn input_pin() -> InputPin {
        InputPin {
            locked: json!({ "rev": "ff459c63e3bbcccc", "narHash": "sha256-g" }),
            failed_nar_hash: "sha256-b".into(),
            since: "2026-10-24".into(),
            reason: Reason::Build,
            failed_drv: Some("/nix/store/aaaa-aws-lc-5.12.0.drv".into()),
            error: None,
            targets: vec!["host pi".into()],
            retrying: false,
        }
    }

    fn input_label(pin: InputPin) -> Value {
        let mut f = PinsFile::default();
        f.inputs.insert("atlas".into(), pin);
        let s = summary(&f, today(), 14, 7);
        assert_eq!(s["items"][0]["name"], "flake input atlas");
        s["items"][0].clone()
    }

    #[test]
    fn an_input_pin_names_its_targets_apart_from_the_cause() {
        let mut p = input_pin();
        p.targets.push("home alice".into());
        let row = input_label(p);
        assert_eq!(
            row["label"],
            "3d, aws-lc-5.12.0 failed to build; breaks host pi, home alice; kept at ff459c63e3bb"
        );
        assert_eq!(row["targets"], json!(["host pi", "home alice"]));
        assert_eq!(row["cause"], "aws-lc-5.12.0 failed to build");
    }

    #[test]
    fn a_retrying_input_pin_names_its_last_good_revision() {
        let mut p = input_pin();
        p.retrying = true;
        assert_eq!(
            input_label(p)["label"],
            "3d, aws-lc-5.12.0 failed to build; breaks host pi; last good ff459c63e3bb, flake.lock has another revision to try"
        );
    }

    #[test]
    fn an_input_pin_cuts_its_evaluation_error_to_one_line() {
        let mut p = input_pin();
        p.failed_drv = None;
        p.error = Some(format!(
            "attribute 'foo' missing {}\nat line 2",
            "y".repeat(100)
        ));
        let row = input_label(p);
        let label = row["label"].as_str().unwrap();
        assert!(
            label.starts_with("3d, evaluation failed: attribute 'foo' missing yyy"),
            "{label}"
        );
        assert!(
            label.ends_with("...; breaks host pi; kept at ff459c63e3bb"),
            "{label}"
        );
        assert!(!label.contains("line 2"), "{label}");
        assert!(row["error"].as_str().unwrap().contains("line 2"));
    }

    #[test]
    fn an_input_pin_without_targets_or_error() {
        let mut p = input_pin();
        p.failed_drv = None;
        p.targets.clear();
        assert_eq!(
            input_label(p)["label"],
            "3d, evaluation failed; kept at ff459c63e3bb"
        );
    }

    #[test]
    fn one_line_keeps_the_first_line_and_names_store_paths() {
        assert_eq!(one_line("linux fails"), "linux fails");
        assert_eq!(one_line("\n  \nfirst line\nsecond line"), "first line");
        assert_eq!(one_line(""), "");
        assert_eq!(
            one_line(
                "its system did not build: builder for '/nix/store/0123456789abcdfghijklmnpqrsvwxyz-aws-lc-5.12.0.drv' failed with exit code 2"
            ),
            "its system did not build: builder for 'aws-lc-5.12.0' failed with exit code 2"
        );
        assert_eq!(
            one_line("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-hello-2.12/bin/hello"),
            "hello-2.12/bin/hello"
        );
        let short = one_line(&"x".repeat(200));
        assert_eq!(short.chars().count(), 80);
        assert!(short.ends_with("..."));
        let short = one_line(&"\u{e6}".repeat(200));
        assert_eq!(short.chars().count(), 80);
        assert!(short.starts_with("\u{e6}\u{e6}"));
    }

    #[test]
    fn a_hold_label_says_why_in_one_line() {
        let mut f = PinsFile::default();
        f.entry_mut(Kind::Nixos, "home").held = Some(Held {
            since: "2026-10-27".into(),
            reason: format!(
                "its system did not build: builder for '/nix/store/0123456789abcdfghijklmnpqrsvwxyz-aws-lc-5.12.0.drv' failed {}",
                "z".repeat(100)
            ),
            lock: json!({}),
        });
        let s = summary(&f, today(), 14, 7);
        let label = s["items"][0]["label"].as_str().unwrap();
        assert!(
            label.starts_with(
                "today, held: its system did not build: builder for 'aws-lc-5.12.0' failed zzz"
            ),
            "{label}"
        );
        assert!(label.ends_with("..."), "{label}");
        assert_eq!(label.chars().count(), "today, held: ".len() + 80);

        f.entry_mut(Kind::Nixos, "home")
            .held
            .as_mut()
            .unwrap()
            .reason = String::new();
        let s = summary(&f, today(), 14, 7);
        assert_eq!(s["items"][0]["label"], "today, held");
    }
}
