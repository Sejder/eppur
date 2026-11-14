# Changelog

## Unreleased

- Most derivations on current nixpkgs use structured attributes, which `nix
  derivation show` prints outside `env`, and eppur read `pname` and
  `outputHash` only from `env`. On a real system it saw 246 of 8285 downloads
  and 3732 of 5933 packages. A failed download was therefore pinned as a
  broken package instead of being built once more, and the guard against
  pinning a library that many packages depend on, and the check that no
  package depends on one before a donor's copy replaces it, counted too few
  dependents, mostly those in nested sets such as `python3Packages`. The
  name is now read from `env`, then the structured attributes (also the
  `env.__json` of older Nix), and a download is recognised by an output with
  a fixed hash. With the full count, a failure in a package that more than 25
  others depend on now holds the host where it was pinned before.
- `programs.eppur.nixArgs` and `--nix-arg ARG` (repeatable) pass extra
  arguments to the nix commands of a build, switch or boot run. They go
  before nix's subcommand, so they are global settings such as `--max-jobs`,
  `--builders` or `--option NAME VALUE`; one nix refuses there stops the run
  with an error before anything builds. The switcher does not get them.
  `programs.eppur.evalJobs` and `--eval-jobs N` set how many evaluations run
  at once (4, as before).
- After a failed build, eppur dumped the whole derivation graph of every host
  it had built, including the hosts that built, which for a fleet is the
  heaviest call of a round (76 MB of JSON for one host). Only the hosts that
  did not build are dumped now.
- Looking up whether a package is available from a substituter asked each
  substituter one after the other; they are asked at the same time now.
- `build`, `switch` and `boot` take `--json`: one JSON object on stdout with
  what the run pinned, unpinned, held, released and built, the exit code and
  what it means (see the README). The log stays on stderr, and so does the
  output of the switcher and of nix-output-monitor, which used to go to stdout.
- CI runs the end-to-end suite on Nix 2.19, 2.24 and the current release,
  and a NixOS VM test (x86_64-linux) checks that the
  `programs.eppur.autoUpgrade` service builds and activates.

- A package nixpkgs newly refuses as insecure held the whole host, although
  the host already ran that same vulnerable copy: the hold did not remove the
  vulnerability, and it kept every other package old, security fixes included.
  Such a package is now pinned to the revision the host last built with, as
  long as this machine already has that copy in its store. A package the
  configuration only now starts to use still holds the host. The pin is marked
  `insecure` in `pins.json` (an optional field, so format version 2 is
  unchanged), "marked insecure upstream" in `eppur status` and `eppur why`, and
  is warned about on every run. `programs.eppur.pinInsecure = false` keeps the
  old behaviour of holding the host. Unfree and unsupported packages still
  hold the host.
- A package nixpkgs refuses as insecure, with no copy of it in this machine's
  store to keep, held the host even when a donor had a copy nixpkgs does not
  refuse, such as a fixed version that reached `nixos-unstable-small` before
  `nixos-unstable`, or one the configuration only now starts to use. When no
  older copy can be kept, the package is now pinned to a donor whose copy
  evaluates and is already built or in a substituter. Once the system
  evaluates with the pin, and before it is built, the pin is dropped and the
  target held, as before, when the copy still contains the refused package,
  when other packages in the system depend on it, or when the configuration
  overrides it, which would build it from the donor's sources. The pin stays
  marked `insecure` and is warned about on every run, since a donor behind on
  the marking may still ship an older vulnerable version.

## 0.1.9

- The rows of `eppur status --json` left out why a host or home was held and
  which targets a pinned flake input failed on, although pins.json records both,
  so a dashboard showing only `name` and `label` said "today, held" and "...
  failed to build; kept at REV". A hold's label now carries the first line of its
  reason with store paths shortened to their names ("today, held: REASON"), and
  an input pin's label names its targets ("...failed to build; breaks host pi;
  kept at REV", an evaluation error cut to one line). The `eppur status` table
  shows those targets in its TARGET column instead of "flake", and no longer
  breaks on a multi-line evaluation error. An input waiting for a build to try
  another revision said "kept at REV, retrying" in the label, "retrying" in the
  table and "the lock has a newer revision" in `eppur why`, although the lock
  can also hold the revision that failed when the good one is gone upstream; they
  now say "last good REV, flake.lock has another revision to try".
