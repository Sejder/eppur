#!/usr/bin/env bash
# End-to-end run of eppur against a toy nixpkgs whose history breaks and then
# fixes packages, on two toy hosts: a (the machine eppur runs on) and b.
# Needs a working nix with flakes; run from anywhere:
#
#   cargo build && tests/e2e.sh
#
# EPPUR overrides the binary under test. nh, nixos-rebuild and home-manager
# are replaced by stubs that record their arguments.

set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
eppur="${EPPUR:-$repo/target/debug/eppur}"
nix() { command nix --extra-experimental-features "nix-command flakes" "$@"; }
system="$(nix eval --raw --impure --expr builtins.currentSystem)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
toy="$work/toypkgs"
donor="$work/donorpkgs"
sys="$work/system"
log="$work/log"
calls="$work/nh.calls"
config="$work/eppur.json"
salt="$(date +%s%N)"

export XDG_STATE_HOME="$work/state"
export GIT_AUTHOR_NAME=e2e GIT_AUTHOR_EMAIL=e2e@localhost
export GIT_COMMITTER_NAME=e2e GIT_COMMITTER_EMAIL=e2e@localhost
mkdir -p "$work/bin"
for tool in nh nixos-rebuild home-manager; do
  printf '#!/bin/sh\necho "%s $*" >> %s\n[ -z "$STUB_FAIL" ] || exit 1\n' "$tool" "$calls" >"$work/bin/$tool"
  chmod +x "$work/bin/$tool"
done
export PATH="$work/bin:$PATH"
touch "$calls"

step() { printf '\n=== %s\n' "$*"; }
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  [ -f "$log" ] && sed 's/^/  | /' "$log" >&2
  exit 1
}

jq_pins() {
  nix eval --raw --impure --expr "
    let f = builtins.fromJSON (builtins.readFile $sys/pins.json); in $1"
}

