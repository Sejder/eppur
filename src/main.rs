mod cmd;
mod config;
mod date;
mod engine;
mod git;
mod graph;
mod guard;
mod inputs;
mod lock;
mod log;
mod logparse;
mod nix;
mod pins;
mod resolve;
mod state;
mod summary;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

const BUILD_EXIT: &str = "Exit status: 0 when everything built or was held, 1 when eppur gave up on this machine's own host, 2 on errors, 3 with --fail-on-hold when something is held, 130 when interrupted.";
const SWITCH_EXIT: &str = "Exit status: 0 when everything built or was held and the switch worked, 1 when eppur gave up on this machine's own host, 2 on errors, 3 with --fail-on-hold when something is held, 4 when the switcher failed (the build and the files are kept), 130 when interrupted.";

/// Update a flake and keep it building: packages that fail are pinned to the
/// nixpkgs they last built with, and unpinned once they build again.
#[derive(Parser)]
#[command(
    version,
    after_help = "Start with 'eppur switch --update'. See what is pinned with 'eppur status', \
                  why with 'eppur why ATTR', and undo a pin with 'eppur clear ATTR'."
)]
struct Cli {
    /// Only report pins, unpins, holds, warnings and errors.
    #[arg(short, long, global = true, help_heading = "Global options")]
    quiet: bool,
    /// Show progress and eppur's reasoning, overriding programs.eppur.quiet.
    #[arg(
        short,
        long,
        global = true,
        conflicts_with = "quiet",
        help_heading = "Global options"
    )]
    verbose: bool,
    #[command(flatten)]
    at: cmd::Where,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pin and unpin until every configured host and home builds.
    #[command(after_help = BUILD_EXIT)]
    Build {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// Build, then switch this machine's own host or home.
    #[command(after_help = SWITCH_EXIT)]
    Switch {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// Build, then make this machine's own host boot into it.
    #[command(after_help = SWITCH_EXIT)]
    Boot {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// Run nix flake update, keeping a pinned flake input where it is until
    /// its upstream has a new revision to try.
    Update,
    /// List the pins and held hosts and homes, with their age.
    Status {
        /// Print a JSON summary for dashboards and scripts instead.
        #[arg(long)]
        json: bool,
    },
    /// Show why a package is pinned: the build that failed, how it leads to
    /// the package, and the end of its build log.
    Why {
        #[command(flatten)]
        pick: cmd::Pick,
        /// A top-level package attribute, e.g. firefox.
        #[arg(value_name = "ATTR")]
        attr: String,
    },
    /// Pin a package by hand at the current lock; it stays until cleared.
    /// Applied by the next build, switch or boot.
    Pin {
        #[command(flatten)]
        pick: cmd::Pick,
        /// A top-level package attribute, e.g. firefox.
        #[arg(value_name = "ATTR")]
        attr: String,
        /// The input to pin it in; defaults to programs.eppur.input.
        #[arg(long, value_name = "INPUT")]
        input: Option<String>,
        /// Pin it at the input's revision in this flake.lock instead of the
        /// current one, e.g. one from git history, for a package that builds
        /// but misbehaves.
        #[arg(long, value_name = "FILE")]
        old_lock: Option<std::path::PathBuf>,
    },
    /// Have the next run retry a pin eppur made, without waiting for its
    /// package set to move: the pin goes if the package builds, and stays at
    /// its revision if it still fails.
    Retry {
        #[command(flatten)]
        pick: cmd::Pick,
        /// The pinned attribute, e.g. firefox.
        #[arg(value_name = "ATTR")]
        attr: String,
        /// Only retry the pin in this input; by default every input pinning ATTR.
        #[arg(long, value_name = "INPUT")]
        input: Option<String>,
    },
    /// Remove a pin, or release a held host or home with --held. A package
    /// that still fails is pinned again by the next run.
    #[command(
        visible_alias = "unpin",
        group = clap::ArgGroup::new("what").required(true).args(["attr", "held", "flake_input"])
    )]
    Clear {
        #[command(flatten)]
        pick: cmd::Pick,
        /// The pinned attribute, e.g. firefox.
        #[arg(value_name = "ATTR")]
        attr: Option<String>,
        /// Only clear the pin in this input; by default every input pinning ATTR.
        #[arg(long, value_name = "INPUT")]
        input: Option<String>,
        /// Release the held host or home instead, so the next run builds it
        /// with the current lock.
        #[arg(long, conflicts_with_all = ["attr", "input"])]
        held: bool,
        /// Remove the pin of this flake input instead. flake.lock stays as it
        /// is until the next update moves the input; if it still fails then,
        /// it is pinned again.
        #[arg(long, value_name = "INPUT", conflicts_with_all = ["attr", "input", "held"])]
        flake_input: Option<String>,
    },
    /// Fold another pins.json into the flake's; per host or home, the entry
    /// for the flake's own lock or else from the newer lock wins, and the
    /// flake input pins changed last are taken whole.
    Merge {
        /// Another pins.json, e.g. one published by a build host.
        #[arg(value_name = "FILE")]
        file: std::path::PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        guard::restore_after_panic();
    }));
    log::set_verbose(cli.verbose);
    log::set_quiet(cli.quiet);
    let at = &cli.at;
    let result = match &cli.command {
        Command::Build { build } => cmd::reconcile(at, build, cmd::Activation::None),
        Command::Switch { build } => cmd::reconcile(at, build, cmd::Activation::Switch),
        Command::Boot { build } => cmd::reconcile(at, build, cmd::Activation::Boot),
        Command::Update => cmd::update(at),
        Command::Merge { file } => cmd::merge(at, file),
        Command::Status { json } => cmd::status(at, *json),
        Command::Why { pick, attr } => cmd::why(at, pick, attr),
        Command::Pin {
            pick,
            attr,
            input,
            old_lock,
        } => cmd::pin(at, pick, attr, input.as_deref(), old_lock.as_deref()),
        Command::Retry { pick, attr, input } => cmd::retry(at, pick, attr, input.as_deref()),
        Command::Clear {
            pick,
            attr,
            input,
            held,
            flake_input,
        } => match flake_input {
            Some(name) => cmd::clear_input(at, name),
            None => cmd::clear(at, pick, attr.as_deref(), input.as_deref(), *held),
        },
    };
    result.unwrap_or_else(|e| {
        log::flush_nix_errors();
        eprintln!("eppur: error: {e:#}");
        if let Err(e) = guard::restore() {
            eprintln!("eppur: error: restoring flake.lock and pins.json failed: {e:#}");
        }
        ExitCode::from(2)
    })
}
