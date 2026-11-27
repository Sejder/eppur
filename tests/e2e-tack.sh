#!/usr/bin/env bash
# End-to-end run of eppur on a flake pinned with tack: a toy nixpkgs whose
# history breaks and fixes a package, and a toy input, tool, that breaks and
# is held back by freezing it in pins.toml. Needs a working nix with flakes and
# tack (TACK names the binary, by default tack on PATH); run from anywhere:
#
#   cargo build && tests/e2e-tack.sh
#
# EPPUR overrides the binary under test.

set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
eppur="${EPPUR:-$repo/target/debug/eppur}"
nix() { command nix --extra-experimental-features "nix-command flakes" "$@"; }
system="$(nix eval --raw --impure --expr builtins.currentSystem)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
toy="$work/toypkgs"
tool="$work/tool"
sys="$work/system"
log="$work/log"
config="$work/eppur.json"
salt="$(date +%s%N)"

export XDG_STATE_HOME="$work/state"
export GIT_AUTHOR_NAME=e2e GIT_AUTHOR_EMAIL=e2e@localhost
export GIT_COMMITTER_NAME=e2e GIT_COMMITTER_EMAIL=e2e@localhost
mkdir -p "$work/bin"
tack_bin="$(command -v "${TACK:-tack}" || true)"
[ -n "$tack_bin" ] || { echo "FAIL: tack is not on PATH; set TACK" >&2; exit 1; }
ln -s "$tack_bin" "$work/bin/tack"
printf '#!/bin/sh\necho "nh $*" >> %s\n' "$work/nh.calls" >"$work/bin/nh"
chmod +x "$work/bin/nh"
export PATH="$work/bin:$PATH"
tack() { (cd "$sys" && command tack "$@"); }

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
expect_pins() {
  local got
  got="$(pins_of "$1")"
  [ "$got" = "$2" ] || fail "expected pins on $1 [$2], got [$got]"
  echo "pins on $1: [$got]"
}
input_field() { jq_pins "toString (f.inputs.$1.$2 or \"\")"; }
input_pins() { jq_pins "builtins.concatStringsSep \" \" (builtins.attrNames (f.inputs or { }))"; }
expect_input_pins() {
  local got
  got="$(input_pins)"
  [ "$got" = "$1" ] || fail "expected input pins [$1], got [$got]"
  echo "input pins: [$got]"
}
locked_rev() { nix eval --raw --impure --expr "(builtins.fromJSON (builtins.readFile $sys/.tack/pins.lock.json)).$1.rev"; }
frozen() {
  nix eval --raw --impure --expr "let t = builtins.fromTOML (builtins.readFile $sys/.tack/pins.toml); p = t.inputs.$1; in toString (p.frozen or \"\")"
}
expect_log() { grep -qF -- "$1" "$log" || fail "log lacks: $1"; }
reject_log() { if grep -qF -- "$1" "$log"; then fail "log unexpectedly has: $1"; fi; }
expect_built() {
  local out
  out="$(nix build --no-link --print-out-paths "$sys#nixosConfigurations.$1.config.system.build.toplevel")"
  grep -qF -- "$2" "$out" || fail "$1 does not contain $2: $(head -c 300 "$out")"
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
  printf '{ libfoo = "%s"; hello = "%s"; salt = "%s"; }\n' "$1" "$2" "$salt" >"$toy/state.nix"
  git -C "$toy" add -A
  git -C "$toy" commit -qm "libfoo=$1 hello=$2"
}

export_tool() {
  local body="echo tool-$1 > \$out"
  [ "$2" = broken ] && body="echo tool does not compile >&2; exit 1"
  printf '{ system }:\nderivation { name = "tool-%s"; inherit system; salt = "%s"; builder = "/bin/sh"; args = [ "-c" "%s" ]; }\n' \
    "$1" "$salt" "$body" >"$tool/default.nix"
  git -C "$tool" add -A
  git -C "$tool" commit -qm "tool $1 $2"
}

[ -x "$eppur" ] || fail "$eppur is not built; run cargo build first"