pins_of() {
  jq_pins "builtins.concatStringsSep \" \" (builtins.concatLists (builtins.attrValues
    (builtins.mapAttrs (i: s: map (a: i + \".\" + a) (builtins.attrNames s)) ((f.hosts.$1 or { }).pins or { }))))"
}

field() { jq_pins "toString (f.hosts.$1.pins.$2.$3.$4 or \"\")"; }
held_reason() { jq_pins "(f.hosts.$1.held or { }).reason or \"\""; }

expect_pins() {
  local got
  got="$(pins_of "$1")"
  [ "$got" = "$2" ] || fail "expected pins on $1 [$2], got [$got]"
  echo "pins on $1: [$got]"
}

expect_log() { grep -qF -- "$1" "$log" || fail "log lacks: $1"; }
reject_log() { if grep -qF -- "$1" "$log"; then fail "log unexpectedly has: $1"; fi; }
commits() { git -C "$sys" rev-list --count HEAD; }

# nh gets the system eppur built, so it does not evaluate the flake again.
expect_switched_to_built() {
  local want
  want="$(nix eval --raw "$sys#nixosConfigurations.$1.config.system.build.toplevel.outPath")"
  [ "$(tail -n1 "$calls")" = "nh os switch $want" ] || fail "nh was not given the built $1 ($want): $(tail -n1 "$calls")"
}
nh_calls() { wc -l <"$calls" | tr -d ' '; }

expect_built() {
  local out
  out="$(nix build --no-link --print-out-paths "$sys#nixosConfigurations.$1.config.system.build.toplevel")"
  grep -qF -- "$2" "$out" || fail "$1 does not contain $2: $(head -c 300 "$out")"
}

write_config() {
  local hosts='"a", "b"'
  [ -n "${3:-}" ] && hosts="$3"
  cat >"$config" <<EOF
{ "kind": "nixos", "self": "a", "flake": "$sys", "hosts": [$hosts], "homes": [],
  "warnAfterDays": 14, "commit": ${COMMIT:-true}, "deny": [$1]${2:+, $2} }
EOF
}

write_home_config() {
  cat >"$config" <<EOF
{ "kind": "home", "self": "alice", "flake": "$sys", "hosts": [], "homes": ["alice"],
  "warnAfterDays": 14, "commit": true, "deny": [] }
EOF
}

eppur_() {
  set +e
  "$eppur" "$@" --config "$config" >"$work/stdout" 2>"$log"
  local rc=$?
  set -e
  cat "$work/stdout" >>"$log"
  grep -h 'eppur:' "$log" | sed 's/^/  | /' || true
  return $rc
}

revision() {
  cat >"$toy/state.nix" <<EOF
{ libfoo = "$1"; goneRemoved = $2; hello = "$3"; linux = "$4"; moduleError = ${5:-false}; extra = "${6:-ok}"; salt = "$salt"; }
EOF
  git -C "$toy" add -A
  git -C "$toy" commit -qm "libfoo=$1 gone-removed=$2 hello=$3 linux=$4 extra=${6:-ok}"
}

[ -x "$eppur" ] || fail "$eppur is not built; run cargo build first"

mkdir -p "$toy" "$sys"
git -C "$toy" init -q -b main
cat >"$toy/default.nix" <<'EOF'
{ localSystem ? { inherit system; }, system ? localSystem.system, crossSystem ? null, config ? { }, overlays ? [ ] }:
assert builtins.all (n: config.${n} != null) (builtins.attrNames config)
  || throw "A definition for option `${builtins.head (builtins.filter (n: config.${n} == null) (builtins.attrNames config))}' is not of the right type: null";
let
  state = import ./state.nix;
  mk = name: pname: deps: script: derivation {
    inherit name pname system deps;
    inherit (state) salt;
    builder = "/bin/sh";
    args = [ "-c" script ];
  };
  broken = what: "echo ${what} does not compile >&2; exit 1";
  base = self: {
    inherit config overlays;
    helloTag = if state.extra == "tagarg" then ({ flavor }: "-${flavor}") else ({ }: "-plain");
    inherit (state) moduleError;
    stdenv.hostPlatform.system = system;
    stdenv.buildPlatform.system = system;
    libfoo = mk "libfoo-1.0" "libfoo" [ ] (
      if state.libfoo == "broken" then broken "libfoo" else "echo libfoo-${state.libfoo} > $out"
    );
    app = mk "app-2.0" "app" [ self.libfoo ] "echo app $deps > $out";
    hello =
      if state.hello == "insecure" then
        throw "Refusing to evaluate package 'hello-9' in /nix/store/x-source/pkgs/by-name/he/hello/package.nix:1 because it is marked as insecure"
      else if state.hello == "foreign" then
        derivation { name = "hello-foreign"; system = "aarch64-darwin"; builder = "/bin/sh"; args = [ "-c" "echo > $out" ]; inherit (state) salt; }
      else mk "hello-${state.hello}${self.helloSuffix or ""}" "hello" [ ] (
      if state.hello == "broken" then broken "hello"
      else if state.hello == "slow" then "i=0; while [ $i -lt 30000000 ]; do i=$((i + 1)); done; echo hello > $out"
      else "echo hello > $out"
    );
    linux = mk "linux-6.0" "linux" [ ] (
      if state.linux == "broken" then broken "linux" else "echo linux-${state.linux} > $out"
    );
    gone = if state.goneRemoved then throw "'gone' has been removed" else mk "gone-1.0" "gone" [ ] "echo gone > $out";
    corelib = mk "corelib-1.0" "corelib" [ ] (
      if state.extra == "corebroken" then broken "corelib" else "echo corelib > $out"
    );
    tool = mk "tool-1.0" "tool" (builtins.genList (
      i: mk "pydep-${toString i}-1.0" "pydep-${toString i}" [ self.corelib ] "echo pydep $deps > $out"
    ) 30) "echo tool > $out";
    webui = mk "webui-1.0" "webui" [
      (if state.extra == "innergone" then throw "'innerlib' has been removed" else self.libfoo)
    ] "echo webui $deps > $out";
    flaky = derivation {
      name = "flaky-${state.extra}-${state.salt}";
      pname = "flaky";
      inherit system;
      builder = "/bin/sh";
      args = [ "-c" (if state.extra == "ok" then "printf x > $out" else "exit 1") ];
      outputHashMode = "flat";
      outputHashAlgo = "sha256";
      outputHash = "sha256-LXEWQrcmsEQBYnyp+6wy9chTD7GQPMTbAiWHF5IaSIE=";
    };
    deadlink = derivation {
      name = "deadlink-${state.extra}-${state.salt}";
      pname = "deadlink";
      inherit system;
      builder = "/bin/sh";
      args = [ "-c" (if state.extra == "deadlink" then "exit 1" else "printf x > $out") ];
      outputHashMode = "flat";
      outputHashAlgo = "sha256";
      outputHash = "sha256-LXEWQrcmsEQBYnyp+6wy9chTD7GQPMTbAiWHF5IaSIE=";
    };
    usesflaky = mk "usesflaky-1.0" "usesflaky" [ self.flaky ] "echo usesflaky $deps > $out";
    realfail = mk "realfail-1.0" "realfail" [ ] (
      if state.extra == "realfail" then broken "realfail" else "echo realfail > $out"
    );
    fetched = derivation {
      name = if state.extra == "fodfail" then "fetched-1.1" else "fetched-1.0";
      pname = "fetched";
      inherit system;
      builder = "/bin/sh";
      args = [ "-c" (if state.extra == "fodfail" then "exit 1" else "printf x > $out") ];
      outputHashMode = "flat";
      outputHashAlgo = "sha256";
      outputHash = "sha256-LXEWQrcmsEQBYnyp+6wy9chTD7GQPMTbAiWHF5IaSIE=";
    };
  };
  extend = f: o: self: let prev = f self; in prev // o self prev;
  fix = f: let x = f x; in x;
in
fix (builtins.foldl' extend base overlays)
EOF

mkdir -p "$donor"
git -C "$donor" init -q -b main
cp "$toy/default.nix" "$donor/"
cat >"$donor/state.nix" <<EOF
{ libfoo = "ok"; goneRemoved = false; hello = "90"; linux = "ok"; moduleError = false; extra = "ok"; salt = "$salt"; }
EOF
git -C "$donor" add -A
git -C "$donor" commit -qm "a donor with hello 90"

cp "$repo/nix/overlay.nix" "$repo/nix/platform.nix" "$repo/nix/import-pinned.nix" "$repo/nix/marked.nix" "$sys/"
cat >"$sys/flake.nix" <<EOF
{
  inputs.toypkgs = { url = "git+file://$toy"; flake = false; };
  inputs.toypkgs2 = { url = "git+file://$toy"; flake = false; };
  inputs.toydonor = { url = "git+file://$donor"; flake = false; };
  outputs = { self, toypkgs, toypkgs2, ... }:
    let
      system = "$system";
      mkHost = name: paths: mkHostOf "hosts" name paths { };
      ov = name: input: import ./overlay.nix { pins = ./pins.json; kind = "hosts"; inherit name input; };
      toplevel = name: extra: paths: derivation ({
        name = "toplevel-\${name}";
        inherit system paths;
        builder = "/bin/sh";
        args = [ "-c" "echo \$paths > \$out" ];
      } // extra);
      mkTwo = name: paths:
        let
          pkgs = import toypkgs { inherit system; overlays = [ (ov name "toypkgs") ]; };
          pkgs2 = import toypkgs2 { inherit system; overlays = [ (ov name "toypkgs2") ]; };
        in {
          config.programs.eppur = {
            enable = true;
            inherit name;
            pinsFile = ./pins.json;
            packageSets = { toypkgs = pkgs; toypkgs2 = pkgs2; };
          };
          config.system.build.toplevel = toplevel name { } (paths pkgs pkgs2);
        };
      mkOverlaid = name: user: paths:
        let
          marked = import ./marked.nix;
          eppur = marked.wrap (import ./overlay.nix) true { pins = ./pins.json; kind = "hosts"; inherit name; input = "toypkgs"; };
          pkgs = import toypkgs { inherit system; overlays = [ user eppur ]; };
        in {
          config.programs.eppur = {
            enable = true;
            inherit name;
            pinsFile = ./pins.json;
            packageSets.toypkgs = pkgs;
            pinnedOverlays = true;
            importPinned = { locked, overlays }: import ./import-pinned.nix {
              config = { };
              buildPlatform = { inherit system; };
              hostPlatform = { inherit system; };
            } { inherit locked; overlays = if overlays then marked.before pkgs.overlays else [ ]; };
          };
          config.system.build.toplevel = toplevel name { } (paths pkgs);
        };
      mkEmbedding = name: paths:
        let pkgs = import toypkgs { inherit system; overlays = [ (ov name "toypkgs") ]; };
        in {
          config.programs.eppur = {
            enable = true;
            inherit name;
            pinsFile = ./pins.json;
            packageSets.toypkgs = pkgs;
          };
          config.system.build.toplevel = toplevel name { flakeSource = self; } (paths pkgs);
        };
      mkHostOf = kind: name: paths: settings:
        let
          pkgs = import toypkgs {
            inherit system;
            overlays = [ (import ./overlay.nix {
              pins = ./pins.json; inherit kind name; input = "toypkgs";
              nixpkgsConfig = if name == "elaborated" then { rewriteURL = null; allowUnfree = false; } else null;
            }) ];
          };
          sp = derivation {
            name = "system-path-\${name}";
            inherit system;
            builder = "/bin/sh";
            args = [ "-c" "echo \$paths > \$out" ];
            paths = paths pkgs;
          };
        in rec {
          config.programs.eppur =
            if pkgs.moduleError && name == "b" then
              throw "The option \`services.gone' does not exist. Definition values:"
            else {
              enable = true;
              inherit name;
              pinsFile = ./pins.json;
              packageSets.toypkgs = pkgs;
            } // settings;
          activationPackage = config.system.build.toplevel;
          config.system.build.toplevel = derivation {
            name = "toplevel-\${name}";
            inherit system;
            builder = "/bin/sh";
            args = [ "-c" "read -r line < \$sp; echo \"\$line\" > \$out" ];
            inherit sp;
          };
        };
    in {
      nixosConfigurations.a = mkHost "a" (p: [ p.hello p.gone ]);
      nixosConfigurations.b = mkHost "b" (p: [ p.app p.hello p.linux ]);
      nixosConfigurations.c = mkHost "c" (p: [ p.tool p.webui p.fetched ]);
      nixosConfigurations.e = mkTwo "e" (p: p2: [ p2.app ]);
      nixosConfigurations.p = mkHost "p" (p: [ p.usesflaky ]);
      nixosConfigurations.q = mkHost "q" (p: [ p.realfail ]);
      nixosConfigurations.r = mkHost "r" (p: [ p.deadlink ]);
      nixosConfigurations.d = mkEmbedding "d" (p: [ p.hello ]);
      nixosConfigurations.o = mkOverlaid "o" (final: prev: { helloSuffix = "-patched"; }) (p: [ p.hello ]);
      nixosConfigurations.o2 = mkOverlaid "o2" (final: prev: { helloSuffix = prev.helloTag { flavor = "patched"; }; }) (p: [ p.hello ]);
      homeConfigurations.alice = mkHostOf "homes" "alice" (p: [ p.hello ]) { };
      homeConfigurations.elaborated = mkHostOf "homes" "elaborated" (p: [ p.hello ]) { };
      nixosConfigurations.misnamed = mkHostOf "hosts" "misnamed" (p: [ p.hello ]) { name = "other"; };
      nixosConfigurations.elsewhere = mkHostOf "hosts" "elsewhere" (p: [ p.hello ]) { pinsFile = ./sub/pins.json; };
    };
}
EOF
write_config ""

git -C "$sys" init -q -b main
git -C "$sys" add flake.nix overlay.nix platform.nix import-pinned.nix marked.nix
revision ok false 1 ok
nix flake lock "$sys"
git -C "$sys" add flake.lock
git -C "$sys" commit -qm system

step "r1: everything builds; update creates pins.json, commits it and switches a"
eppur_ switch --update || fail "eppur update gave up on r1"
expect_log "created $sys/pins.json"
git -C "$sys" ls-files --error-unmatch pins.json >/dev/null || fail "pins.json is not tracked"
git -C "$sys" diff --quiet HEAD -- pins.json || fail "pins.json is not committed"
expect_switched_to_built a
expect_pins a ""
expect_pins b ""

step "r2: libfoo breaks (only b uses app) and gone is removed (only a uses it)"
revision broken true 2 ok
before_calls="$(nh_calls)"
eppur_ switch --update || fail "eppur update gave up on r2"
expect_pins a "toypkgs.gone"
expect_pins b "toypkgs.app"
[ "$(field a toypkgs gone reason)" = eval ] || fail "gone on a should be an eval pin"
[ "$(field b toypkgs app reason)" = build ] || fail "app on b should be a build pin"
git -C "$sys" log -1 --format=%B | grep -qF "pinned host b: app (toypkgs)" || fail "commit message lacks the b pin"
git -C "$sys" log -1 --format=%B | grep -qF "pinned host a: gone (toypkgs)" || fail "commit message lacks the a pin"
[ "$(nh_calls)" -gt "$before_calls" ] || fail "nh was not called on r2"
expect_built a hello-2
expect_built b hello-2
since_gone="$(field a toypkgs gone since)"

step "why: explain the app pin on b"
eppur_ why app --host b || fail "eppur why failed"
expect_log "-libfoo-1.0.drv"
expect_log "trail:     libfoo-1.0 -> app-2.0"
expect_log "libfoo does not compile"

step "r2 switch: no update, so nothing is retried and nothing is committed"
before_commits="$(commits)"
eppur_ switch || fail "eppur switch gave up on r2"
reject_log "retrying"
[ "$(commits)" = "$before_commits" ] || fail "switch committed without changes"
expect_switched_to_built a

step "r3: libfoo is fixed, gone is still removed"
revision fixed true 3 ok
eppur_ switch --update || fail "eppur update gave up on r3"
expect_log "unpinned host b: app (toypkgs): builds again"
expect_log "host a: gone still fails; keeping its pin in toypkgs"
expect_pins a "toypkgs.gone"
expect_pins b ""
[ "$(field a toypkgs gone since)" = "$since_gone" ] || fail "gone lost its original date"
r3_lock="$work/r3.lock"
cp "$sys/flake.lock" "$r3_lock"

step "r4: linux breaks on b (denylisted): b is held on the r3 lock, a moves on"
revision fixed true 4 broken
eppur_ switch --update || fail "eppur update gave up on r4"
expect_log "held host b on its last good lock"
case "$(held_reason b)" in *"linux fails and is on the never-pin list"*) ;; *) fail "b should be held for linux: $(held_reason b)" ;; esac
held_rev="$(jq_pins 'f.hosts.b.held.lock.nodes.toypkgs.locked.rev')"
r3_rev="$(nix eval --raw --impure --expr "(builtins.fromJSON (builtins.readFile $r3_lock)).nodes.toypkgs.locked.rev")"
[ "$held_rev" = "$r3_rev" ] || fail "b is held on $held_rev, expected the r3 lock $r3_rev"
expect_built a hello-4
eppur_ status || fail "eppur status failed"
expect_log "whole host held"

step "r5: linux is fixed and gone is back: b is released, a unpins"
revision fixed false 5 ok
eppur_ switch --update || fail "eppur update gave up on r5"
expect_log "released host b"
expect_log "unpinned host a: gone (toypkgs): builds again"
expect_pins a ""
expect_pins b ""
[ -z "$(held_reason b)" ] || fail "b is still held"

step "r5 quiet switch: nothing changes, so nothing is printed"
eppur_ switch --quiet || fail "the quiet switch gave up"
reject_log "round 1:"
reject_log "builds"
[ -z "$(grep 'eppur:' "$log" || true)" ] || fail "the quiet switch printed: $(cat "$log")"

step "r6: hello breaks on a, the local host, and is denylisted: nothing changes"
write_config '"hello"'
revision fixed false broken ok
cp "$sys/flake.lock" "$work/before.lock"
cp "$sys/pins.json" "$work/before.json"
before_commits="$(commits)"
before_calls="$(nh_calls)"
if eppur_ switch --update; then fail "eppur update should have given up on r6"; fi
expect_log "giving up on host a"
expect_log "restored flake.lock and pins.json"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored"
cmp -s "$sys/pins.json" "$work/before.json" || fail "pins.json was not restored"
[ "$(commits)" = "$before_commits" ] || fail "a failed update committed"
[ "$(nh_calls)" = "$before_calls" ] || fail "a failed update switched"
write_config ""

step "r6 dry run, quiet: shows the hello pins and nothing else, writes nothing"
eppur_ -q switch --update --dry-run || fail "the dry run gave up"
reject_log "round 1:"
expect_log "(dry run) pinned host a: hello (toypkgs)"
expect_log "(dry run) pinned host b: hello (toypkgs)"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "the dry run changed flake.lock"
cmp -s "$sys/pins.json" "$work/before.json" || fail "the dry run changed pins.json"
[ "$(commits)" = "$before_commits" ] || fail "the dry run committed"
[ "$(nh_calls)" = "$before_calls" ] || fail "the dry run switched"

step "manual pin: kept by switch, removed by clear"
eppur_ pin hello --input toypkgs || fail "eppur pin failed"
expect_pins a "toypkgs.hello"
[ "$(field a toypkgs hello reason)" = manual ] || fail "hello should be a manual pin"
eppur_ switch || fail "eppur switch gave up with a manual pin"
reject_log "retrying"
expect_pins a "toypkgs.hello"
git -C "$sys" log -1 --format=%s | grep -qF "eppur: update pins" || fail "switch did not commit the manual pin"
eppur_ clear hello || fail "eppur clear failed"
expect_log "unpinned host a: hello (toypkgs)"
expect_pins a ""

step "fleet: gone removed and denylisted; with --hold-local a, the local host, is held and b moves on"
write_config '"gone"'
revision fixed true 7 ok
before_calls="$(nh_calls)"
before_commits="$(commits)"
mkdir -p "$work/roots"
ln -sfn /nix/store/not-eppurs "$work/roots/result"
ln -sfn /nix/store/an-old-host "$work/roots/eppur-host-old"
eppur_ build --update --hold-local --no-commit --gc-roots "$work/roots" || fail "the fleet run gave up"
[ -L "$work/roots/result" ] || fail "--gc-roots removed a root eppur did not make"
[ -L "$work/roots/eppur-host-old" ] || fail "--gc-roots removed a root this flake did not make"
expect_log "held host a on its last good lock"
reject_log "giving up on host a"
case "$(held_reason a)" in *"gone fails and is on the never-pin list"*) ;; *) fail "a should be held for gone: $(held_reason a)" ;; esac
[ -z "$(held_reason b)" ] || fail "b should not be held"
[ "$(nh_calls)" = "$before_calls" ] || fail "a fleet run switched"
[ "$(commits)" = "$before_commits" ] || fail "a --no-commit run committed"
git -C "$sys" diff --quiet -- flake.lock && fail "the fleet run restored flake.lock"
[ -L "$work/roots/eppur-host-b" ] || fail "no GC root for b"
[ -e "$work/roots/eppur-host-a" ] && fail "a GC root for the held host a"
reject_log "rooted 1 system in"
cp "$sys/flake.lock" "$work/fleet.lock"
cp "$sys/pins.json" "$work/fleet.json"
write_config "" "" '"a"'
eppur_ build --hold-local --no-commit --gc-roots "$work/roots" || fail "the fleet run for a alone gave up"
[ -L "$work/roots/eppur-host-a" ] || fail "no GC root for a"
[ -L "$work/roots/eppur-host-b" ] && fail "--gc-roots kept this flake's root of b, which is no longer configured"
expect_log "removed the root eppur-host-b"
[ -L "$work/roots/eppur-host-old" ] || fail "--gc-roots removed a root this flake did not make"
cp "$work/fleet.lock" "$sys/flake.lock"
cp "$work/fleet.json" "$sys/pins.json"
write_config ""

step "merge: an entry from a newer lock replaces b, one from an older lock does not"
mkpins() {
  nix eval --impure --json --expr "
    let f = builtins.fromJSON (builtins.readFile $sys/pins.json); b = f.hosts.b; in
    f // { hosts = f.hosts // { b = b // { lockLastModified = b.lockLastModified + ($1); checkedLastModified = (b.checkedLastModified or b.lockLastModified) + ($1); pins.toypkgs.$2 = {
      locked = { type = \"path\"; path = \"/x\"; narHash = \"sha256-x\"; }; since = \"2026-01-01\"; reason = \"manual\"; checkedNarHash = \"\"; }; }; }; }" >"$3"
}
mkpins -100 older "$work/older.json"
mkpins 100 newer "$work/newer.json"
eppur_ merge "$work/older.json" || fail "merge failed"
expect_log "kept host b: the entry here comes from a newer lock"
expect_pins b ""
eppur_ -v merge "$work/newer.json" || fail "merge failed"
expect_log "took host b from"
expect_pins b "toypkgs.newer"
case "$(held_reason a)" in *"gone"*) ;; *) fail "merge lost a's held entry" ;; esac