- A run printed "retrying input NAME" for a pinned flake input even when it
  built none of the targets the input had failed on, so `eppur switch --update`
  on one machine announced a retry it could neither test nor clear. The line is
  now only printed by a run that builds one of those targets, and by `eppur update`.
- Every run evaluated each target in full, even when nothing had changed since
  the last one, because `nix eval` does not use the flake evaluation cache.
  Targets are now evaluated with `nix path-info --derivation`, so a run over an
  unchanged, clean flake skips the evaluation (about a minute for a NixOS host).
  On Nix before 2.24, which hides the error of a cached failure, a failed
  evaluation is run again without the cache.
- While upstream kept a pinned flake input at the revision that failed, every
  update showed `nix flake update`'s "Updated input" lines for it, although eppur
  put the input straight back and flake.lock did not change. nix's list is now
  kept back unless the update fails; eppur lists the root inputs that actually
  moved once it has settled the lock ("no input changed" when none did), and the
  line for an input put back names the revision that failed.

## 0.1.8

- A flake input pinned again after a retry kept every target its earlier pin had
  failed on, including targets that had just built the newer revision. Its
  `targets` in `pins.json` (and "failed on" in `why` and `status --json`) now drop
  those, so a revision that fails on one host no longer names others that built it.

## 0.1.7

- An interrupted run whose terminal had been closed, such as `eppur switch` in a
  terminal window closed mid-build, could hang instead of exiting and keep the
  flake's lock, so every later run was refused with "another eppur run is using".
  It now restores `flake.lock` and `pins.json` and exits 130. A run already stuck
  this way has to be killed with `kill -9`.

## 0.1.6

Renamed from nixheal to eppur, after "Eppur si muove" ("and yet it moves"):

- The binary, flake repository (`github:Sejder/eppur`), `programs.eppur` options,
  `/etc/eppur.json`, `~/.config/eppur.json`, `$EPPUR_CONFIG` and the
  `eppur-upgrade` units all change. Setting `programs.nixheal` is an error that
  says so. `pins.json` is unchanged.
- The state directory and the roots in a `--gc-roots` directory are taken over on
  the first run, so last good locks and the root of a held host are kept.
- Switch the machine that runs eppur before it next runs on the renamed flake. The
  old binary cannot evaluate `programs.eppur`, so with `--hold-local` it holds
  every target and exits 0, and a pipeline that publishes after that run publishes
  the holds. The first full run of the new binary releases them, so a pipeline that
  skips a run when nothing changed has to be made to run once.

Less output:

- eppur no longer advertises flags, options and commands while it runs, or
  describes what a later run will do: that is in `--help` and the README. `status`
  and `why` lose their footers of follow-up commands. An error that stops the run
  still says how to get past it.
- `-v` now also shows eppur's reasoning, which is no longer shown by default:
  each target that built, why a package set was not pinned, what a merge took,
  and where roots went. `-q` is unchanged.
- Hold reasons in `pins.json` are shorter, without the advice they ended in.

Faster runs:

- Targets evaluate side by side, up to four at once, so a fleet's rounds take
  less time and more memory at their peak.
- The checks of existing pins before the first round take one evaluation per
  target, instead of one per pin, revision and donor.
- A flake input the failure visibly involves (its source is in the failed build,
  or its path in the evaluation error) is tested first when looking for which
  input to roll back. It still has to prove itself.
- An evaluation failure that is traced through several packages checks them in
  one evaluation per package set, and remembers the answers for the run.
- After a failed download or a killed builder only those are built once more, not
  a real failure next to them, which may take long and would fail again.

Fixes:

- `--timeout` could be overrun by a whole build: when it passed during some of
  eppur's nix commands, nothing was stopped, and the next build ran to its end.
  Every nix command now stops at the deadline, and nothing new starts after it. A
  SIGTERM now stops all of them too.