mkdir -p "$toy" "$tool" "$sys"
git -C "$toy" init -q -b main
cat >"$toy/default.nix" <<'EOF'
{ localSystem ? { inherit system; }, system ? localSystem.system, crossSystem ? null, config ? { }, overlays ? [ ] }:
let
  state = import ./state.nix;
  sys = localSystem.system;
  mk = name: pname: deps: script: derivation {
    inherit name pname deps;
    system = sys;
    inherit (state) salt;
    builder = "/bin/sh";
    args = [ "-c" script ];
  };
  base = self: {
    inherit config overlays;
    stdenv.hostPlatform.system = sys;
    stdenv.buildPlatform.system = sys;
    libfoo = mk "libfoo-1.0" "libfoo" [ ] (
      if state.libfoo == "broken" then "echo libfoo does not compile >&2; exit 1" else "echo libfoo-${state.libfoo} > $out"
    );
    app = mk "app-2.0" "app" [ self.libfoo ] "echo app $deps > $out";
    hello = mk "hello-${state.hello}" "hello" [ ] "echo hello > $out";
  };
  extend = f: o: self: let prev = f self; in prev // o self prev;
  fix = f: let x = f x; in x;
in
fix (builtins.foldl' extend base overlays)
EOF
revision ok 1
git -C "$tool" init -q -b main
export_tool 1 ok

cat >"$config" <<EOF
{ "kind": "nixos", "self": "a", "flake": "$sys", "hosts": ["a", "b"], "homes": [],
  "input": "toypkgs", "commit": true, "deny": [] }
EOF

git -C "$sys" init -q -b main
tack init --flake >/dev/null
tack add --fetch toypkgs "git+file://$toy" >/dev/null
tack add --fetch tool "git+file://$tool" >/dev/null
sed -i 's/^\[inputs.tool\]$/[inputs.tool]\ngroup = "extras"/' "$sys/.tack/pins.toml"
grep -q 'group = "extras"' "$sys/.tack/pins.toml" || fail "could not put tool in a group"
cp "$repo/nix/overlay.nix" "$repo/nix/platform.nix" "$repo/nix/import-pinned.nix" "$sys/"
cat >"$sys/flake.nix" <<EOF
{
  outputs =
    { self, ... }@args:
    let
      inputs = (import ./.tack) { overrides = args.tackOverrides or { }; };
      system = "$system";
      mk = name: paths:
        let
          pkgs = import inputs.toypkgs {
            inherit system;
            overlays = [ (import ./overlay.nix { pins = ./pins.json; kind = "hosts"; inherit name; input = "toypkgs"; }) ];
          };
          tool = import inputs.tool { inherit system; };
        in {
          config.programs.eppur = {
            enable = true;
            inherit name;
            pinsFile = ./pins.json;
            packageSets.toypkgs = pkgs;
          };
          config.system.build.toplevel = derivation {
            name = "toplevel-\${name}";
            inherit system;
            paths = paths pkgs tool;
            builder = "/bin/sh";
            args = [ "-c" "echo \$paths > \$out" ];
          };
        };
    in {
      nixosConfigurations.a = mk "a" (p: t: [ p.hello t ]);
      nixosConfigurations.b = mk "b" (p: t: [ p.app p.hello ]);
    };
}
EOF
git -C "$sys" add -A
git -C "$sys" commit -qm system

step "t1: everything builds; the run says it is a tack flake and commits pins.json"
eppur_ build --update || fail "the first build gave up"
expect_log "pinned with tack"
git -C "$sys" diff --quiet HEAD -- pins.json || fail "pins.json is not committed"
expect_pins a ""
expect_pins b ""

step "t2: libfoo breaks; b pins app at the toypkgs revision tack locked"
good_toy="$(locked_rev toypkgs)"
revision broken 2
eppur_ build --update || fail "pinning app gave up"
expect_pins b "toypkgs.app"
[ "$(jq_pins 'f.hosts.b.pins.toypkgs.app.locked.rev')" = "$good_toy" ] || fail "app is not pinned at $good_toy"
[ "$(jq_pins 'toString (f.hosts.b.pins.toypkgs.app.locked ? public)')" = "" ] || fail "the pin carries tack's own keys"
[ "$(locked_rev toypkgs)" = "$(git -C "$toy" rev-parse HEAD)" ] || fail "toypkgs did not update"
git -C "$sys" log -1 --format=%s | grep -qF "eppur: update tack pins" || fail "commit title: $(git -C "$sys" log -1 --format=%s)"
git -C "$sys" diff --quiet HEAD -- .tack pins.json || fail "the tack lock is not committed"
expect_built b hello-2

step "t3: libfoo is fixed; app is unpinned"
revision fixed 3
eppur_ build --update || fail "unpinning app gave up"
expect_log "unpinned host b: app (toypkgs): builds again"
expect_pins b ""

step "t4: tool breaks; it goes back and is frozen there in pins.toml"
good_tool="$(locked_rev tool)"
export_tool 2 broken
eppur_ build --update || fail "rolling back tool gave up"
expect_log "pinned input tool at "
expect_log "froze tack pin tool at ${good_tool:0:12}"
expect_input_pins tool
[ "$(locked_rev tool)" = "$good_tool" ] || fail "tool did not go back to $good_tool"
[ "$(frozen tool)" = "$good_tool" ] || fail "pins.toml has tool frozen at [$(frozen tool)]"
[ "$(input_field tool tackFrozen)" = "$good_tool" ] || fail "pins.json does not record the freeze"
[ "$(locked_rev toypkgs)" = "$(git -C "$toy" rev-parse HEAD)" ] || fail "toypkgs was held back with tool"
git -C "$sys" diff --quiet HEAD -- .tack pins.json || fail "the freeze is not committed"

step "t5: a plain tack update by hand leaves the held pin where it is"
tack update >/dev/null
[ "$(locked_rev tool)" = "$good_tool" ] || fail "tack update moved the frozen tool"
git -C "$sys" checkout -q -- .tack

step "t6: while tool stays broken, an update with its group keeps it back"
lock_blob="$(git -C "$sys" rev-parse HEAD:.tack/pins.lock.json)"
eppur_ build --update extras || fail "updating the extras group gave up"
[ "$(git -C "$sys" rev-parse HEAD:.tack/pins.lock.json)" = "$lock_blob" ] || fail "putting tool back did not give the same lock: $(git -C "$sys" diff HEAD~1 -- .tack/pins.lock.json)"
git -C "$sys" diff --quiet -- .tack/pins.lock.json || fail "the lock is left changed"
reject_log "froze tack pin"
[ "$(locked_rev tool)" = "$good_tool" ] || fail "tool moved to the broken revision"
[ "$(frozen tool)" = "$good_tool" ] || fail "tool is not frozen again at $good_tool"
reject_log "pinned input"
eppur_ build --update nosuch && fail "an unknown --update name was taken"
expect_log "--update nosuch: not a pin or group of .tack/pins.toml (its pins and groups: extras, tool, toypkgs)"

step "t7: a pin frozen by hand stays frozen, and eppur does not claim it"
tack freeze toypkgs >/dev/null
git -C "$sys" commit -qam "freeze toypkgs by hand"
frozen_toy="$(locked_rev toypkgs)"
revision fixed 7
eppur_ build --update || fail "the build with a hand-frozen pin gave up"
[ "$(locked_rev toypkgs)" = "$frozen_toy" ] || fail "the hand-frozen toypkgs moved"
[ "$(frozen toypkgs)" = "$frozen_toy" ] || fail "the hand freeze of toypkgs changed"
reject_log "tack pin toypkgs"
tack unfreeze toypkgs >/dev/null
git -C "$sys" commit -qam "unfreeze toypkgs"

step "t8: a dry run puts .tack back as it was"
export_tool 3 ok
cp -r "$sys/.tack" "$work/tack.before"
eppur_ build --update --dry-run || fail "the dry run gave up"
diff -r -x patched "$work/tack.before" "$sys/.tack" || fail "the dry run left .tack changed"
[ "$(frozen tool)" = "$good_tool" ] || fail "the dry run unfroze tool"

step "t9: a failed run puts .tack back too"
EPPUR_TACK=/nonexistent/tack eppur_ build --update && fail "the run without tack succeeded"
expect_log "tack is not installed"
diff -r -x patched "$work/tack.before" "$sys/.tack" || fail "the failed run left .tack changed"

step "t10: tool is fixed upstream; it is unfrozen and its pin goes"
eppur_ build --update || fail "releasing tool gave up"
expect_log "unpinned input tool: builds again"
expect_input_pins ""
[ -z "$(frozen tool)" ] || fail "tool is still frozen at $(frozen tool)"
[ "$(locked_rev tool)" = "$(git -C "$tool" rev-parse HEAD)" ] || fail "tool is not at its fixed revision"
expect_built a tool-3
git -C "$sys" diff --quiet HEAD -- .tack pins.json || fail "the release is not committed"

step "t11: clear --flake-input unfreezes the pin eppur froze"
export_tool 4 broken
eppur_ build --update || fail "rolling back tool again gave up"
[ -n "$(frozen tool)" ] || fail "tool was not frozen"
eppur_ clear --flake-input tool || fail "clear --flake-input failed"
expect_log "unfroze tack pin tool"
[ -z "$(frozen tool)" ] || fail "clear left tool frozen"
expect_input_pins ""
git -C "$sys" checkout -q -- .tack pins.json

step "t12: status shows the held pin, and --look what tack look sees upstream"
eppur_ status --look || fail "status --look failed"
expect_log "input tool"
expect_log "as tack look sees it"
expect_log "tool"
eppur_ why tool || fail "why tool failed"
expect_log "tack pin tool is held back in pins.lock.json"

step "t13: a pin naming signers stops an unsigned update before anything builds"
cp "$sys/.tack/pins.toml" "$work/pins.toml.unsigned"
ssh-keygen -q -t ed25519 -N '' -f "$work/key" -C e2e
printf '\n[signers]\nme = "%s"\n' "$(cat "$work/key.pub")" >"$work/signers.toml"
sed -i '/^\[signers\]$/d' "$sys/.tack/pins.toml"
cat "$work/signers.toml" >>"$sys/.tack/pins.toml"
sed -i 's/^\[inputs.toypkgs\]$/[inputs.toypkgs]\nsigners = ["me"]/' "$sys/.tack/pins.toml"
git -C "$sys" commit -qam "toypkgs must be signed"
revision fixed 13
eppur_ build --update && fail "the unsigned toypkgs was built"
expect_log "toypkgs: FAILED: commit"
expect_log "is not signed"
reject_log "round 1"
git -C "$sys" diff --quiet HEAD -- .tack || fail "the refused update left .tack changed"

step "t14: a lock changed by hand is checked against the signers before a build"
python3 -I - "$sys/.tack/pins.lock.json" "$(git -C "$toy" rev-parse HEAD)" <<'PY'
import json, sys
path, rev = sys.argv[1], sys.argv[2]
lock = json.load(open(path))
lock["toypkgs"]["rev"] = rev
json.dump(lock, open(path, "w"), indent=2)
PY
eppur_ build && fail "a build went ahead with an unsigned commit in the lock"
expect_log "did not sign"
reject_log "round 1"
git -C "$sys" checkout -q -- .tack
eppur_ build || fail "the build of the committed, signed-off lock gave up"

step "t15: --locker flake on a tack flake is refused"
eppur_ --locker flake build && fail "a tack flake was built as a flake.lock flake"
expect_log "no flake.lock in"

step "t16: a new revision of a held pin waits unfrozen for a build to judge it"
eppur_ clear --flake-input tool || fail "clear --flake-input failed"
sed -i '/^signers = \["me"\]$/d' "$sys/.tack/pins.toml"
git -C "$sys" commit -qam "let tool go, toypkgs need not be signed"
export_tool 5 broken
eppur_ build --update tool || fail "rolling back tool 5 gave up"
[ "$(frozen tool)" = "$(input_field tool tackFrozen)" ] || fail "tool is not frozen where pins.json says"
export_tool 6 broken
eppur_ update || fail "eppur update failed"
[ "$(input_field tool retrying)" = 1 ] || fail "the new tool is not marked as retrying"
[ -z "$(frozen tool)" ] || fail "tool is frozen at a revision no build has tried: $(frozen tool)"
eppur_ build || fail "judging tool 6 gave up"
[ -n "$(frozen tool)" ] || fail "tool was not frozen again once tool 6 failed"
expect_input_pins tool

step "t17: a pin frozen by hand at a broken revision is not rolled back"
eppur_ clear --flake-input tool || fail "clear --flake-input failed"
git -C "$sys" commit -qam "let tool go again"
tack update tool >/dev/null
tack freeze tool >/dev/null
git -C "$sys" commit -qam "tool frozen by hand at a broken revision"
broken_tool="$(locked_rev tool)"
eppur_ build && fail "this machine's host built with a broken tool"
reject_log "pinned input tool"
expect_log "giving up on host a: tool-6"
expect_log "tack pin tool is frozen by hand in pins.toml, so it is not rolled back"
[ "$(locked_rev tool)" = "$broken_tool" ] || fail "the hand-frozen tool was moved"
[ "$(frozen tool)" = "$broken_tool" ] || fail "the hand freeze of tool changed"

echo
echo "all tack end-to-end steps passed"