step "build: a builds, --built-link points at it, nothing is activated"
printf '{"version":2}\n' >"$sys/pins.json"
write_config ""
revision fixed false 8 ok
before_calls="$(nh_calls)"
eppur_ build --update --built-link "$work/built" || fail "eppur build gave up"
want_a="$(nix eval --raw "$sys#nixosConfigurations.a.config.system.build.toplevel.outPath")"
[ "$(readlink "$work/built")" = "$want_a" ] || fail "--built-link does not point at a's system: $(readlink "$work/built")"
[ "$(nh_calls)" = "$before_calls" ] || fail "eppur build activated something"
expect_built a hello-8

step "build: without --hold-local a failing local host gives up and removes --built-link"
write_config '"hello"'
revision fixed false broken ok
cp "$sys/flake.lock" "$work/before.lock"
if eppur_ build --update --built-link "$work/built"; then fail "eppur build should have given up"; fi
expect_log "giving up on host a"
[ -e "$work/built" ] && fail "--built-link survived a failed build"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored"
write_config ""
revision fixed false 9 ok
eppur_ build --update || fail "eppur build gave up on hello-9"

step "boot: nh os boot gets the built system"
eppur_ boot || fail "eppur boot failed"
want_a="$(nix eval --raw "$sys#nixosConfigurations.a.config.system.build.toplevel.outPath")"
[ "$(tail -n1 "$calls")" = "nh os boot $want_a" ] || fail "boot did not call nh os boot: $(tail -n1 "$calls")"

step "native switcher: nixos-rebuild switches the flake"
write_config "" '"switcher": "native"'
eppur_ switch || fail "eppur switch with the native switcher failed"
[ "$(tail -n1 "$calls")" = "nixos-rebuild switch --flake $sys#a --sudo" ] || fail "nixos-rebuild was not called: $(tail -n1 "$calls")"

step "home: a standalone Home Manager configuration switches through nh home"
write_home_config
eppur_ switch || fail "eppur switch for the home failed"
want_home="$(nix eval --raw "$sys#homeConfigurations.alice.activationPackage.outPath")"
[ "$(tail -n1 "$calls")" = "nh home switch $want_home" ] || fail "nh home switch was not given the home: $(tail -n1 "$calls")"
if eppur_ boot; then fail "boot should refuse a home"; fi
expect_log "Home Manager has no boot activation"
reject_log "round 1"
write_config ""

step "probe: hosts whose pins could never apply are refused before anything builds"
write_config "" "" '"misnamed"'
if eppur_ build; then fail "a misnamed host should be refused"; fi
expect_log 'has programs.eppur.name = "other"; it must be "misnamed"'
reject_log "round 1"
write_config "" "" '"elsewhere"'
if eppur_ build; then fail "a pinsFile outside the flake root should be refused"; fi
expect_log "pinsFile = sub/pins.json; it must be pins.json, the pins.json next to flake.lock"
reject_log "round 1"
write_config ""

step "--only: an unknown name is an error, not a silent no-op"
if eppur_ build --only nope; then fail "--only nope should fail"; fi
expect_log "--only nope is not among the configured hosts and homes (a, b)"

step "dry run on a fresh checkout: pins.json is created for the build and removed again, --built-link is left alone"
cp "$sys/pins.json" "$work/pins.keep"
git -C "$sys" rm -q pins.json
git -C "$sys" commit -qm "no pins.json"
ln -sfn /nonexistent "$work/keep"
eppur_ build --dry-run --built-link "$work/keep" || fail "the dry run on a fresh checkout failed"
[ -e "$sys/pins.json" ] && fail "the dry run left pins.json behind"
git -C "$sys" ls-files --error-unmatch pins.json >/dev/null 2>&1 && fail "the dry run left pins.json in the index"
[ "$(readlink "$work/keep")" = /nonexistent ] || fail "the dry run touched --built-link"
cp "$work/pins.keep" "$sys/pins.json"
git -C "$sys" add pins.json
git -C "$sys" commit -qm "pins.json again"

step "switch --hold-local: a held local host is not activated"
write_config '"hello"'
revision fixed false broken ok
before_calls="$(nh_calls)"
eppur_ switch --update --hold-local --no-commit || fail "switch --hold-local gave up"
expect_log "held host a on its last good lock"
expect_log "not activating: host a is held"
[ "$(nh_calls)" = "$before_calls" ] || fail "a held local host was activated"
git -C "$sys" checkout -q -- flake.lock pins.json
write_config ""

step "interrupted: SIGTERM during the build restores flake.lock and pins.json"
revision fixed false slow ok
cp "$sys/flake.lock" "$work/before.lock"
cp "$sys/pins.json" "$work/before.json"
"$eppur" switch --update --config "$config" >"$work/stdout" 2>"$log" &
pid=$!
for _ in $(seq 60); do
  grep -q "round 1" "$log" && break
  sleep 0.5
done
grep -q "round 1" "$log" || fail "the slow build never started"
cp "$log" "$work/bg.log"
if "$eppur" build --config "$config" >/dev/null 2>"$work/second.log"; then fail "a second run started while one was running"; fi
grep -qF "another eppur run is using" "$work/second.log" || fail "the second run was not refused for the lock: $(cat "$work/second.log")"
if "$eppur" pin hello --input toypkgs --config "$config" >/dev/null 2>"$work/second.log"; then fail "eppur pin ran during a run"; fi
grep -qF "another eppur run is using" "$work/second.log" || fail "eppur pin was not refused for the lock: $(cat "$work/second.log")"
sleep 1
kill -TERM "$pid"
set +e
wait "$pid"
rc=$?
set -e
[ "$rc" = 130 ] || fail "an interrupted run should exit 130, not $rc"
expect_log "interrupted; restored flake.lock and pins.json"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored after SIGTERM"
cmp -s "$sys/pins.json" "$work/before.json" || fail "pins.json was not restored after SIGTERM"
revision fixed false 10 ok
eppur_ build --update || fail "eppur build gave up after the interrupted run"

step "--timeout: past the deadline nothing new starts, and the files are restored"
revision fixed false 105 ok
cp "$sys/flake.lock" "$work/before.lock"
cp "$sys/pins.json" "$work/before.json"
mkdir -p "$work/slownix"
real_nix="$(type -P nix)"
cat >"$work/slownix/nix" <<EOF
#!/bin/sh
case "\$*" in
  *"eval --json"*) sleep 4 ;;
  *" build "*) echo "\$*" >>"$work/slownix.builds"; sleep 60 ;;
esac
exec "$real_nix" "\$@"
EOF
chmod +x "$work/slownix/nix"
started=$SECONDS
rc=0
PATH="$work/slownix:$PATH" eppur_ build --update --no-commit --timeout 2s || rc=$?
[ "$rc" = 2 ] || fail "a run past its --timeout should exit 2, not $rc"
expect_log "the run took longer than --timeout"
[ ! -e "$work/slownix.builds" ] || fail "a build started past the deadline: $(cat "$work/slownix.builds")"
[ $((SECONDS - started)) -lt 30 ] || fail "--timeout took $((SECONDS - started))s to stop the run"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored after --timeout"
cmp -s "$sys/pins.json" "$work/before.json" || fail "pins.json was not restored after --timeout"

step "a configuration that no longer evaluates holds that host, and the others move on"
revision fixed false 11 ok true
eppur_ build --update --no-commit || fail "a module error on b should hold b, not stop the run"
reject_log "error (ignored)"
case "$(held_reason b)" in *"does not evaluate"*"services.gone"*) ;; *) fail "b should be held for its module error: $(held_reason b)" ;; esac
expect_built a hello-11
git -C "$sys" checkout -q -- flake.lock pins.json
revision fixed false 12 ok
eppur_ build --update || fail "eppur build gave up after the module error"

step "an insecure package is not pinned around"
revision fixed false insecure ok
cp "$sys/flake.lock" "$work/before.lock"
if eppur_ build --update; then fail "an insecure hello on the local host should give up"; fi
expect_log "nixpkgs refuses hello because it is marked as insecure"
reject_log "pinned host a"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored"
revision fixed false 13 ok
eppur_ build --update || fail "eppur build gave up after the insecure revision"

step "--max-new-pins: a host needing too many new pins is held"
write_config '"linux"'
revision broken false broken ok
eppur_ build --update --hold-local --no-commit --max-new-pins 1 || fail "the run with --max-new-pins 1 gave up"
case "$(held_reason b)" in *"more than 1 new pin in one run"*) ;; *) fail "b should be held for needing two pins: $(held_reason b)" ;; esac
expect_pins a "toypkgs.hello"
git -C "$sys" checkout -q -- flake.lock pins.json
write_config ""
revision fixed false 14 ok
eppur_ build --update || fail "eppur build gave up after the max-new-pins revision"

step "a build that cannot run here stops the run, pins nothing and restores the files"
revision fixed false foreign ok
cp "$sys/flake.lock" "$work/before.lock"
cp "$sys/pins.json" "$work/before.json"
if eppur_ build --update; then fail "a platform mismatch should stop the run"; fi
expect_log "could not run on this machine"
expect_log "platform mismatch"
reject_log "pinned host"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored after the platform mismatch"
cmp -s "$sys/pins.json" "$work/before.json" || fail "pins.json was not restored after the platform mismatch"
revision fixed false 15 ok
eppur_ build --update || fail "eppur build gave up after the platform mismatch"

