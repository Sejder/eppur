# eppur

Keep a NixOS flake updating when single packages break.

*Eppur si muove*: "and yet it moves". See [the name](#the-name).

When one package in a `nix flake update` fails to build, the whole system
update fails, and every other security and bug fix waits with it until you
notice, disable the package or write an overlay. eppur does that for you,
and undoes it by itself:

- it updates your flake and builds your hosts;
- a package that fails is **pinned** to the nixpkgs it last built with, and
  everything else moves to the new lock;
- on every later update the pin is retried, and it is **removed** as soon as
  the package builds again;
- a host that pinning cannot fix (a kernel, glibc, a NixOS module error) is
  **held** on its last good lock while the other hosts move on.

```
$ eppur switch --update
eppur: flake /home/alice/nixos (config /etc/eppur.json)
eppur: round 1: evaluating and building host laptop
eppur: pinned host laptop: sbctl (nixpkgs) at 718f670e8453 from 2026-10-20: sbctl-0.18 failed to build
eppur: round 2: evaluating and building host laptop with 1 pinned package
eppur: done: 1 built, 1 pinned; flake.lock and pins.json uncommitted
eppur: switching host laptop with nh
...
$ eppur switch --update          # a few days later, sbctl is fixed upstream
eppur: retrying host laptop: sbctl (nixpkgs)
eppur: unpinned host laptop: sbctl (nixpkgs): builds again
```

## The name

"Eppur si muove" is what Galileo is said to have muttered after he was forced
to recant that the Earth moves. It fits a system update: a package fails to
build, and yet your system moves. Say it "EP-poor".

eppur was called nixheal until 0.1.5. To move over, rename the flake input,
`programs.nixheal` to `programs.eppur`, and any use of `NIXHEAL_CONFIG` to
`EPPUR_CONFIG`. Its state directory and `--gc-roots` roots are taken over on
the first run, and `pins.json` is unchanged.

Switch the machine that runs it to the new name by hand first (with `nh` or
`nixos-rebuild`): the old binary cannot evaluate a flake that only has
`programs.eppur`.

## Quick start

eppur is a flake. Add it, import the module on your host and point it at the
`pins.json` it will maintain next to `flake.lock`:

```nix
# flake.nix
inputs.eppur = {
  url = "github:Sejder/eppur";
  inputs.nixpkgs.follows = "nixpkgs";
};

# your host's configuration (with inputs passed in specialArgs)
{ inputs, ... }:
{
  imports = [ inputs.eppur.nixosModules.default ];

  programs.eppur = {
    enable = true;
    pinsFile = ../../pins.json;      # the pins.json at the root of your flake; eppur creates it
    flake = "/home/alice/nixos";     # the checkout eppur updates
    hosts = [ "laptop" ];            # nixosConfigurations eppur builds when it runs here
  };
}
```

Three things have to line up, and eppur checks them before it builds
anything:

- **`pinsFile` is the `pins.json` next to `flake.lock`**, written as a path
  (`./pins.json`, or `../../pins.json` from a module in `hosts/laptop/`). The
  flake may live in a subdirectory of its git repository.
- **`programs.eppur.name` is the configuration's name**:
  `nixosConfigurations.laptop` needs `name = "laptop"`. It defaults to
  `networking.hostName` (Home Manager: `home.username`), so set it when those
  differ.
- **`programs.eppur.input` is the flake input `pkgs` comes from**, as named in
  `flake.lock`. The default is `nixpkgs`; set it if yours is called `nixos`.

Switch once so the module is active, then update through eppur instead of
`nh os switch --update` or `nixos-rebuild switch --upgrade`:

```
eppur switch --update
```

That updates the flake inputs, builds `nixosConfigurations.laptop` (pinning and
unpinning as needed) and switches, leaving `flake.lock` and `pins.json` changed
in the checkout for you to commit. Pass `--commit`, or set
`programs.eppur.commit = true`, to have eppur commit exactly those two
files after each run. Use `eppur boot --update` to activate on the next
boot, or `eppur build --update` to only build.

To have it run on its own, replace `system.autoUpgrade` with:

```nix
programs.eppur.autoUpgrade = {
  enable = true;
  user = "alice";        # the owner of the checkout; activation runs as root
  operation = "switch";  # or "boot"
};
```

The timer builds the checkout as it is, uncommitted changes included, so keep
work in progress on a branch or out of the tree when it runs. `user` runs the
update and the build, and root then activates what it built, so whoever can
write to the checkout or act as `user` can decide what root activates: treat
that account as root. `autoUpgrade.onFailure` names units to start when a run
fails, for a notification.

### Home Manager, standalone

```nix
# homeConfigurations.alice, with the Home Manager module imported
programs.eppur = {
  enable = true;
  pinsFile = ./pins.json;
  flake = "/home/alice/nixos";
  homes = [ "alice" ];
};
```

`name` defaults to `home.username` and must be the `homeConfigurations` key, so
set it for keys such as `"alice@laptop"`. `eppur switch --update` then runs
`nh home switch`.

## Requirements

- Nix 2.19 or newer with flakes; developed against Nix 2.34. Lix is untested.
- nh 4.x when `switcher = "nh"` (tested with 4.4.2). The module installs nh
  when `programs.nh` does not.
- git, for committing; without it eppur still runs and says it cannot
  commit.

## Concepts

**Pin.** A top-level package (an attribute of `pkgs`, or of another package set
you register) taken from an older nixpkgs: the one in the last lock that
built. eppur walks up the derivation graph from a failed build to the
packages your configuration uses directly and pins those, not the library
underneath. A derivation is matched to an attribute by its `pname`, its name
without the version, and versioned names such as `nodejs_22`, each confirmed
against the derivation path. It never pins a package that more than 25 other
packages in the system depend on (nested ones such as `python3Packages.*`
included): the pinned copy would replace it for all of them, and every one
would rebuild from source. Such a failure holds the host instead.

A package that several of your package sets provide as the same derivation
(say `cloudflared` from `nixpkgs` and `nixpkgs-unstable` at the same revision)
is one failure: it is pinned in every set that can take the pin, skipping a set
that did not move or is new in the lock, and reported, counted, retried and
released as one package. `status` shows the sets next to it.

**Hold.** When a failure is not inside a pinnable package (configuration glue,
a configuration that no longer evaluates, such as a removed NixOS option, a
denylisted core package such as the kernel, glibc, systemd, nix, stdenv or
gcc, a package too much depends on, a package that fails at the old revision
too, or a package nixpkgs refuses to evaluate because it became insecure,
unfree or unsupported), the host is held: eppur records the lock it last
built with in its entry and does not build it on the new one. The shared
`flake.lock` still moves for the other hosts, so a held machine only stays on
its old system if whatever updates it checks `held` (see [A fleet](#a-fleet));
a run on the machine itself never activates a held host. A held host is
retried on every run and released once it builds; `eppur clear --held`
releases it by hand. Packages marked broken are pinned like failed builds. The
machine
eppur runs on is never held unless you pass `--hold-local`: eppur gives up
on it instead, for `build`, `switch`, `boot` and the timer alike. It restores
`flake.lock` and `pins.json`, commits nothing and activates nothing.

**Infrastructure.** A build that cannot run on this machine at all (another
platform, no builder, a timeout, out of memory, a build killed by a signal) is
not a broken package: nothing is pinned for it, and the hosts that need it are
held (this machine's own is given up on, as above) while the others move on. A
build killed by a signal, or a failed download that is not a hash mismatch, is
retried once first. A build that fails without nix naming a derivation (the
daemon went away, the disk is full) says nothing about any host, so the run
stops and restores the files.

**Last good lock.** The lock a target last built with, kept per flake and
target under `$XDG_STATE_HOME/eppur` together with the pins it built with:
a lock is only good with those pins. A package that was pinned then and fails
again is pinned to the same revision, even if its pin was cleared or the
run's changes were thrown away since. For a target that has not built yet,
the last lock any target of the flake built with, then the current
`flake.lock`.

**Retry.** Each pin records the input revision it was last tried at. When that
input moves, the pin (with the package's pins in other sets from the same
failure) is dropped for the next build; if the package builds, the pin is
gone, otherwise it comes back with its original date. `eppur retry` asks
for the same without waiting for the input to move. Manual pins
(`eppur pin`) are never retried.

A build failure is only retried when there is something new to try: the pin
records the package's own derivation, and when its input moves but the
package evaluates to that same derivation (the update changed other packages
only), it is not built again. It is retried once the derivation changes, or
once `retrySameDerivationAfterDays` (7) have passed since it was last tried,
for a failure that may have been a flaky test; `0` retries on every move.

**Donor.** An input listed in `programs.eppur.donors`, such as a stable
nixpkgs next to an unstable one, that a failed package may be taken from
instead of the revision it last built with. When a package fails and a donor
has a newer version of it whose copy is already in the store or one of the
substituters, the pin is made to the donor: it builds nothing and is not a
step back. An existing pin moves to a donor once one has such a copy, and goes
back to the pin it replaced if the donor's copy fails. Only a package nothing
else in the system depends on is taken from a donor, since its copy brings the
donor's libraries along. The input must be in `flake.lock`; it does not have
to be used by your configuration:

```nix
inputs.nixpkgs-stable.url = "https://channels.nixos.org/nixos-26.05/nixexprs.tar.zst";

programs.eppur.donors = [ "nixpkgs-stable" ];
```

**Flake input.** A package from a flake input other than a package set never
passes through the overlay, so it cannot be pinned. When a target fails in a way
no pin helps with, eppur looks at the inputs that changed since the target last
built, first checking that the failure happens again, and tries each one put
back: a configuration that no longer evaluates has to evaluate, and what the input
changes under the failed build's name has to build (more than ten of those and
the input is not tested). The first input that does this goes back to
that revision in `flake.lock`, and the whole run starts again on that lock, which
also finds any further failure. It is tested rather than guessed from names,
since two inputs can provide a package of the same name. `flake.lock` has one revision per input, so
this holds for every target, not one: targets that do not use the input are not
affected, and everything else in the lock still updates. It is recorded under
`inputs` in `pins.json`.

While upstream stays at the revision that failed, an update puts the input back,
leaving the lock unchanged. A new revision is tried by the next build: the pin
goes once every target it failed on builds, and otherwise the input goes back
again, keeping the pin's date. If a target still fails after the rollback, a
target that built before it now fails, or targets need revisions that exclude
each other, rolling back cannot settle it: the lock and pins go back to how they
were and the targets are held, all of them, even one a rollback had fixed.

Never rolled back: package sets and donors, inputs that other inputs follow,
inputs whose branch or URL changed in `flake.nix`, and inputs listed in
`programs.eppur.inputDeny`. **List any input holding secrets there**: going back
to an older revision restores secrets rotated since, and a host held for a long
time takes the input back as far as the lock it is held on.

`flake.lock` is the whole flake's, so only a run that speaks for the whole flake
rolls back: one with `--hold-local`, or whose config lists every host and home the
flake has, and without `--only`. Any other run holds or gives up as it would have.

## Commands

The verbs follow `nh` and `nixos-rebuild`:

| Command | |
|---|---|
| `eppur build [--update [INPUT...]]` | pin and unpin until every configured host builds; commit with `--commit` |
| `eppur switch [--update [INPUT...]]` | the same, then switch this machine's host or home |
| `eppur boot [--update [INPUT...]]` | the same, then make this machine boot into it |
| `eppur status [--json]` | pinned packages and held hosts with their age |
| `eppur why <attr>` | the failed derivation (or the evaluation error), the path up to the pinned package, the end of the build log |
| `eppur update` | `nix flake update`, keeping a pinned flake input where it is until upstream has a new revision |
| `eppur pin <attr> [--input I] [--old-lock FILE]` | pin by hand (in `programs.eppur.input` by default), at the current lock or at the input's revision in `FILE`, e.g. a `flake.lock` from git history for a package that builds but misbehaves; kept until cleared |
| `eppur retry <attr> [--input I]` | have the next run retry a pin eppur made: the pin goes if the package builds, and stays at its revision if it still fails; `status` shows it as "retry queued" |
| `eppur clear <attr> [--input I]` (or `unpin`) | remove a pin; if the package still fails, the next run pins it again at the revision it last built with |
| `eppur clear --flake-input I` | remove a flake input's pin; `flake.lock` stays until the next update moves it, and it is pinned again if it still fails then |
| `eppur clear --held` | release a held host or home: drop its held lock, so machines that skip held entries update again, and the next run builds it with the current lock |
| `eppur merge <file>` | fold another `pins.json` in; per host or home, the entry from the newer lock wins, keeping pins made by hand here |

`why`, `pin` and `clear` act on this machine's entry, or on `--host NAME` /
`--home NAME`, which must be configured (or, for `why` and `clear`, have an
entry). `pin`, `clear` and `merge` only change `pins.json`; the next `build`,
`switch` or `boot` applies it, and commits it with `--commit` or
`programs.eppur.commit`.

`--config FILE`, `--flake DIR`, `-q` and `-v` work before or after the
command. `-q` (`--quiet`) prints only pins, unpins, holds, warnings, errors and
files left uncommitted; nix's own messages are shown only when eppur could
not handle them. `-v` (`--verbose`) overrides `programs.eppur.quiet`, and
adds eppur's reasoning: each target that built, why a package set was not
pinned, what a merge took. Up to four evaluations run at once.
Every command but `status` and `why` says first which flake and config it
acts on, since by default both come from `/etc/eppur.json` or
`~/.config/eppur.json`, wherever you run it. A `--config` (or
`$EPPUR_CONFIG`) that cannot be read is an error; a directory without a
`flake.lock` is refused by every command that writes.

Options of `build`, `switch` and `boot`:

| Option | |
|---|---|
| `-u`, `--update [INPUT...]` | run `nix flake update` first, for all inputs or the named ones (an unknown name is an error) |
| `--dry-run` | evaluate and build as usual and show what would be pinned and unpinned (marked `(dry run)`), then leave every file as it was |
| `--commit` | commit `flake.lock` and `pins.json` after the run (the default with `programs.eppur.commit = true`) |
| `--no-commit` | leave them uncommitted, whatever `programs.eppur.commit` says (the default otherwise) |
| `--only NAME` | build only some of the configured hosts and homes; repeatable or comma-separated, and `host:NAME` or `home:NAME` where a host and a home share a name |
| `--hold-local` | hold this machine's own host like any other instead of giving up; for a machine that builds a fleet |
| `--gc-roots DIR` | keep every system that built alive under `DIR/eppur-host-<host>` and `DIR/eppur-home-<name>`, e.g. for a binary cache; without `--only`, the roots this flake's runs made for targets no longer configured are removed (they are listed in a hidden file there), and nothing else in `DIR` is touched. Give each flake its own `DIR`: a second flake rooting a target of the same name there is refused |
| `--fail-on-hold` | exit with 3 when a host or home is held |
| `--timeout DURATION` | stop updating, evaluating and building after this long (`12h`, `45min`, `90s`); the files are restored and activation is never cut short |
| `--built-link PATH` | a GC root pointing at this machine's built system, removed when it did not build, so a separate step can activate it |
| `--old-lock FILE` | the lock to pin back to, instead of the last good one |
| `--max-rounds N` | give up after N build rounds (8) |
| `--max-new-pins N` | hold a host, or give up on this machine's own, rather than pin more than N new packages for it in one run (10) |
| `--system SYSTEM` | the system for eval checks when a target's own cannot be evaluated |

eppur works in a dirty tree. When it commits, it commits only `flake.lock`
and `pins.json`, without running git hooks or signing. A run takes a lock on the
flake (also taken by `update`, `pin`, `clear` and `merge`), so a timer run and a manual
run cannot overlap, and Ctrl-C or SIGTERM puts both files back. A flake path
containing `#` or `?` is refused, since nix would read those as part of a
flake reference. On a terminal, builds are shown through
nix-output-monitor.

Every configured host and home must exist in the flake; a name that does not
is an error before anything builds.

Exit codes: 0 when everything built or was held, 1 when eppur gave up on this
machine's own host, 2 on errors, 3 with `--fail-on-hold` when something is
held, 4 when `switch` or `boot` built but the switcher failed (the build and
the files are kept), 130 when interrupted. A run stopped by `--timeout` is an error (2).

The settings come from `$EPPUR_CONFIG`, then `~/.config/eppur.json`
(Home Manager), then `/etc/eppur.json` (NixOS), whichever exists first. The
modules write them; by hand, the file looks like this (`kind` and `self` are
required, the rest default as the module options do):

```json
{ "kind": "nixos", "self": "laptop", "flake": "/home/alice/nixos",
  "hosts": ["laptop"], "homes": [], "input": "nixpkgs", "warnAfterDays": 14,
  "deny": [], "quiet": false, "commit": false, "switcher": "nh",
  "donors": [], "retrySameDerivationAfterDays": 7 }
```

## Options

`nixosModules.default` and `homeManagerModules.default` (also exported as
`homeModules.default`) share these options under `programs.eppur`:

| Option | Default | |
|---|---|---|
| `enable` | `false` | apply this machine's pins; install eppur when `hosts` or `homes` is set |
| `pinsFile` | `null` | the `pins.json` next to `flake.lock`, as a path (it may not exist yet) |
| `name` | host name / user name | this machine's entry in `pins.json`; must be its configuration's name |
| `input` | `"nixpkgs"` | the flake input `pkgs` comes from |
| `flake` | `programs.nh.flake` | the checkout eppur updates (not a store path) |
| `hosts`, `homes` | `[ ]` | `nixosConfigurations` / `homeConfigurations` eppur builds when it runs here |
| `switcher` | `"nh"` | `"nh"` (4.x, given the built system's store path), or `"native"`: `nixos-rebuild` (with `--sudo`, i.e. nixos-rebuild-ng) for hosts, `home-manager` for homes |
| `warnAfterDays` | `14` | report a pin or hold as stale, and log it as a warning, after this many days |
| `deny` | `[ ]` | attributes never to pin, on top of the built-in list; `cuda*` matches a prefix |
| `quiet` | `false` | always run as `--quiet` |
| `commit` | `false` | commit `flake.lock` and `pins.json` after each run, as with `--commit` |
| `pinnedOverlays` | `true` | apply your own nixpkgs overlays to pinned packages (see below) |
| `donors` | `[ ]` | flake inputs a failed package may be pinned to when one has a newer, already built copy of it (see [Donor](#concepts)) |
| `inputDeny` | `[ ]` | flake inputs never to roll back in `flake.lock` when they break a build; a trailing `*` matches a prefix, so `[ "*" ]` turns rolling back off (see [Flake input](#concepts)) |
| `retrySameDerivationAfterDays` | `7` | retry a build failure whose derivation an update did not change only after this many days; `0` on every move of its input |
| `package` | this flake's | the eppur package |
| `nixpkgs.<input>` | `{ }` | another nixpkgs to import with pins applied: `source`, `config` (default: this host's `nixpkgs.config`), `argName` |
| `packageSets` | read-only | every package set eppur pins on this machine, keyed by input |
| `autoUpgrade.enable` | `false` | a timer that runs eppur and activates the result |
| `autoUpgrade.dates` | `"04:40"` | systemd calendar expression |
| `autoUpgrade.randomizedDelaySec` | `"45min"` | |
| `autoUpgrade.inputs` | `[ ]` | inputs to update; all when empty |
| `autoUpgrade.commit` | `null` | commit after each timer run; `null` follows `commit` |
| `autoUpgrade.operation` | `"switch"` | NixOS only: `"switch"` or `"boot"` |
| `autoUpgrade.user` | (required) | NixOS only: the owner of the checkout, who updates, builds and commits |
| `autoUpgrade.onFailure` | `[ ]` | units to start when a run fails |
| `autoUpgrade.timeout` | `"12h"` | stop updating and building after this long (`--timeout`, e.g. `12h`, `45min`); activation is never cut short |

The settings are written to `/etc/eppur.json` (NixOS) or
`~/.config/eppur.json` (Home Manager), so eppur keeps working when the
updated flake no longer evaluates.

The pin overlay is added with `mkAfter` and replaces each pinned attribute with
the old nixpkgs' copy. That copy is imported with your own overlays, the ones
ordered before eppur's (those after it apply on top as usual), so a pinned
package keeps your patches and is the copy your last good system ran. When a
new pin is made, eppur evaluates the package both ways: if your overlays do
not evaluate against the old nixpkgs (they were written for the new one), the
pin is made without them and a warning names the error. Each pin records its
choice (`"overlays": true` in `pins.json`; pins without the field, from before
this, get none). With `pinnedOverlays = false`, pins never get your overlays,
and a warning says when that changes a pinned package. Extra package sets
(`programs.eppur.nixpkgs`) take no overlays of yours, so neither do their
pins.

`lib.mkOverlay { pins, kind ? "hosts", name, input, nixpkgsConfig ? null, overlays ? [ ] }`
is the overlay itself, for package sets the modules do not import; `overlays`
are applied to the pins marked `"overlays": true`. Sets made
that way are not in `packageSets`, so eppur does not match failures to them;
use `programs.eppur.nixpkgs` for that.

### More package sets

Packages from a second nixpkgs, say `nixpkgs-unstable` on a stable system, are
pinned too if eppur imports that nixpkgs for you:

```nix
programs.eppur.nixpkgs.nixpkgs-unstable = {
  source = inputs.nixpkgs-unstable;
  config.allowUnfree = true;
  argName = "unstable-pkgs";   # modules can then take `unstable-pkgs` as an argument
};
```

`argName` cannot be a name modules already get, such as `pkgs` or `lib`.

### Home Manager

Inside NixOS, the Home Manager module follows the host's settings and applies
its pins; `hosts`, `homes` and `autoUpgrade` set there are ignored with a
warning, so set them in the NixOS configuration. Standalone, it uses its own
`homes` entry, and `eppur switch` runs `nh home switch` (or
`home-manager switch` with the native switcher). `autoUpgrade` there is a user
timer that runs `eppur switch --update`; it runs while you are logged in, or
always with `loginctl enable-linger`.

## A fleet

For several machines, let one of them build all of them and publish the result,
the way a binary cache host would:

1. On the build host, list every machine in `hosts` and run
   `eppur build --update --hold-local --no-commit --gc-roots DIR` on a timer.
   A host that cannot be fixed is held; the others still get the new lock. To
   skip the build when nothing changed, run `eppur update` first and compare
   `flake.lock` with what you published; a plain `nix flake update` moves a pinned
   flake input, so that comparison would never match while one is pinned.
2. Publish `flake.lock` and `pins.json` together (`pins.json` first). Each host
   entry carries the `lockSha256` it was computed for, so a consumer can refuse
   a mismatched pair. A held entry keeps the `lockSha256` of the lock it is held
   on, so check `held` before comparing.
3. On each machine, install both into its checkout before rebuilding. A machine
   whose entry is `held` should skip the update and stay on what it runs. A lock
   may be older than the last one for an input listed under `inputs` in `pins.json`,
   and for what that input brings along: a check that refuses to move an input
   backwards should leave those out, or it waits until upstream is fixed.
4. Fold the published `pins.json` back into your repository with
   `eppur merge`, so manual runs and the fleet do not overwrite each other.

`eppur status --json` on the build host feeds a dashboard (see below).

## Dashboards

`eppur status --json` prints one object for a dashboard, e.g. a
homepage-dashboard `customapi` widget:

```json
{"status":"PINNED","pins":1,"pin_entries":2,"held":0,"stale":0,
 "oldest":"host laptop: open-webui, 3d","oldest_days":3,"list":"host laptop: open-webui (3d)",
 "items":[{"name":"host laptop: open-webui","label":"3d, evaluation failed; pinned to 0.6.30 at 718f670e8453","inputs":["nixpkgs","nixpkgs-unstable"],...}],
 "warn_after_days":14,"generated":"2026-10-30"}
```

`status` is the most urgent of `STALE` (a pin or hold older than
`warnAfterDays`), `HELD`, `PINNED` and `OK`. Manual pins never go stale. `items`
has one row per pinned package or held host for a `dynamic-list` widget, and a
single `OK` row when nothing is pinned; targets are named `host NAME` and
`home NAME`, as everywhere else. `pins` counts packages and flake inputs,
`input_pins` the flake inputs, `pin_entries` the pins
across package sets. For scripts, each row also has `target`, `target_kind`
(`host` or `home`) and `since`, plus `inputs` and `attr` for a pin or
`held_reason` for a hold. A pin's row says why and what it provides: `cause`
(e.g. "cloudflared-2026.10.3 failed to build"), `failed` and `failed_version`
(the derivation that failed), `version` (what the pin provides), `donor`,
`rev` and `rev_date` (the revision it is pinned to), `tried`, `retry_from` (the
day an unchanged failure is retried anyway), `error` for an evaluation failure,
and `sets` with the same per package set; its `label` sums it up, e.g.
"1d, cloudflared-2026.10.3 failed to build; pinned to 2026.10.3 from nixpkgs-stable".
Ages are in local days
and computed when it runs, so regenerate it regularly. The text `status` marks
the same pins and holds `STALE`.

The text `eppur status`:

```
flake /home/alice/nixos
TARGET       PACKAGE (SETS)                          REASON           SINCE
host laptop  open-webui (nixpkgs, nixpkgs-unstable)  evaluation failed  2026-10-20 (8d)  0.6.30 at 718f670e8453
host server  -                                       whole host held  2026-10-01 (27d) STALE
    nixpkgs.linux fails and is on the never-pin list
1 pinned package, 1 held, 1 stale (older than 14 days).
```

## Limitations

- **A system that embeds `pins.json` as text** (for example
  `environment.etc."pins".text = builtins.readFile ./pins.json`) and finished
  in an early round is not built again for the final file; the next run builds
  it. Systems that take the flake's source or `pins.json` as a path are.

- **A pin holds back fixes for that package**, security fixes included. That is
  the trade: everything else updates. `warnAfterDays` and `status` make old
  pins visible; check `eppur why` and fix or `eppur clear` them.
- **A rolled back flake input holds back its fixes** for every target that uses
  it, security fixes included, and a run that rolls one back evaluates everything
  again. `warnAfterDays` and `status` make it visible.
- **Other update tools move a pinned flake input.** After a plain
  `nix flake update`, the next eppur run puts it back if upstream has not moved.
  A script that compares the lock after updating, to skip a build, should run
  `eppur update` instead.
- **Only top-level attributes are pinned.** A broken package inside a nested set
  (`python3Packages.foo`) is handled by pinning the top-level package that uses
  it; eppur finds that from the build graph or from the evaluation trace.
- **Activation is not checked.** eppur knows what builds, not what starts:
  a data migration a new release needs (say PostgreSQL collations after a
  glibc update) still fails at switch time.
- **Configurations are expected at `nixosConfigurations.<host>` and
  `homeConfigurations.<name>`**, with eppur's module imported in each.
- **Home Manager inside NixOS with `home-manager.useGlobalPkgs = false`** has
  its own `pkgs`, but its failures are matched against the host's package
  sets; a package that differs between the two may hold the host instead of
  being pinned. Use `useGlobalPkgs = true`, or a standalone Home Manager.
- **Linux only.** The package is built for x86_64-linux and aarch64-linux.
- **A read-only nixpkgs** (`nixosModules.readOnlyPkgs`) cannot take the
  module's overlay; the module warns, and you add `lib.mkOverlay` to the `pkgs`
  you pass in yourself.
- **Nix's log format.** eppur reads the messages of current Nix (the
  "Cannot build ... Reason:" format) and the older "builder for ... failed"
  one; after a failed build it also checks that each system it counts as
  built really is in the store.
- **Evaluation dominates the run time.** Each round evaluates every host once,
  one after another, and a dirty git tree gets no evaluation cache. With nh, the
  switch reuses the system eppur built instead of evaluating again.

## Compared with

- **`system.autoUpgrade`** updates and switches, but one failing package stops
  the whole update until someone fixes it. `programs.eppur.autoUpgrade` is
  the same timer with pinning in between.
- **Pinning by hand in an overlay** works, but you have to notice the failure,
  find the right revision, and remember to remove the pin later. eppur does
  all three and records why in `pins.json`.

## pins.json

```json
{ "version": 2,
  "hosts": {
    "laptop": { "lockSha256": "...", "lockLastModified": 1793083946,
                "pins": { "nixpkgs": { "sbctl": { "locked": { ... }, "since": "2026-10-27", "reason": "build",
                                                  "failedDrv": "...", "trail": [ ... ], "checkedNarHash": "..." } } } },
    "server": { "held": { "since": "2026-10-27", "reason": "...", "lock": { ... } } } },
  "homes": { },
  "inputs": { "atlas": { "locked": { ... }, "failedNarHash": "...", "since": "2026-11-02", "reason": "build",
                         "failedDrv": "...", "targets": [ "host server" ] } },
  "inputsChanged": 1793500000 }
```

`locked` is the flake.lock node of the nixpkgs the package is taken from, so
the overlay fetches it purely. A run replaces the entries of the targets it
built and leaves the others alone. `eppur merge` combines two files: per
target, an entry made for the flake's current `flake.lock` wins over one that was
not, then the entry from the newer lock (a held entry counts the lock it was
tried against, `checkedLastModified`), and pins made by hand here are kept.
`inputs` is one decision about one lock, so the map with the later
`inputsChanged` is taken whole. `inputs` and `inputsChanged` are absent until an
input is first pinned.

`pins.json` is code, like `flake.lock`: every `locked` node in it is fetched
and imported when your systems evaluate. Only merge files from a build host
you trust, and review changes to it like any other change to the flake.

## Development

```
cargo test             # unit tests
nix flake check        # package, overlay, module and activation checks
tests/e2e.sh           # a toy nixpkgs that breaks and heals, end to end
```

`tests/e2e.sh` needs Nix with flakes and uses `target/debug/eppur` (or
`$EPPUR`). nh, nixos-rebuild and home-manager are replaced by stubs.

## License

MIT, see [LICENSE](LICENSE).