- A locked input containing `''` or `${` no longer stops eppur from evaluating it.

## 0.1.5

Flake inputs that break a build go back in `flake.lock`:

- A package from a flake input other than a package set could not be pinned, so
  one bad commit there held every host using it, away from all other updates
  too, until it was fixed. nixheal now finds the input by evaluating the failed
  target with each changed input put back, takes it back to the revision it last
  built with for the whole flake, and starts the run again. A failed build counts
  as fixed only if its counterpart builds with the input put back. Evaluation
  failures count too. A rollback that does not help, or breaks a target that
  built, is undone, and the targets are held as before. See
  [Flake input](README.md#concepts).
- New `nixheal update`, for scripts that compare the lock after updating: it
  leaves a pinned input where it is until upstream has a new revision. `--update`
  does the same. Fleet scripts that run `nix flake update` should switch to it, or
  the lock differs on every poll while an input is pinned.
- New `programs.nixheal.inputDeny`: inputs never to roll back; `[ "*" ]` turns
  this off. List any input holding secrets: going back restores secrets rotated
  since. Package sets, donors, inputs others follow and inputs whose source changed
  in `flake.nix` are never rolled back. Only a run that speaks for the whole flake
  rolls back: with `--hold-local`, or a config listing every host and home it has,
  and without `--only`.
- `nixheal merge` prefers an entry made for the flake's current `flake.lock`, since
  rolling back an input can lower the lock's `lastModified`.
- New `nixheal clear --flake-input`. `status`, `status --json` (rows of kind
  `input`, and `input_pins`) and `why` show input pins. `merge` takes the later of
  two files' input pins.
- A check that refuses to move inputs backwards in a published lock should leave
  out inputs under `inputs` in `pins.json` and what they bring along.
- `pins.json` is still version 2. It only gains `inputs` and `inputsChanged` once
  an input is pinned. An older nixheal ignores them, and drops them when it
  rewrites the file.

Status and pins:

- `status --json` says why each package is pinned and what the pin provides:
  `cause`, `failed`, `failed_version`, `version`, `donor`, `rev`, `rev_date`,
  `tried`, `retry_from`, `error` and per-set `sets`, and the row's `label`
  sums it up ("1d, cloudflared-2026.10.3 failed to build; pinned to 2026.10.3 from
  nixpkgs-stable") instead of only the age and kind. The text `status` shows
  the cause and the pinned version, and `why` the version and when an
  unchanged failure is retried.
- Pins record the version they provide (`"version"` in `pins.json`); existing
  pins get it on the next build.
- The closing line of a run counts pins moved to a donor instead of saying
  nothing was pinned or unpinned.

## 0.1.4

Retries that cannot help are skipped, and pins can move forward to a donor:

- A pinned build failure was retried every time its input moved, even when
  the update left the package's derivation exactly as it was, so the same
  failing build (a test hanging for its full timeout, say) ran again on every
  run. Each build pin now records the package's own drv (`"rootDrv"`); when
  its input moves and the package is still that drv, it is not built again
  until it changes or `programs.nixheal.retrySameDerivationAfterDays` (default
  7; `0` restores the old behaviour) have passed since it was last tried
  (`"tried"`). `nixheal retry` still retries it at once. Pins made before this
  are retried once more, as before, and record it then.
- New `programs.nixheal.donors`: flake inputs, such as a stable nixpkgs next
  to an unstable one, that a failed package may be pinned to instead of the
  revision it last built with. A donor is used when it has a newer version of
  the package whose copy is already in the store or a substituter, so the pin
  gains the newer version and builds nothing. Existing pins move to a donor
  once one has such a copy, and back to the pin they replaced if the donor's
  copy fails. Only packages nothing else in the system depends on are taken
  from a donor (`"leaf"` in `pins.json`, recorded from the next failure on);
  `status` and `why` show the donor (`"donor"`).

Pinned packages keep your overlays:

- A pinned package was imported from the old nixpkgs without your overlays,
  so a patch or override of yours silently disappeared while it was pinned.
  It now gets the overlays ordered before nixheal's own (those after it still
  apply on top, once), so the pinned copy is the one your last good system
  ran.
- When a pin is made, nixheal evaluates the package with and without your
  overlays; if they do not evaluate against the old nixpkgs, the pin is made
  without them and a warning says why. Each pin records the choice
  (`"overlays"` in `pins.json`); existing pins keep getting none.
- New `programs.nixheal.pinnedOverlays` (default `true`); with `false`, a
  warning says when leaving your overlays out changes a pinned package.
- `lib.mkOverlay` takes `overlays`, applied to pins that ask for them.


Fixes from a sixth review and a fourth review of the command line:

- Standalone Home Manager no longer fails to evaluate once something is
  pinned: `homeManagerConfiguration` passes its full `nixpkgs.config`, with
  options left at `null`, which an older nixpkgs rejects. Null settings are
  now dropped for every pinned import and extra package set.

- A lock only counts as last good together with the pins it built with, which
  are now kept next to it. A package that fails again is pinned to the
  revision that worked, also after its pin was cleared or a run's uncommitted
  changes were thrown away; before, it could be pinned to a revision where it
  was broken too, and this machine's runs gave up until upstream fixed it.
- New `nixheal retry ATTR` queues a retry of a pin nixheal made (shown as
  "retry queued" in `status`); `nixheal clear ATTR` (and `unpin`) removes a
  pin, as its name says. `clear --drop` is gone.
- An unreadable `pins.json` is an error instead of being removed by the
  restore, and a restore no longer stages the deletion of a tracked
  `pins.json`.
- A switcher that fails is reported, and exits with 4 instead of 1.
- `--timeout` (and `autoUpgrade.timeout`) stops updating and building only;
  activation is never cut short. The timers no longer set `TimeoutStartSec`.
- An interrupt waits for nix to exit before restoring, and a panic restores
  too.
- Pinned nixpkgs are imported without the user's own overlays, so
  `~/.config/nixpkgs/overlays` cannot reach them under `--impure`.
- `merge` keeps the local entry on a tie, so a local `clear` or `retry` is not
  undone; `write_atomic` writes through a symlinked `pins.json`; a pin kept on
  retry records the current failure.
- Messages: hold reasons say why there is no older revision to pin to, a pin
  in several sets shows each set's revision when they differ, dry runs no
  longer print created, added and restored lines, `pin` defaults to the
  target's own input, `merge` of a `flake.lock` says so, the exit codes in
  `build --help` and `switch --help` differ, the no-config error says that
  hosts or homes must be set, `status` shows each pin's revision and lists
  the next commands on separate lines.

## 0.1.2

Fixes from four more independent reviews of 0.1.1 and three reviews of its
command line.

Breaking: nixheal no longer commits by default. `build`, `switch` and `boot`
leave `flake.lock` and `pins.json` changed in the checkout unless given
`--commit` or `programs.nixheal.commit = true`; `--no-commit` still turns it
off. `autoUpgrade.commit` defaults to `null`, which follows
`programs.nixheal.commit`, so a timer that should keep committing needs one of
the two set.

Failures:

- A configuration that no longer evaluates, such as one using a removed NixOS
  option, holds that host instead of stopping the run.
- A package nixpkgs refuses to evaluate as insecure, unfree or unsupported
  holds the host instead of being pinned around; a package marked broken is
  still pinned.
- A build killed by a signal, or a failed download that is not a hash
  mismatch, is retried once, and a signal counts as infrastructure rather than
  a broken package.
- Derivations are matched to versioned attributes such as `nodejs_22`.
- Derivation paths are recognised in any store directory.

Setups:

- A flake in a subdirectory of its git repository works; `pinsFile` is the
  `pins.json` next to `flake.lock`, and the run lock lives in the git directory.
- Home Manager inside NixOS takes its entry name from the host's
  `programs.nixheal.name` and its input from the host's
  `programs.nixheal.input`.
- Home Manager with `nixpkgs.config = null`, and NixOS with `nixpkgs.pkgs`
  set, no longer break the overlay.
- Extra package sets and pinned packages are imported for the right platforms
  when cross-compiling.
- The timers set `NIXHEAL_CONFIG`, so they read the settings of the module
  that defined them. The Home Manager timer has git and ssh on its PATH and
  requires `flake`.
- New `autoUpgrade.onFailure`, units to start when a run fails.
- nh is installed when it is the switcher and `programs.nh` is off.
- `nixheal pin` pins in `programs.nixheal.input` unless `--input` says
  otherwise.

Robustness:

- `pin`, `clear` and `merge` take the run lock and check the flake path like
  the build commands.
- A signal can no longer land between writing `pins.json` and restoring it, or
  between the commit and the end of guarding, and an error after the build
  (GC roots, the commit) restores both files.
- Commits run with `core.hooksPath=/dev/null`, so no hook can run.
- A flake path containing `#` or `?` is refused.
- `--gc-roots` removes the roots of targets no longer configured.
- `--quiet` keeps nix's own error messages.
- Journal output is detected by the inode in `JOURNAL_STREAM`, so a run piped
  from a service still prints plainly.

Pins, holds and roots:

- `--gc-roots` only removes roots it made: they are named
  `nixheal-host-NAME` and `nixheal-home-NAME`, so a host and a home can no
  longer share one, and other links in the directory are left alone. Roots
  under the old names are not removed; delete them once by hand.
- The Home Manager timer is kept running through the switch it starts
  (`X-SwitchMethod = keep-old`), and the NixOS one is not stopped when an
  activation drops it (`X-StopOnRemoval = false`).
- The cap on dependents counts every package above the one to pin, not only
  top-level attributes, so a library many `python3Packages` use is not pinned.
- After a failed build, a system counts as built only if it is in the store,
  and older Nix's "N dependencies of derivation ... failed to build", timeout
  and signal messages are understood.
- A host or home that is not in the flake is an error before anything builds,
  instead of being held.
- A build that fails without naming a derivation stops the run instead of
  holding every other host.
- A system that built in an early round is evaluated again with the final
  `pins.json`, and built again if that changed it.
- The evaluation trace is read in full when a failure inside a nested package
  needs the package that uses it.
- `merge` refuses a missing file and one that is not a pins file; every
  command that writes refuses a directory without `flake.lock`.

Command line:

- Every command that writes says which flake and config it acts on. An
  explicit `--config` or `$NIXHEAL_CONFIG` that cannot be read is an error.
- `--config`, `--flake` and `-q` work before or after the command, under their
  own help heading; every flag and argument has help text, and help wraps.
- `--update` with an input the flake does not have is an error.
- `pin`, `clear` and `clear --held` say what happens next and how to undo
  them; `pin` says when it replaces an existing pin, and `clear --held` no
  longer takes an attribute.
- Holds and give-ups say what to do; a host held before is "still" held.
- Dry runs mark their changes `(dry run)` instead of reporting them twice.
- A run ends with one line of what it built, pinned, unpinned and held, and
  what it committed. Short rounds and fresh pins are reported more quietly.
- `--quiet` holds back nix's error messages unless nixheal could not handle
  them, and without it handled errors are followed by a line saying so.
- `status` prints an aligned table with a header and a summary, names the
  flake, and says when there is no `pins.json`. `status --json` rows carry
  `target`, `target_kind`, `inputs`, `attr`, `since` and `held_reason`.
- `why` says where else a package is pinned and how to clear it.
- Error messages name where a path came from, cut nix's errors to the line
  that says what failed, and avoid store paths where a name will do.
- `--max-rounds 0` is refused.

Package sets:

- A package several package sets provide as the same derivation is one
  failure: it is pinned in every set that can take the pin (a set that did
  not move, or is new in the lock, is skipped instead of holding the host),
  counts once against `--max-new-pins`, is retried and released as a group,
  and is never reported "builds again" while a sibling pin still carries it.
  `status`, `why`, `status --json` and the logs show it once, with its sets.
- An evaluation failure is pinned in every set that lost the attribute, not
  only the first.
- Pinned packages and extra package sets are imported for the right build
  and host platforms, so `pkgsStatic` and cross builds keep them.

Runs:

- A build that cannot run here (another platform, no builder, out of memory)
  holds only the hosts that need it; this machine's own is given up on.
- A held entry records the lock hash of the lock it is held on.
- An interrupt stops the running nix too, and no write lands after the
  restore.
- The Home Manager `autoUpgrade` with no `homes` is an error, as on NixOS.
- New `autoUpgrade.timeout` (12h) stops a stuck run.
- `--gc-roots` lists the roots each flake made, and refuses a second flake
  rooting a target of the same name in one directory.
- `merge` keeps pins made by hand here that the incoming entry lacks.
- A read-only nixpkgs (`readOnlyPkgs`) gets a warning instead of an error.
- New `--fail-on-hold`, exiting with 3 when something is held.
- Pins of evaluation failures keep nix's error, and `why` shows it.

More command line:

- `pin`, `why` and `clear` refuse a `--host` or `--home` that is not
  configured.
- The closing line says when nothing changed instead of claiming uncommitted
  files, and a commit's title follows whether `flake.lock` changed.
- Holds say how to release them; `status` ends with what to run next.
- Pin lines name the package, its sets, the revision's date and what failed.
- Per-pin age lines are gone below `warnAfterDays`.
- `build --help` groups fleet and tuning options; `-u` for `--update`,
  `unpin` for `clear`, `-v`/`--verbose` to override `quiet`, `--only a,b`.
- `clear` with no argument says `<ATTR|--held>`.
- Errors about a config file say where its path came from, and the config's
  JSON format is documented.
- `status --json` names targets `host NAME` and `home NAME`, `pins` counts
  packages, and rows carry `inputs`.

Fifth review:

- `nixheal clear` on a pin nixheal made no longer throws the old revision
  away: the next run retries the package, dropping the pin if it builds and
  keeping it at its revision if it still fails. `--drop` removes it outright,
  and says what that means. `nixheal pin --old-lock FILE` pins at the input's
  revision in another `flake.lock`.
- The read-only nixpkgs check works (`readOnlyPkgs` declares
  `nixpkgs.overlays` itself), with a flake check.
- Pinned packages keep musl, `gcc.arch`, LLVM and other plain platform
  settings: a platform that is not the default for its system is passed as
  its triple plus the settings that differ, never as an elaborated platform,
  which an older nixpkgs cannot take. Checks cover glibc, musl, static, LLVM,
  a tuned `gcc.arch` and a cross build.
- `--gc-roots` keeps a held host's root on its list, so it is pruned once the
  host is no longer configured.
- Every error after a run starts restores `flake.lock` and `pins.json`
  (including a missing `--old-lock` on a fresh checkout), the error comes
  first and the restore is reported only when something changed, and a dry
  run or failed run takes a `pins.json` it added out of the git index.
- `nix flake update` is stopped with nixheal, and its output is held back
  under `--quiet` unless it fails.
- A hold that `merge` sees is ranked by the lock it was tried against
  (`checkedLastModified`), so a newer hold is not lost.
- A system that embeds `pins.json` by itself is built again with the final
  file too.
- Once this machine's own host fails, the run stops instead of building the
  others for nothing.
- `autoUpgrade` without `enable` is an error; `--dry-run` honours
  `--fail-on-hold`; `--only host:NAME` and `home:NAME` tell a host and a home
  apart.
- Messages: the hint for a package pinned on this machine's own entry, the
  "not pinning" reasons, a closing line that says "nothing pinned or
  unpinned" and "uncommitted changes" (shown under `--quiet` too), `why` on a
  held host, `clear --input` with the wrong set, pins dropped when their host
  is held, "held" for homes, hints that do not repeat the flag you passed,
  one `attr (sets)` form, a warning when `switch` activates nothing, exit
  codes in `--help`, real plurals, and "today".
- The README says that `pins.json` is code to review and only merge from a
  trusted build host.

## 0.1.1

Fixes from an independent review of 0.1.0.

Setups where pins silently never applied are now caught before anything
builds, with an error saying what to change:

- `programs.nixheal.name` must be the configuration's name: a host whose name
  differs (for example `networking.hostName = "alice-laptop"` on
  `nixosConfigurations.laptop`, or `homeConfigurations."alice@laptop"`) wrote
  its pins under one name and read them under another.
- `pinsFile` must be the `pins.json` at the flake root, where nixheal writes
  it, and a path rather than a string.
- `programs.nixheal.enable` must be on in every target.
- New `programs.nixheal.input`, for a main nixpkgs input not called `nixpkgs`.

Bugs:

- Fixed-output hash mismatches are recognised as build failures, so a broken download
  is pinned instead of failing every host in the round. Only hosts whose own
  build failed are affected by an unrecognised failure.
- Builds that cannot run here (another platform, no builder, a timeout, out of
  memory) stop the run without pinning or holding anything.
- `switch` and `boot` with `--hold-local` no longer activate a held local host.
- Each target uses its own package sets, read from its configuration, instead
  of the running machine's.
- The last good lock is kept per target, and saved only after the commit.
- A run writes the entries it built, instead of keeping a stale entry when the
  lock moved backwards.
- A run takes a lock on the flake, and Ctrl-C or SIGTERM restores `flake.lock`
  and `pins.json`.
- `--dry-run` leaves `--built-link` and a fresh checkout alone.
- `boot` on a Home Manager configuration is refused before building.
- `--only` with an unknown name is an error.
- `nixheal pin` checks that the package exists.
- Host names are quoted in attribute paths.
- Packages more than 25 others depend on are no longer pinned, since that
  rebuilt all of them from source; the host is held instead.

Safety:

- `autoUpgrade.user` is required, and nixheal refuses to run as root in a
  checkout another user owns, which left root-owned files there. Files are
  written through exclusively created temporary files that keep the original
  owner and mode, so a planted symlink is never followed.
- Commits skip git hooks and signing, so a hook or a GPG prompt cannot stall
  the timer. When git refuses a repository, nixheal says so.
- The root activation step only activates a NixOS system in the store, and
  `--built-link` is a GC root.
- The NixOS timer has ssh available for `git+ssh` inputs; the Home Manager
  timer drops a dependency the user manager cannot satisfy.
- `argName` cannot shadow a module argument such as `pkgs`.
- Home Manager inside NixOS warns when `hosts`, `homes` or `autoUpgrade` are
  set there, since they only apply standalone.
- A flake path inside the Nix store is refused.

Smaller changes:

- `homeModules.default` is exported next to `homeManagerModules.default`.
- `status` marks old pins and holds `STALE`, as the JSON does, and manual pins
  never go stale. Homes are named `home NAME` in the summary. Dates are local.
- The README documents every flag, the exit codes, the config lookup order, the
  three requirements above and the overlay's precedence.
- Tests cover the Home Manager module, the activation step, the new log
  formats, the probes, the dry run, holds without activation and interruption.
  CI lints test code too.

## 0.1.0

First public release.

- `nixheal build`, `switch` and `boot`, each with `--update [INPUT...]`:
  update the flake, build every configured host and home, pin packages that
  fail to the nixpkgs they last built with, unpin them once they build again,
  hold hosts pinning cannot fix, commit `flake.lock` and `pins.json`, and
  activate with nh, nixos-rebuild or home-manager.
- `nixheal status [--json]`, `why`, `pin`, `clear` and `merge`.
- NixOS and Home Manager modules (`programs.nixheal`) that apply the pins to
  `pkgs` and to further nixpkgs imports, write the settings to
  `/etc/nixheal.json` or `~/.config/nixheal.json`, and provide
  `autoUpgrade`, a timer in place of `system.autoUpgrade`.
- Fleet support: `--hold-local`, `--gc-roots` and `--built-link`, per-host
  entries in `pins.json` (format version 2) with the lock each was computed
  for, and `merge` with newer-lock-wins.
- `pins.json` format version 2 is stable from this release on.