step "c builds: a host with a widely used library, a nested package and a download"
write_config "" "" '"c"'
revision fixed false 16 ok false ok
eppur_ build --update || fail "c did not build"
expect_pins c ""

step "a library 31 packages depend on is not pinned, even though only one is a top-level attribute"
revision fixed false 17 ok false corebroken
eppur_ build --update --no-commit || fail "the corelib run gave up"
case "$(held_reason c)" in *"corelib-1.0 is used by 31 other packages"*) ;; *) fail "c should be held for corelib: $(held_reason c)" ;; esac
expect_pins c ""
git -C "$sys" checkout -q -- flake.lock pins.json

step "an evaluation failure inside a nested package pins the package that uses it"
revision fixed false 18 ok false innergone
eppur_ -v build --update --no-commit || fail "the innerlib run gave up"
expect_log "innerlib is not pinnable itself; webui, which evaluation fails inside, is"
expect_pins c "toypkgs.webui"
git -C "$sys" checkout -q -- flake.lock pins.json

step "a failed download is built once more, then pinned"
revision fixed false 19 ok false fodfail
mkdir -p "$work/lognix"
real_nix="$(type -P nix)"
cat >"$work/lognix/nix" <<EOF
#!/bin/sh
case "\$*" in *" build "*) echo "\$*" >>"$work/lognix.builds" ;; esac
exec "$real_nix" "\$@"
EOF
chmod +x "$work/lognix/nix"
PATH="$work/lognix:$PATH" eppur_ build --update --no-commit || fail "the download run gave up"
expect_log "building once more"
sed -n 2p "$work/lognix.builds" | grep -qF "fetched-1.1" || fail "the retry did not build the download: $(cat "$work/lognix.builds")"
if sed -n 2p "$work/lognix.builds" | grep -qF -- "-system"; then fail "the retry built more than the download: $(cat "$work/lognix.builds")"; fi
expect_pins c "toypkgs.fetched"
git -C "$sys" checkout -q -- flake.lock pins.json

# A fixed-output path depends only on its name and hash: building a good twin
# before the retry makes the failing download succeed then, whatever the timing.
cat >"$work/lognix/nix" <<EOF
#!/bin/sh
case "\$*" in
  *" build "*)
    echo "\$*" >>"$work/lognix.builds"
    case "\$*" in
      *toplevel*) ;;
      *flaky-*)
        name="\$(printf '%s\n' "\$*" | grep -o 'flaky-[a-z]*-[0-9]*' | head -n1)"
        "$real_nix" --extra-experimental-features "nix-command flakes" build --no-link --impure --expr "derivation { name = \"\$name\"; system = \"$system\"; builder = \"/bin/sh\"; args = [ \"-c\" \"printf x > \\\$out\" ]; outputHashMode = \"flat\"; outputHashAlgo = \"sha256\"; outputHash = \"sha256-LXEWQrcmsEQBYnyp+6wy9chTD7GQPMTbAiWHF5IaSIE=\"; }" || exit 1
        ;;
    esac
    ;;
esac
exec "$real_nix" "\$@"
EOF

step "a download that recovers next to a real failure: only the real one is pinned, and it is not built twice"
write_config "" "" '"p", "q"'
revision fixed false 19 ok
eppur_ build --update --no-commit || fail "the baseline for the recovery run gave up"
git -C "$sys" add -A && git -C "$sys" commit -qm "recovery baseline"
: >"$work/lognix.builds"
revision fixed false 19 ok false realfail
PATH="$work/lognix:$PATH" eppur_ build --update --no-commit || fail "the recovery run gave up"
expect_log "building once more"
expect_pins p ""
expect_pins q "toypkgs.realfail"
[ -z "$(held_reason p)" ] || fail "p was held for a download that recovered: $(held_reason p)"
[ -z "$(held_reason q)" ] || fail "q was held: $(held_reason q)"
reject_log "pinned host p"
sed -n 2p "$work/lognix.builds" | grep -qF "flaky-" || fail "the retry did not build the download: $(cat "$work/lognix.builds")"
if sed -n 2p "$work/lognix.builds" | grep -qF -- "-realfail-1.0"; then fail "the retry built the real failure too: $(cat "$work/lognix.builds")"; fi
git -C "$sys" checkout -q -- flake.lock pins.json

step "one download recovers and another stays dead: each is judged by its own retry"
write_config "" "" '"p", "r"'
: >"$work/lognix.builds"
revision fixed false 19 ok false deadlink
PATH="$work/lognix:$PATH" eppur_ build --update --no-commit || fail "the run with a dead download gave up"
expect_log "building once more"
expect_pins p ""
expect_pins r "toypkgs.deadlink"
reject_log "pinned host p"
[ -z "$(held_reason p)" ] || fail "p was held for a download that recovered: $(held_reason p)"
git -C "$sys" checkout -q -- flake.lock pins.json
write_config ""

step "--old-lock: a new pin comes from the given lock"
write_config "" "" '"b"'
revision fixed false broken ok
eppur_ build --update --no-commit --old-lock "$r3_lock" || fail "the --old-lock run gave up"
expect_pins b "toypkgs.hello"
[ "$(field b toypkgs hello locked.rev)" = "$r3_rev" ] || fail "hello was pinned at $(field b toypkgs hello locked.rev), not the --old-lock revision $r3_rev"
git -C "$sys" checkout -q -- flake.lock pins.json
revision fixed false 20 ok
write_config ""
eppur_ build --update || fail "eppur build gave up after the --old-lock revision"

step "commits are off unless asked for: a config without commit leaves the files, --commit commits"
COMMIT=false write_config ""
revision fixed false 21 ok
before_commits="$(commits)"
eppur_ build --update || fail "the build without commit gave up"
[ "$(commits)" = "$before_commits" ] || fail "a run without commit committed"
expect_log "flake.lock and pins.json uncommitted"
reject_log "pass --"
reject_log "set programs."
reject_log "the next build"
git -C "$sys" diff --quiet -- flake.lock && fail "flake.lock was not updated"
eppur_ build --commit || fail "the build with --commit gave up"
[ "$(commits)" -gt "$before_commits" ] || fail "--commit did not commit"
expect_log 'committed'
expect_log '"eppur: update flake.lock"'
eppur_ build --update --commit --no-commit || fail "the build with --commit --no-commit gave up"
expect_log "flake.lock and pins.json unchanged"
git -C "$sys" checkout -q -- flake.lock pins.json
write_config ""

step "a host that is not in the flake is refused before anything builds"
write_config "" "" '"a", "bb"'
if eppur_ build; then fail "a typo in hosts should be refused"; fi
expect_log "programs.eppur.hosts lists bb, which nixosConfigurations does not have"
typo="$work/typo"
mkdir -p "$typo"
cat >"$typo/flake.nix" <<EOF
{ outputs = _: { nixosConfigurations.real = { }; homeConfigurations = throw "homes are broken"; }; }
EOF
echo '{ "nodes": { "root": { "inputs": { } } }, "root": "root", "version": 7 }' >"$typo/flake.lock"
cat >"$work/typo.json" <<EOF
{ "kind": "nixos", "self": "typo", "flake": "$typo", "hosts": ["typo"], "homes": [] }
EOF
if "$eppur" build --config "$work/typo.json" >"$work/stdout" 2>"$log"; then fail "a typo in hosts was held although homeConfigurations does not evaluate"; fi
expect_log "programs.eppur.hosts lists typo, which nixosConfigurations does not have"
reject_log "round 1"
write_config ""

step "--update with an input the flake does not have is refused"
if eppur_ build --update nixpkgs; then fail "--update nixpkgs should be refused"; fi
expect_log "--update nixpkgs: not an input of flake.lock (its inputs: toydonor, toypkgs, toypkgs2)"
reject_log "round 1"

