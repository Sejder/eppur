{
  lib,
  self,
  pkgs,
  defaultName,
  defaultEnable,
  defaultPinsFile,
  defaultFlake,
  defaultSetConfig,
  defaultInput ? "nixpkgs",
}:
let
  inherit (lib) mkOption types;
  nixpkgsSet = types.submodule {
    options = {
      source = mkOption {
        type = types.raw;
        description = "A nixpkgs source, usually a flake input.";
      };
      config = mkOption {
        type = types.attrs;
        default = defaultSetConfig;
        description = "The nixpkgs config to import it with.";
      };
      argName = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Also pass the imported set to modules under this argument name.";
      };
    };
  };
in
{
  enable = mkOption {
    type = types.bool;
    default = defaultEnable;
    description = "Apply this machine's eppur pins, and install eppur when it builds hosts or homes.";
  };
  package = mkOption {
    type = types.package;
    default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
    defaultText = lib.literalExpression "eppur.packages.\${system}.default";
    description = "The eppur package.";
  };
  input = mkOption {
    type = types.str;
    default = defaultInput;
    example = "nixos";
    description = "The flake input this machine's pkgs come from, as named in flake.lock; pins for pkgs are kept under it.";
  };
  pinsFile = mkOption {
    type = types.nullOr types.path;
    default = defaultPinsFile;
    example = lib.literalExpression "./pins.json";
    description = "The flake's pins.json. It may not exist yet; eppur creates it.";
  };
  name = mkOption {
    type = types.str;
    default = defaultName;
    description = "The entry in pins.json that holds this machine's pins.";
  };
  flake = mkOption {
    type = types.nullOr types.str;
    default = defaultFlake;
    description = "Path of the flake checkout that eppur updates.";
  };
  hosts = mkOption {
    type = types.listOf types.str;
    default = [ ];
    description = "nixosConfigurations that eppur builds and pins when it runs here.";
  };
  homes = mkOption {
    type = types.listOf types.str;
    default = [ ];
    description = "homeConfigurations that eppur builds and pins when it runs here.";
  };
  warnAfterDays = mkOption {
    type = types.ints.unsigned;
    default = 14;
    description = "Log a pin at warning level once it is older than this.";
  };
  deny = mkOption {
    type = types.listOf types.str;
    default = [ ];
    example = [ "cuda*" ];
    description = "Attributes never to pin, on top of the built-in list. A trailing * matches a prefix.";
  };
  donors = mkOption {
    type = types.listOf types.str;
    default = [ ];
    example = [ "nixpkgs-stable" ];
    description = "Flake inputs, as named in flake.lock, that a failed package may be pinned to instead of the revision it last built with, when one has a newer version of it that is already built or in a substituter. Only for packages nothing else in the system depends on.";
  };
  inputDeny = mkOption {
    type = types.listOf types.str;
    default = [ ];
    example = [ "nix-secrets" ];
    description = "Flake inputs never to roll back in flake.lock when they break a build, such as one holding secrets that were rotated. A trailing * matches a prefix, so [ \"*\" ] turns rolling back off.";
  };
  retrySameDerivationAfterDays = mkOption {
    type = types.ints.unsigned;
    default = 7;
    description = "When an input moves but a pinned package's derivation is still the one that failed, retry it only once it was last tried this many days ago; 0 retries it whenever its input moves.";
  };
  pinInsecure = mkOption {
    type = types.bool;
    default = true;
    description = "When nixpkgs starts refusing a package as insecure, pin it to the copy this machine already built, so the rest of the host still updates; the pin is marked insecure in status and warned about on every run. false holds the host instead, as nixpkgs' refusal intends, until the package is fixed, removed, or allowed with nixpkgs.config.permittedInsecurePackages.";
  };
  switcher = mkOption {
    type = types.enum [
      "nh"
      "native"
    ];
    default = "nh";
    description = "How eppur switch and boot activate: nh, or native (nixos-rebuild for hosts, home-manager for homes).";
  };
  autoUpgrade = {
    enable = lib.mkEnableOption "a timer that updates the flake with eppur and activates the result";
    dates = mkOption {
      type = types.str;
      default = "04:40";
      example = "daily";
      description = "When to run, as a systemd calendar expression.";
    };
    randomizedDelaySec = mkOption {
      type = types.str;
      default = "45min";
      description = "Random delay added to each run.";
    };
    timeout = mkOption {
      type = types.str;
      default = "12h";
      description = "Stop updating, evaluating and building after this long, e.g. one stuck on a hung download, as with --timeout (e.g. 12h, 45min, 90s); activation is never cut short.";
    };
    inputs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "nixpkgs" ];
      description = "Flake inputs to update; all of them when empty.";
    };
    commit = mkOption {
      type = types.nullOr types.bool;
      default = null;
      description = "Commit flake.lock and pins.json after each timer run; null follows programs.eppur.commit.";
    };
    onFailure = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "notify-admin@%n.service" ];
      description = "Units to start when a run fails, e.g. to send a notification.";
    };
  };
  quiet = mkOption {
    type = types.bool;
    default = false;
    description = "Only report pins, unpins, holds, warnings and errors, as with --quiet.";
  };
  pinnedOverlays = mkOption {
    type = types.bool;
    default = true;
    description = "Apply this machine's own nixpkgs overlays (those ordered before eppur's) to pinned packages, so a pinned package keeps your patches. A pin is made without them when they do not evaluate against the old nixpkgs, with a warning.";
  };
  importPinned = mkOption {
    type = types.raw;
    readOnly = true;
    internal = true;
    description = "How this machine imports a pinned nixpkgs, for eppur's checks: { locked, overlays }: pkgs, with overlays a bool.";
  };
  commit = mkOption {
    type = types.bool;
    default = false;
    description = "Commit flake.lock and pins.json after each build, switch or boot, as with --commit.";
  };
  nixpkgs = mkOption {
    type = types.attrsOf nixpkgsSet;
    default = { };
    example = lib.literalExpression ''
      {
        nixpkgs-unstable = {
          source = inputs.nixpkgs-unstable;
          argName = "unstable-pkgs";
        };
      }
    '';
    description = "Further nixpkgs to import with pins applied, keyed by their flake input name.";
  };
  packageSets = mkOption {
    type = types.attrsOf types.raw;
    readOnly = true;
    description = "Every package set eppur pins on this machine, keyed by flake input.";
  };
}