step "merge: a missing file or one that is not a pins file is an error, and held entries come through"
if eppur_ merge "$work/missing.json"; then fail "merging a missing file succeeded"; fi
expect_log "reading $work/missing.json"
printf '{}\n' >"$work/empty.json"
if eppur_ merge "$work/empty.json"; then fail "merging {} succeeded"; fi
expect_log "is not an eppur pins file"
nix eval --impure --json --expr "
  let f = builtins.fromJSON (builtins.readFile $sys/pins.json); b = f.hosts.b; in
  f // { hosts = f.hosts // { b = b // { lockLastModified = b.lockLastModified + 100;
    held = { since = \"2026-01-01\"; reason = \"held by the fleet\"; lock = builtins.fromJSON (builtins.readFile $sys/flake.lock); }; }; }; }" >"$work/held.json"
eppur_ merge "$work/held.json" || fail "merging a held entry failed"
[ "$(held_reason b)" = "held by the fleet" ] || fail "the held entry did not come through: $(held_reason b)"
git -C "$sys" checkout -q -- pins.json

step "the command line: global flags, status, why and clear say where to look"
"$eppur" --config "$config" --flake "$sys" status >"$work/stdout" 2>"$log" || fail "--flake before the subcommand was refused: $(cat "$log")"
grep -qF "flake $sys" "$work/stdout" || fail "status does not name the flake"
"$eppur" pin hello --host b --input toypkgs --config "$config" 2>"$log" || fail "pinning hello on b failed: $(cat "$log")"
expect_log "pinned host b: hello (toypkgs) at "
reject_log "eppur clear"
"$eppur" status --config "$config" >"$work/stdout" 2>"$log" || fail "status failed"
grep -qE '^TARGET +PACKAGE \(SETS\) +REASON +SINCE$' "$work/stdout" || fail "status has no header: $(cat "$work/stdout")"
grep -qE '^host b +hello \(toypkgs\) +pinned by hand +' "$work/stdout" || fail "status lacks the manual pin: $(cat "$work/stdout")"
if "$eppur" why hello --config "$config" 2>"$log"; then fail "why hello on a succeeded"; fi
expect_log "hello is not pinned on host a; it is pinned on host b (pass --host b)"
if "$eppur" clear --held hello --config "$config" 2>"$log"; then fail "clear --held with an attr was accepted"; fi
"$eppur" clear hello --host b --config "$config" 2>"$log" || fail "clearing hello on b failed"
expect_log "unpinned host b: hello (toypkgs)"
mkdir -p "$work/nopins"
"$eppur" status --flake "$work/nopins" --config "$config" >"$work/stdout" 2>"$log" || fail "status in a directory without pins.json failed"
grep -qF "no pins.json in $work/nopins" "$work/stdout" || fail "status did not say pins.json is missing: $(cat "$work/stdout")"
if "$eppur" status --config "$work/nonexistent.json" 2>"$log"; then fail "a missing --config was ignored"; fi
expect_log "reading $work/nonexistent.json"
if "$eppur" pin hello --flake "$work/nopins" --config "$config" 2>"$log"; then fail "pin in a directory without flake.lock succeeded"; fi
expect_log "no flake.lock in $work/nopins"
git -C "$sys" checkout -q -- pins.json

step "two package sets: a set that did not move is skipped, the one that did is pinned"
write_config "" "" '"e"'
revision fixed false 30 ok
eppur_ build --update || fail "e did not build"
revision broken false 31 ok
eppur_ build --update toypkgs || fail "updating only the unused set gave up"
expect_pins e ""
eppur_ -v build --update toypkgs2 || fail "updating the used set gave up"
expect_log "e: not pinning app (toypkgs): toypkgs is still at the revision this host last built with"
expect_pins e "toypkgs2.app"
[ -z "$(held_reason e)" ] || fail "e was held although pinning toypkgs2 fixed it: $(held_reason e)"

step "two package sets: one failure is one pin group, retried together and not reported fixed while it fails"
write_config "" '"retrySameDerivationAfterDays": 0' '"e"'
revision fixed false 32 ok
eppur_ build --update || fail "the fix for e gave up"
expect_pins e ""
revision broken false 33 ok
eppur_ build --update || fail "breaking both sets gave up"
expect_log "pinned host e: app (toypkgs, toypkgs2)"
expect_log "done: 1 built, 1 pinned"
expect_pins e "toypkgs.app toypkgs2.app"
"$eppur" status --config "$config" >"$work/stdout" 2>/dev/null
grep -qE '^host e +app \(toypkgs, toypkgs2\) ' "$work/stdout" || fail "status does not show one row for app: $(cat "$work/stdout")"
revision broken false 34 ok
eppur_ build --update toypkgs || fail "moving the unused set gave up"
reject_log "unpinned host e"
expect_log "keeping its pin in toypkgs (since"
expect_pins e "toypkgs.app toypkgs2.app"
revision broken false 35 ok
eppur_ build --update toypkgs2 || fail "moving the used set gave up"
[ -z "$(held_reason e)" ] || fail "e was held while app still fails: $(held_reason e)"
expect_pins e "toypkgs.app toypkgs2.app"
revision fixed false 36 ok
eppur_ build --update || fail "fixing app gave up"
expect_log "unpinned host e: app (toypkgs, toypkgs2): builds again"
expect_pins e ""

step "a system that embeds the flake's source is built again with the final pins.json"
write_config "" "" '"d", "b"'
revision fixed false 37 ok
eppur_ build --update --no-commit || fail "d and b did not build"
git -C "$sys" commit -qam "d builds"
revision broken false 38 ok
eppur_ build --update --no-commit --gc-roots "$work/roots2" || fail "the run with d gave up"
expect_log "building 1 system again with the final pins.json"
want="$(nix eval --raw "$sys#nixosConfigurations.d.config.system.build.toplevel.outPath")"
[ "$(readlink "$work/roots2/eppur-host-d")" = "$want" ] || fail "d's root is not the system for the final pins.json"
git -C "$sys" checkout -q -- flake.lock pins.json
write_config ""

step "a commit that fails restores flake.lock and pins.json"
revision fixed false 39 ok
cp "$sys/flake.lock" "$work/before.lock"
if GIT_AUTHOR_NAME= GIT_COMMITTER_NAME= eppur_ build --update --commit; then fail "a failing commit was not an error"; fi
expect_log "git commit failed"
expect_log "restored flake.lock and pins.json"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored after the failed commit"

step "--fail-on-hold exits with 3 when a host is held"
write_config '"linux"'
revision fixed false 40 broken
rc=0
eppur_ build --update --no-commit --fail-on-hold || rc=$?
[ "$rc" = 3 ] || fail "--fail-on-hold exited with $rc, not 3"
git -C "$sys" checkout -q -- flake.lock pins.json
write_config ""

step "--host with a name that is not configured is refused"
if "$eppur" pin hello --host zzz --input toypkgs --config "$config" 2>"$log"; then fail "pin --host zzz succeeded"; fi
expect_log "host zzz is not a configured host (configured: a, b); check --host"

step "retry keeps a pin that still fails; clear removes it and the next run pins it again at the same revision"
write_config "" "" '"b"'
revision fixed false 41 ok
eppur_ build --update || fail "b did not build before the retry steps"
revision broken false 41 ok
eppur_ build --update || fail "pinning app on b gave up"
expect_pins b "toypkgs.app"
app_rev="$(field b toypkgs app locked.rev)"
"$eppur" retry app --host b --config "$config" 2>"$log" || fail "retry app --host b failed: $(cat "$log")"
expect_log "host b: retry queued for app (toypkgs)"
"$eppur" status --config "$config" >"$work/stdout" 2>/dev/null
grep -qF "retry queued" "$work/stdout" || fail "status does not show the queued retry: $(cat "$work/stdout")"
eppur_ build --no-commit || fail "the retry run gave up"
expect_log "retrying host b: app (toypkgs)"
expect_log "app still fails; keeping its pin in toypkgs"
expect_pins b "toypkgs.app"
[ -z "$(held_reason b)" ] || fail "b was held after a retry of a pin that still fails: $(held_reason b)"
"$eppur" clear app --host b --config "$config" 2>"$log" || fail "clear app --host b failed"
eppur_ build --no-commit || fail "the run after clear gave up"
expect_pins b "toypkgs.app"
[ "$(field b toypkgs app locked.rev)" = "$app_rev" ] || fail "app was pinned again at $(field b toypkgs app locked.rev), not $app_rev"
[ -z "$(held_reason b)" ] || fail "b was held after clear: $(held_reason b)"
if "$eppur" retry hello --host b --config "$config" 2>"$log"; then fail "retry of an attr that is not pinned succeeded"; fi
expect_log "hello is not pinned on host b"

step "discarding a run's changes does not leave a pin that cannot work"
git -C "$sys" checkout -q -- flake.lock pins.json
revision broken false 42 ok
eppur_ build --update --no-commit || fail "the run after discarding gave up"
expect_pins b "toypkgs.app"
[ "$(field b toypkgs app locked.rev)" = "$app_rev" ] || fail "app was pinned at $(field b toypkgs app locked.rev) after discarding, not $app_rev"
[ -z "$(held_reason b)" ] || fail "b was held after discarding a run: $(held_reason b)"
git -C "$sys" checkout -q -- flake.lock pins.json

step "retry of a pin whose package builds now removes it"
nix eval --impure --json --expr "
  let f = builtins.fromJSON (builtins.readFile $sys/pins.json); l = builtins.fromJSON (builtins.readFile $sys/flake.lock); b = f.hosts.b; in
  f // { hosts = f.hosts // { b = b // { pins.toypkgs.hello = {
    locked = (builtins.fromJSON (builtins.readFile $r3_lock)).nodes.toypkgs.locked; since = \"2026-01-01\"; reason = \"build\";
    checkedNarHash = l.nodes.toypkgs.locked.narHash; }; }; }; }" >"$work/with-hello.json"
cp "$work/with-hello.json" "$sys/pins.json"
"$eppur" retry hello --host b --config "$config" 2>"$log" || fail "retry hello failed: $(cat "$log")"
eppur_ build --no-commit || fail "the retry of hello gave up"
expect_log "unpinned host b: hello (toypkgs): builds again"
git -C "$sys" checkout -q -- flake.lock pins.json
if "$eppur" retry app --host b --input nope --config "$config" 2>"$log"; then fail "retry --input nope succeeded"; fi
expect_log "app is not pinned in nope on host b (it is pinned in toypkgs)"

step "why on a held host says so"
write_config '"app"' "" '"b"'
"$eppur" clear app --host b --config "$config" 2>/dev/null || fail "clearing app failed"
eppur_ build --no-commit || fail "holding b gave up"
case "$(held_reason b)" in *"never-pin list"*) ;; *) fail "b should be held for app: $(held_reason b)" ;; esac
if "$eppur" why linux --host b --config "$config" 2>"$log"; then fail "why linux on b succeeded"; fi
expect_log "host b is held since"
git -C "$sys" checkout -q -- flake.lock pins.json
write_config "" "" '"b"'

step "pin --old-lock pins at the revision in another flake.lock"
"$eppur" pin hello --host b --input toypkgs --old-lock "$r3_lock" --config "$config" 2>"$log" || fail "pin --old-lock failed: $(cat "$log")"
[ "$(field b toypkgs hello locked.rev)" = "$r3_rev" ] || fail "hello was pinned at $(field b toypkgs hello locked.rev), not $r3_rev"
git -C "$sys" checkout -q -- pins.json
revision fixed false 42 ok
write_config ""
eppur_ build --update || fail "eppur build gave up after the clear steps"

step "--gc-roots keeps the root of a held host, and prunes it once the host is no longer configured"
eppur_ build --hold-local --no-commit --gc-roots "$work/roots4" || fail "rooting a and b failed"
[ -L "$work/roots4/eppur-host-b" ] || fail "no root for b"
write_config '"linux"'
revision fixed false 43 broken
eppur_ build --update --hold-local --no-commit --gc-roots "$work/roots4" || fail "holding b gave up"
[ -n "$(held_reason b)" ] || fail "b should be held"
[ -L "$work/roots4/eppur-host-b" ] || fail "the held host's root was removed"
step "--gc-roots takes over the roots made when eppur was called nixheal"
held_root=$(readlink "$work/roots4/eppur-host-b")
for r in a b; do mv "$work/roots4/eppur-host-$r" "$work/roots4/nixheal-host-$r"; done
for m in "$work"/roots4/.eppur-roots-*; do
  sed 's/^eppur-/nixheal-/' "$m" >"${m/.eppur-roots-/.nixheal-roots-}"
  rm "$m"
done
eppur_ build --hold-local --no-commit --gc-roots "$work/roots4" || fail "the run with old roots gave up"
[ "$(readlink "$work/roots4/eppur-host-b")" = "$held_root" ] || fail "the held host's old root was not taken over"
[ ! -e "$work/roots4/nixheal-host-b" ] || fail "the old root of b was left behind"
ls -A "$work/roots4" | grep -q '^\.nixheal-roots-' && fail "the old manifest was left behind"
write_config "" "" '"a"'
eppur_ build --hold-local --no-commit --gc-roots "$work/roots4" || fail "the run without b gave up"
expect_log "removed the root eppur-host-b"
git -C "$sys" checkout -q -- flake.lock pins.json
revision fixed false 44 ok
write_config ""

step "--only host:NAME picks the host"
eppur_ build --update --no-commit --only host:b || fail "--only host:b gave up"
expect_log "round 1: evaluating and building host b"
reject_log "host a builds"
git -C "$sys" checkout -q -- flake.lock pins.json

step "merge ranks a hold by the lock it was tried against"
nix eval --impure --json --expr "
  let f = builtins.fromJSON (builtins.readFile $sys/pins.json); b = f.hosts.b; in
  f // { hosts = f.hosts // { b = b // { lockLastModified = b.lockLastModified - 100; checkedLastModified = b.lockLastModified + 100;
    held = { since = \"2026-01-01\"; reason = \"held on an older lock\"; lock = builtins.fromJSON (builtins.readFile $sys/flake.lock); }; }; }; }" >"$work/held-older.json"
eppur_ merge "$work/held-older.json" || fail "merging the held entry failed"
[ "$(held_reason b)" = "held on an older lock" ] || fail "a hold tried against a newer lock was not taken: $(held_reason b)"
git -C "$sys" checkout -q -- pins.json

step "a flake in a subdirectory of its repository"
repo2="$work/repo2"
mkdir -p "$repo2/nixos"
git -C "$repo2" init -q -b main
cp "$sys/flake.nix" "$sys/overlay.nix" "$sys/platform.nix" "$sys/import-pinned.nix" "$sys/marked.nix" "$repo2/nixos/"
git -C "$repo2" add nixos/flake.nix nixos/overlay.nix nixos/platform.nix nixos/import-pinned.nix nixos/marked.nix
nix flake lock "$repo2/nixos" 2>/dev/null
git -C "$repo2" add nixos/flake.lock
git -C "$repo2" commit -qm "a flake in nixos/"
sub_config="$work/sub.json"
cat >"$sub_config" <<EOF
{ "kind": "nixos", "self": "a", "flake": "$repo2/nixos", "hosts": ["a"], "homes": [], "warnAfterDays": 14, "deny": [] }
EOF
"$eppur" build --config "$sub_config" >"$work/stdout" 2>"$log" || fail "a flake in a subdirectory was refused: $(cat "$log")"
[ -f "$repo2/nixos/pins.json" ] || fail "pins.json was not created next to the subdirectory's flake.lock"
git -C "$repo2" ls-files --error-unmatch nixos/pins.json >/dev/null || fail "the subdirectory's pins.json is not tracked"
[ -e "$repo2/nixos/.eppur.lock" ] && fail "the run lock was left in the work tree"

step "an error after a run starts on a fresh checkout leaves no pins.json behind, and a dry run untracks one it added"
git -C "$repo2" rm -q --cached nixos/pins.json
rm -f "$repo2/nixos/pins.json"
if GIT_AUTHOR_NAME= GIT_COMMITTER_NAME= "$eppur" build --commit --config "$sub_config" 2>"$log"; then fail "a failing commit on a fresh checkout was not an error"; fi
expect_log "git commit failed"
[ -e "$repo2/nixos/pins.json" ] && fail "the failed run left pins.json behind"
git -C "$repo2" ls-files --error-unmatch nixos/pins.json >/dev/null 2>&1 && fail "the failed run left pins.json in the index"
printf '{"version":2}\n' >"$repo2/nixos/pins.json"
"$eppur" build --dry-run --config "$sub_config" 2>"$log" || fail "the dry run with an untracked pins.json failed: $(cat "$log")"
git -C "$repo2" ls-files --error-unmatch nixos/pins.json >/dev/null 2>&1 && fail "the dry run left pins.json in the index"
[ -e "$repo2/nixos/pins.json" ] || fail "the dry run removed a pins.json it did not create"
reject_log "restored flake.lock"

step "a tracked pins.json that was deleted stays an unstaged deletion after a dry run"
git -C "$repo2" add nixos/pins.json
git -C "$repo2" commit -qm "pins.json"
rm "$repo2/nixos/pins.json"
"$eppur" build --dry-run --config "$sub_config" 2>"$log" || fail "the dry run without the tracked pins.json failed: $(cat "$log")"
[ "$(git -C "$repo2" status --porcelain -- nixos/pins.json)" = " D nixos/pins.json" ] || fail "the deletion of pins.json changed: $(git -C "$repo2" status --porcelain -- nixos/pins.json)"
git -C "$repo2" checkout -q -- nixos/pins.json

step "an unreadable pins.json is an error, and is kept"
chmod 000 "$repo2/nixos/pins.json"
if "$eppur" build --config "$sub_config" 2>"$log"; then fail "an unreadable pins.json was accepted"; fi
chmod 644 "$repo2/nixos/pins.json"
expect_log "reading $repo2/nixos/pins.json"
[ -e "$repo2/nixos/pins.json" ] || fail "the unreadable pins.json was removed"

step "two flakes rooting a host of the same name in one --gc-roots directory are refused"
"$eppur" build --config "$sub_config" --gc-roots "$work/roots5" 2>"$log" || fail "rooting the subdirectory flake failed: $(cat "$log")"
write_config "" "" '"a"'
if eppur_ build --no-commit --gc-roots "$work/roots5"; then fail "a second flake rooting host a in the same directory was accepted"; fi
expect_log "already roots eppur-host-a for another flake"
git -C "$sys" checkout -q -- flake.lock pins.json
write_config ""

step "a switcher that fails exits with 4 and says the build is kept"
export STUB_FAIL=1
rc=0
eppur_ switch --no-commit || rc=$?
unset STUB_FAIL
[ "$rc" = 4 ] || fail "a failing switcher exited with $rc, not 4"
expect_log "nh failed"
expect_log "flake.lock and pins.json kept"
git -C "$sys" checkout -q -- flake.lock pins.json

step "a Home Manager config with null settings (as homeManagerConfiguration passes it) takes a pin"
cat >"$config" <<EOF
{ "kind": "home", "self": "elaborated", "flake": "$sys", "hosts": [], "homes": ["elaborated"],
  "warnAfterDays": 14, "deny": [] }
EOF
revision fixed false 60 ok
eppur_ build --update --no-commit || fail "the elaborated home did not build"
revision fixed false broken ok
eppur_ -v build --update --no-commit || fail "pinning hello for a home whose config has null settings gave up"
expect_log "home elaborated builds"
expect_log "pinned home elaborated: hello (toypkgs)"
git -C "$sys" checkout -q -- flake.lock pins.json

step "a pinned package keeps the host's own overlays"
write_config "" "" '"o"'
revision fixed false 70 ok
eppur_ build --update --no-commit || fail "o did not build"
expect_built o "hello-70-patched"
revision fixed false broken ok
eppur_ build --update --no-commit || fail "pinning hello on o gave up"
expect_pins o "toypkgs.hello"
[ "$(field o toypkgs hello overlays)" = "1" ] || fail "the pin of hello on o does not carry the overlays"
expect_built o "hello-70-patched"
git -C "$sys" checkout -q -- flake.lock pins.json

step "an overlay that does not evaluate on the old nixpkgs: the pin goes without it, with a warning"
write_config "" "" '"b"'
revision fixed false 71 ok
eppur_ build --update --no-commit || fail "b did not build"
write_config "" "" '"o2"'
revision fixed false broken ok false tagarg
eppur_ build --update --no-commit || fail "pinning hello on o2 gave up"
expect_log "o2: hello (toypkgs) is pinned without your overlays: with them it does not evaluate on the old nixpkgs"
expect_pins o2 "toypkgs.hello"
[ "$(field o2 toypkgs hello overlays)" = "" ] || fail "the pin of hello on o2 should not carry the overlays"
expect_built o2 "hello-71"
git -C "$sys" checkout -q -- flake.lock pins.json
write_config ""

step "an update that leaves the failed package's drv as it was does not build it again"
write_config "" "" '"a"'
revision fixed false 80 ok
eppur_ build --update --no-commit || fail "a did not build on 80"
revision fixed false broken ok
eppur_ build --update --no-commit || fail "pinning hello on a gave up"
expect_pins a "toypkgs.hello"
[ -n "$(field a toypkgs hello rootDrv)" ] || fail "the hello pin records no rootDrv"
revision broken false broken ok
eppur_ build --update --no-commit || fail "the update that leaves hello as it was gave up"
expect_log "host a: hello (toypkgs): same failed derivation, not rebuilt until "
reject_log "retrying host a: hello"
expect_pins a "toypkgs.hello"

step "with retrySameDerivationAfterDays = 0 it is retried, and the day is recorded"
write_config "" '"retrySameDerivationAfterDays": 0' '"a"'
revision fixed true broken ok
eppur_ build --update --no-commit || fail "the retry of hello gave up"
expect_log "retrying host a: hello (toypkgs)"
expect_log "host a: hello still fails; keeping its pin in toypkgs"
[ -n "$(field a toypkgs hello tried)" ] || fail "the failed retry of hello recorded no day"

step "a changed drv is retried, and released when it builds"
write_config "" "" '"a"'
revision fixed false 81 ok
eppur_ build --update --no-commit || fail "the fixed hello gave up"
expect_log "unpinned host a: hello (toypkgs): builds again"
expect_pins a ""
git -C "$sys" checkout -q -- flake.lock pins.json

step "donors: a donor's newer copy that is not built is not used"
write_config "" '"donors": ["toydonor"]' '"a"'
revision fixed false 82 ok
eppur_ build --update --no-commit || fail "a did not build on 82"
revision fixed false broken ok
eppur_ build --update --no-commit || fail "pinning hello on a with a donor gave up"
expect_pins a "toypkgs.hello"
[ -z "$(field a toypkgs hello donor)" ] || fail "hello was pinned to a donor whose copy is not built"
[ "$(field a toypkgs hello leaf)" = 1 ] || fail "hello, which nothing depends on, is not marked as a leaf"
expect_built a hello-82

step "donors: once the donor's copy is built, the pin moves to it"
nix build --no-link --impure --expr "(import $donor { system = \"$system\"; }).hello" || fail "building the donor's hello failed"
eppur_ build --no-commit || fail "moving the hello pin to the donor gave up"
expect_log "moved pin host a: hello (toypkgs) to toydonor at "
[ "$(field a toypkgs hello donor)" = toydonor ] || fail "the hello pin did not move to toydonor"
expect_built a hello-90
eppur_ status || fail "eppur status failed"
expect_log "hello-broken failed to build"
expect_log "90 from toydonor at "
"$eppur" status --json --config "$config" >"$work/status.json" 2>/dev/null || fail "eppur status --json failed"
status_field() { nix eval --raw --impure --expr "let r = builtins.head (builtins.filter (i: i.attr or \"\" == \"hello\") (builtins.fromJSON (builtins.readFile $work/status.json)).items); in toString (r.$1 or \"\")"; }
[ "$(status_field label)" = "today, hello-broken failed to build; pinned to 90 from toydonor" ] || fail "status --json label for hello: $(status_field label)"
[ "$(status_field version)" = 90 ] || fail "status --json gives hello's version as $(status_field version)"
[ "$(status_field donor)" = toydonor ] || fail "status --json gives hello's donor as $(status_field donor)"
[ "$(status_field failed)" = hello-broken ] || fail "status --json gives what failed as $(status_field failed)"
[ -n "$(status_field retry_from)" ] || fail "status --json has no retry_from for hello"
eppur_ why hello || fail "eppur why hello failed"
expect_log ", of donor toydonor), version 90"
expect_log "retried:   once its derivation changes, or from "
git -C "$sys" checkout -q -- flake.lock pins.json

step "donors: a new pin goes to the donor's built copy right away"
revision fixed false 83 ok
eppur_ build --update --no-commit || fail "a did not build on 83"
revision fixed false broken ok
eppur_ build --update --no-commit || fail "pinning hello to the donor gave up"
expect_log "host a: hello (toypkgs) is pinned to toydonor"
[ "$(field a toypkgs hello donor)" = toydonor ] || fail "the new hello pin is not from toydonor"
expect_built a hello-90
git -C "$sys" checkout -q -- flake.lock pins.json
write_config ""


# Flake inputs: packages that come from an input other than a package set never
# pass through the overlay, so the input itself goes back in flake.lock.
sys="$work/system-inputs"
exta="$work/exta"
extb="$work/extb"
mkdir -p "$sys" "$exta" "$extb"
export_ext() {
  # $1 directory, $2 name, $3 version, $4 ok, broken, evalfail, or needsok: it
  # builds only while the toy's extra is ok, so an update elsewhere can break it.
  local body="echo $2-$3 > \$out"
  [ "$4" = broken ] && body="echo $2 does not compile >&2; exit 1"
  [ "$4" = needsok ] && body="[ \${extra} = ok ] || exit 1; echo $2-$3 > \$out"
  [ "$4" = faila ] && body="[ \${host} != a ] || exit 1; echo $2-$3 > \$out"
  [ "$4" = failb ] && body="[ \${host} != b ] || exit 1; echo $2-$3 > \$out"
  local value="derivation { name = \"$2-web-$3\"; inherit system; salt = \"$salt\"; builder = \"/bin/sh\"; args = [ \"-c\" \"$body\" ]; }"
  [ "$4" = evalfail ] && value="throw \"$2 lost an option\""
  [ "$4" = configfail ] && value="{ configError = true; }"
  [ "$4" = gluefail ] && value="$value // { glueError = true; }"
  [ "$4" = srcbroken ] && value="derivation { name = \"$2-web-$3\"; inherit system; salt = \"$salt\"; src = ./.; builder = \"/bin/sh\"; args = [ \"-c\" \"exit 1\" ]; }"
  printf '{ system, toy, host ? \"\" }:\nlet extra = (import (toy + "/state.nix")).extra; in\n%s\n' "$value" >"$1/default.nix"
  git -C "$1" add -A
  git -C "$1" commit -qm "$2 $3 $4"
}
git -C "$exta" init -q -b main
git -C "$extb" init -q -b main
export_ext "$exta" exta 1 ok
export_ext "$extb" extb 1 ok

cp "$repo/nix/overlay.nix" "$repo/nix/platform.nix" "$repo/nix/import-pinned.nix" "$repo/nix/marked.nix" "$sys/"
cat >"$sys/flake.nix" <<EOF
{
  inputs.toypkgs = { url = "git+file://$toy"; flake = false; };
  inputs.exta = { url = "git+file://$exta"; flake = false; };
  inputs.extb = { url = "git+file://$extb"; flake = false; };
  outputs = { self, toypkgs, exta, extb, ... }:
    let
      system = "$system";
      mk = name: paths:
        let
          pkgs = import toypkgs {
            inherit system;
            overlays = [ (import ./overlay.nix { pins = ./pins.json; kind = "hosts"; inherit name; input = "toypkgs"; }) ];
          };
          a = import exta { inherit system; toy = toypkgs; host = name; };
          b = import extb { inherit system; toy = toypkgs; host = name; };
        in {
          config.programs.eppur =
            if pkgs.moduleError || a.configError or false then
              throw "The option \`services.gone' does not exist. Definition values:"
            else {
              enable = true;
              inherit name;
              pinsFile = ./pins.json;
              packageSets.toypkgs = pkgs;
            };
          config.system.build.toplevel = derivation {
            name = "toplevel-\${name}";
            inherit system;
            paths = paths pkgs a b;
            builder = "/bin/sh";
            args = [ "-c" (if name != "c" && a.glueError or false then "exit 1" else "echo \$paths > \$out") ];
          };
        };
    in {
      nixosConfigurations.a = mk "a" (p: x: y: [ p.hello x y ]);
      nixosConfigurations.b = mk "b" (p: x: y: [ p.hello x y ]);
      nixosConfigurations.c = mk "c" (p: x: y: [ p.hello ]);
    };
}
EOF
write_inputs_config() {
  COMMIT=false write_config "" "\"input\": \"toypkgs\", \"inputDeny\": [${1:-}]" '"a", "b", "c"'
}
locked_rev() { nix eval --raw --impure --expr "(builtins.fromJSON (builtins.readFile $sys/flake.lock)).nodes.$1.locked.rev"; }
input_field() { jq_pins "toString (f.inputs.$1.$2 or \"\")"; }
input_pins() { jq_pins "builtins.concatStringsSep \" \" (builtins.attrNames (f.inputs or { }))"; }
expect_input_pins() {
  local got
  got="$(input_pins)"
  [ "$got" = "$1" ] || fail "expected input pins [$1], got [$got]"
  echo "input pins: [$got]"
}
expect_not_held() { [ -z "$(held_reason "$1")" ] || fail "$1 is held: $(held_reason "$1")"; }
write_inputs_config
git -C "$sys" init -q -b main
git -C "$sys" add flake.nix overlay.nix platform.nix import-pinned.nix marked.nix
revision fixed false 100 ok
nix flake lock "$sys"
git -C "$sys" add flake.lock
git -C "$sys" commit -qm system

step "inputs: everything builds at first, and pins.json has no input pins"
eppur_ build || fail "the first build of the inputs flake gave up"
expect_input_pins ""
grep -q inputsChanged "$sys/pins.json" && fail "a pins.json without input pins mentions them"
git -C "$sys" add -A
git -C "$sys" commit -qm built
good_a="$(locked_rev exta)"
good_b="$(locked_rev extb)"

step "inputs: two inputs move, one breaks: only that one goes back, the rest still update"
export_ext "$exta" exta 2 broken
export_ext "$extb" extb 2 ok
revision fixed false 101 ok
new_toy="$(git -C "$toy" rev-parse HEAD)"
eppur_ build --update --no-commit || fail "rolling back exta gave up"
expect_log "pinned input exta at "
reject_log "pinned input extb"
reject_log "held host"
expect_input_pins exta
for h in a b c; do expect_not_held "$h"; done
[ "$(locked_rev exta)" = "$good_a" ] || fail "exta did not go back to $good_a: $(locked_rev exta)"
[ "$(locked_rev extb)" != "$good_b" ] || fail "extb, which is fine, was held back too"
[ "$(locked_rev toypkgs)" = "$new_toy" ] || fail "toypkgs did not update past the broken exta"
expect_built a hello-101
[ "$(input_field exta reason)" = build ] || fail "exta should be a build pin"
[ -n "$(input_field exta failedDrv)" ] || fail "exta records no failed drv"
expect_log "1 pinned"
git -C "$sys" add -A
git -C "$sys" commit -qm "exta pinned"

step "inputs: while upstream stays broken, update leaves the lock and pins byte for byte"
cp "$sys/flake.lock" "$work/lock.before"
cp "$sys/pins.json" "$work/pins.before"
eppur_ update || fail "eppur update failed"
cmp "$sys/flake.lock" "$work/lock.before" || fail "update moved the lock of a pinned input: $(cat "$log")"
cmp "$sys/pins.json" "$work/pins.before" || fail "update changed pins.json for nothing"
eppur_ update || fail "the second eppur update failed"
cmp "$sys/flake.lock" "$work/lock.before" || fail "a second update moved the lock"

step "inputs: a revision that can no longer be fetched neither stops updates nor changes pins.json each poll"
sed -i "s/$good_a/0000000000000000000000000000000000000000/" "$sys/pins.json"
eppur_ update || fail "update stopped on a pinned revision that is gone"
expect_log "cannot put input exta back"
[ "$(input_field exta retrying)" = 1 ] || fail "the pin is not retrying after failing to restore"
cp "$sys/pins.json" "$work/pins.gone"
eppur_ update || fail "the second update stopped on a pinned revision that is gone"
cmp "$sys/pins.json" "$work/pins.gone" || fail "a second failed restore changed pins.json"
git -C "$sys" checkout -q -- flake.lock pins.json

step "inputs: a machine configured for other targets keeps the pin and does not decide about it"
COMMIT=false write_config "" '"input": "toypkgs"' '"c"'
eppur_ build --hold-local --no-commit || fail "the build for c alone gave up"
reject_log "unpinned input"
expect_input_pins exta
git -C "$sys" checkout -q -- flake.lock pins.json
write_inputs_config

step "inputs: status, status --json and why show the pin"
eppur_ status || fail "eppur status failed"
expect_log "input exta"
expect_log "1 pinned flake input"
"$eppur" status --json --config "$config" >"$work/status.json" 2>/dev/null || fail "status --json failed"
json_field() { nix eval --raw --impure --expr "let j = builtins.fromJSON (builtins.readFile $work/status.json); in toString ($1)"; }
[ "$(json_field j.input_pins)" = 1 ] || fail "status --json: input_pins is $(json_field j.input_pins)"
[ "$(json_field j.status)" = PINNED ] || fail "status --json: status is $(json_field j.status)"
[ "$(json_field '(builtins.head j.items).name')" = "flake input exta" ] || fail "status --json has no row for exta"
eppur_ why exta || fail "eppur why exta failed"
expect_log "held back in flake.lock"
expect_log "exta does not compile"

step "inputs: nix flake update alone is undone by the next build"
nix flake update --flake "$sys" 2>/dev/null
[ "$(locked_rev exta)" != "$good_a" ] || fail "nix flake update did not move exta"
eppur_ build --no-commit || fail "the build after a plain update gave up"
[ "$(locked_rev exta)" = "$good_a" ] || fail "the build did not put exta back"
expect_input_pins exta
cmp "$sys/flake.lock" "$work/lock.before" || fail "putting exta back gave another lock"

step "inputs: a new revision that still fails is pinned again, keeping its date"
sed -i 's/"since": "[^"]*"/"since": "2026-01-01"/' "$sys/pins.json"
git -C "$sys" add -A
git -C "$sys" commit -qm "an older pin"
failed_before="$(input_field exta failedNarHash)"
export_ext "$exta" exta 3 broken
eppur_ update || fail "eppur update failed"
[ "$(locked_rev exta)" != "$good_a" ] || fail "update did not let a new exta through"
[ "$(input_field exta retrying)" = 1 ] || fail "the new exta is not marked as retrying"
eppur_ build --no-commit || fail "retrying exta gave up"
expect_log "pinned input exta at "
[ "$(locked_rev exta)" = "$good_a" ] || fail "exta did not go back again"
[ "$(input_field exta since)" = 2026-01-01 ] || fail "the pin lost its date: $(input_field exta since)"
[ "$(input_field exta failedNarHash)" != "$failed_before" ] || fail "the pin does not record the newer failure"
[ -z "$(input_field exta retrying)" ] || fail "the pin is still retrying"
git -C "$sys" add -A
git -C "$sys" commit -qm "pinned again"

step "inputs: without any state, a retried pin still knows where it last built"
export_ext "$exta" exta 3b broken
eppur_ update || fail "eppur update failed"
XDG_STATE_HOME="$work/no-state" eppur_ build --no-commit || fail "retrying exta with no state gave up"
[ "$(locked_rev exta)" = "$good_a" ] || fail "exta did not go back to its pin with no state"
[ -z "$(input_field exta retrying)" ] || fail "the pin is still retrying"
git -C "$sys" add -A
git -C "$sys" commit -qm "pinned again, without state"

step "inputs: a fixed revision takes the pin away, whichever targets are gone by then"
sed -i 's/"host a",/"host gone",\n        "host a",/' "$sys/pins.json"
grep -q '"host gone"' "$sys/pins.json" || fail "could not add a removed target to the pin"
export_ext "$exta" exta 4 ok
eppur_ update || fail "eppur update failed"
eppur_ build --no-commit || fail "the fixed exta gave up"
expect_log "unpinned input exta: builds again"
expect_input_pins ""
[ "$(locked_rev exta)" = "$(git -C "$exta" rev-parse HEAD)" ] || fail "exta is not at its fixed revision"
expect_built a exta-web-4
git -C "$sys" add -A
git -C "$sys" commit -qm "exta released"

step "inputs: an input that breaks evaluation goes back too"
good_a="$(locked_rev exta)"
export_ext "$exta" exta 5 evalfail
eppur_ build --update --no-commit || fail "rolling back the evaluation failure gave up"
expect_log "pinned input exta at "
[ "$(input_field exta reason)" = eval ] || fail "exta should be an eval pin"
[ "$(locked_rev exta)" = "$good_a" ] || fail "exta did not go back"
for h in a b c; do expect_not_held "$h"; done
git -C "$sys" checkout -q -- flake.lock pins.json
export_ext "$exta" exta 6 ok
eppur_ build --update --no-commit || fail "the build after the evaluation failure gave up"
expect_input_pins ""
git -C "$sys" add -A
git -C "$sys" commit -qm "exta 6"

step "inputs: a configuration that stops evaluating blames the input, never the package set"
good_a="$(locked_rev exta)"
export_ext "$exta" exta 6b configfail
eppur_ build --update --no-commit || fail "rolling back the configuration failure gave up"
expect_log "pinned input exta at "
[ "$(locked_rev exta)" = "$good_a" ] || fail "exta did not go back"
for h in a b c; do expect_not_held "$h"; done
git -C "$sys" checkout -q -- flake.lock pins.json
export_ext "$exta" exta 6c ok
revision fixed false 100b ok true
eppur_ build --update --no-commit --hold-local || fail "the package set's configuration failure gave up"
reject_log "pinned input"
expect_input_pins ""
[ -n "$(held_reason a)" ] || fail "a is not held when the package set breaks its configuration"
[ "$(locked_rev toypkgs)" = "$(git -C "$toy" rev-parse HEAD)" ] || fail "the package set was rolled back"
git -C "$sys" checkout -q -- flake.lock pins.json
revision fixed false 100c ok
eppur_ build --update --no-commit || fail "the build after the configuration failures gave up"
expect_input_pins ""
git -C "$sys" add -A
git -C "$sys" commit -qm "configurations evaluate again"

step "inputs: a failure in a drv every input feeds blames only the input that causes it"
good_a="$(locked_rev exta)"
export_ext "$exta" exta 6d gluefail
export_ext "$extb" extb 6d ok
eppur_ build --update --no-commit || fail "rolling back for the system's own drv gave up"
expect_log "pinned input exta at "
reject_log "pinned input extb"
expect_input_pins exta
[ "$(locked_rev exta)" = "$good_a" ] || fail "exta did not go back"
[ "$(locked_rev extb)" = "$(git -C "$extb" rev-parse HEAD)" ] || fail "extb, which did nothing, was held back"
git -C "$sys" checkout -q -- flake.lock pins.json
export_ext "$exta" exta 6e ok
eppur_ build --update --no-commit || fail "the build after the glue failure gave up"
expect_input_pins ""
git -C "$sys" add -A
git -C "$sys" commit -qm "glue is fine again"

step "inputs: a config that leaves targets out does not touch the lock of the whole flake"
good_a="$(locked_rev exta)"
export_ext "$exta" exta 6e2 broken
COMMIT=false write_config "" '"input": "toypkgs"' '"a"'
if eppur_ -v build --update --no-commit; then fail "a narrower config got past the broken exta"; fi
expect_log "no flake input is rolled back"
reject_log "pinned input"
[ "$(locked_rev exta)" = "$good_a" ] || fail "the failed run did not restore the lock"
write_inputs_config

step "inputs: --only does not touch the lock of the whole flake"
good_a="$(locked_rev exta)"
export_ext "$exta" exta 6f broken
eppur_ build --update --no-commit --hold-local --only a || fail "the --only run gave up"
reject_log "pinned input"
expect_input_pins ""
[ -n "$(held_reason a)" ] || fail "a is not held by the --only run"
git -C "$sys" checkout -q -- flake.lock pins.json
export_ext "$exta" exta 6g ok
eppur_ build --update --no-commit || fail "the build after the --only run gave up"
git -C "$sys" add -A
git -C "$sys" commit -qm "after --only"

step "inputs: an input whose source changed in flake.nix is not sent back to the old one"
git -C "$exta" branch -q elsewhere
git -C "$exta" checkout -q elsewhere
export_ext "$exta" exta 6h broken
git -C "$exta" checkout -q main
cp "$sys/flake.nix" "$work/flake.nix.keep"
sed -i "s|git+file://$exta\"|git+file://$exta?ref=elsewhere\"|" "$sys/flake.nix"
eppur_ build --update --no-commit --hold-local || fail "the build on the new branch gave up"
reject_log "pinned input"
[ -n "$(held_reason a)" ] || fail "a is not held on the broken new branch"
git -C "$sys" checkout -q -- flake.nix flake.lock pins.json
eppur_ build --update --no-commit || fail "the build after the branch change gave up"
git -C "$sys" add -A
git -C "$sys" commit -qm "after the branch change" --allow-empty

step "inputs: targets that need different revisions end in a hold, not in a loop"
export_ext "$exta" exta 7a failb
eppur_ build --update --no-commit --hold-local --only a --only c || fail "building a on 7a gave up"
git -C "$sys" add -A
git -C "$sys" commit -qm "a built on 7a"
cp "$sys/flake.lock" "$work/7a.lock"
export_ext "$exta" exta 7a2 faila
eppur_ build --update --no-commit --hold-local --old-lock "$work/7a.lock" || fail "the rollback that breaks b gave up"
expect_log "pinned input exta at "
expect_log "rolling back flake inputs did not get every target building"
expect_input_pins ""
[ -n "$(held_reason a)" ] || fail "a is not held after the rollback was undone"
expect_not_held b
[ "$(locked_rev exta)" = "$(git -C "$exta" rev-parse HEAD)" ] || fail "exta was not put back on its new revision"
expect_built b exta-web-7a2
git -C "$sys" checkout -q -- flake.lock pins.json
write_inputs_config '"exta"'
export_ext "$exta" exta 7b faila
eppur_ build --update --no-commit --hold-local || fail "holding a on 7b gave up"
[ -n "$(held_reason a)" ] || fail "a is not held on 7b"
expect_not_held b
git -C "$sys" add -A
git -C "$sys" commit -qm "a held, b built on 7b"
write_inputs_config
export_ext "$exta" exta 7c broken
timeout 300 "$eppur" build --update --no-commit --hold-local --config "$config" >"$log" 2>&1 || fail "the conflicting targets did not end: rc $?"
expect_log "rolling back flake inputs did not get every target building"
expect_input_pins ""
[ -n "$(held_reason a)" ] || fail "a is not held after the conflict"
[ -n "$(held_reason b)" ] || fail "b is not held after the conflict"
expect_not_held c
git -C "$sys" reset -q --hard HEAD~2
export_ext "$exta" exta 7d ok
eppur_ build --update --no-commit || fail "the build after the conflict gave up"
expect_input_pins ""
git -C "$sys" add -A
git -C "$sys" commit -qm "after the conflict"

step "inputs: inputDeny holds the hosts instead"
write_inputs_config '"exta"'
good_a="$(locked_rev exta)"
export_ext "$exta" exta 7 broken
eppur_ build --update --no-commit --hold-local || fail "the denied rollback gave up"
reject_log "pinned input"
expect_input_pins ""
[ -n "$(held_reason a)" ] || fail "a is not held with exta denied"
[ -n "$(held_reason b)" ] || fail "b is not held with exta denied"
expect_not_held c
git -C "$sys" add -A
git -C "$sys" commit -qm "held, with the broken exta in the lock"

step "inputs: a held host rolls back from the lock it is held on, not the published one"
write_inputs_config
cp "$sys/flake.lock" "$work/published.lock"
eppur_ build --no-commit --hold-local --old-lock "$work/published.lock" || fail "rolling back for the held hosts gave up"
expect_log "pinned input exta at "
expect_log "released host a"
[ "$(locked_rev exta)" = "$good_a" ] || fail "exta did not go back to where a is held"
for h in a b c; do expect_not_held "$h"; done
git -C "$sys" reset -q --hard HEAD~1
export_ext "$exta" exta 8 ok
eppur_ build --update --no-commit || fail "the build on exta 8 gave up"
expect_input_pins ""
git -C "$sys" add -A
git -C "$sys" commit -qm "exta 8"

step "inputs: an input that would not fix the build is not rolled back"
export_ext "$exta" exta 9 needsok
eppur_ build --update --no-commit || fail "the build on exta 9 gave up"
expect_input_pins ""
git -C "$sys" add -A
git -C "$sys" commit -qm "exta 9"
export_ext "$exta" exta 10 needsok
revision fixed false 103 ok false other
eppur_ build --update --no-commit --hold-local || fail "the failure no rollback fixes gave up"
expect_log "testing whether exta at "
reject_log "pinned input"
expect_input_pins ""
[ -n "$(held_reason a)" ] || fail "a is not held when no rollback helps"
[ -n "$(held_reason b)" ] || fail "b is not held when no rollback helps"
expect_not_held c
[ "$(locked_rev exta)" = "$(git -C "$exta" rev-parse HEAD)" ] || fail "exta was not put back on its new revision"
git -C "$sys" checkout -q -- flake.lock pins.json
revision fixed false 104 ok
eppur_ build --update --no-commit || fail "the build after no rollback helped gave up"
expect_input_pins ""
git -C "$sys" add -A
git -C "$sys" commit -qm "toy is fine again"

step "inputs: the input a failure visibly involves is tested first"
first_tested() { grep -m1 "testing whether" "$log" | grep -qF "testing whether $1 at " || fail "the first input tested is not $1: $(grep "testing whether" "$log")"; }
export_ext "$exta" exta 20 ok
export_ext "$extb" extb 20 srcbroken
eppur_ build --update --no-commit --hold-local || fail "the broken extb source gave up"
first_tested extb
expect_log "pinned input extb at "
git -C "$sys" checkout -q -- flake.lock pins.json
export_ext "$exta" exta 21 ok
export_ext "$extb" extb 21 evalfail
eppur_ build --update --no-commit --hold-local || fail "the extb evaluation failure gave up"
first_tested extb
expect_log "pinned input extb at "
git -C "$sys" checkout -q -- flake.lock pins.json
export_ext "$exta" exta 22 ok
export_ext "$extb" extb 22 ok
eppur_ build --update --no-commit || fail "the build after extb was fixed gave up"
expect_input_pins ""
git -C "$sys" add -A
git -C "$sys" commit -qm "extb is fine again"

step "inputs: merge takes the later input pins, an emptied map too"
merge_file="$work/other-pins.json"
pinned_file() {
  cat >"$merge_file" <<EOF
{ "version": 2, "hosts": {}, "homes": {}, "inputs": { "exta": {
    "locked": { "narHash": "sha256-x", "rev": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }, "failedNarHash": "sha256-y",
    "since": "2026-11-01", "reason": "build", "targets": ["host a"] } }, "inputsChanged": $1 }
EOF
}
empty_file() {
  printf '{ "version": 2, "hosts": {}, "homes": {}, "inputsChanged": %s }\n' "$1" >"$merge_file"
}
pinned_file 99999999999
eppur_ merge "$merge_file" || fail "merging input pins failed"
expect_input_pins exta
empty_file 99999999998
eppur_ merge "$merge_file" || fail "merging older input pins failed"
expect_input_pins exta
empty_file 100000000000
eppur_ merge "$merge_file" || fail "merging a later emptied map failed"
expect_input_pins ""
pinned_file 100000000001
eppur_ merge "$merge_file" || fail "merging input pins again failed"
expect_input_pins exta
eppur_ clear --flake-input exta || fail "clearing the input pin failed"
expect_input_pins ""
expect_log "unpinned input exta"
if eppur_ clear --flake-input exta; then fail "clearing an input that is not pinned succeeded"; fi
git -C "$sys" checkout -q -- pins.json

printf '\nall e2e steps passed\n'
