self:
{
  config,
  lib,
  pkgs,
  options,
  ...
}:
let
  cfg = config.programs.eppur;
  nhFlake = config.programs.nh.flake or null;

  marked = import ./marked.nix;
  overlay =
    input: nixpkgsConfig:
    marked.wrap self.lib.mkOverlay cfg.pinnedOverlays {
      pins = cfg.pinsFile;
      kind = "hosts";
      inherit (cfg) name;
      inherit input nixpkgsConfig;
    };

  platformArgs = import ./platform.nix lib pkgs.stdenv.buildPlatform pkgs.stdenv.hostPlatform;

  extraSets = lib.mapAttrs (
    input: set:
    import set.source (
      platformArgs
      // {
        config = lib.filterAttrs (_: v: v != null) set.config;
        overlays = lib.optional cfg.enable (overlay input set.config);
      }
    )
  ) cfg.nixpkgs;

  runsEppur = cfg.hosts != [ ] || cfg.homes != [ ];
  reservedArgs = [
    "pkgs"
    "lib"
    "config"
    "options"
    "modulesPath"
    "specialArgs"
  ];

  up = cfg.autoUpgrade;
  readOnlyPkgs = !(options.nixpkgs ? localSystem);
  upCommit = if up.commit == null then cfg.commit else up.commit;
  builtLink = "/run/eppur/system";
  activateScript = pkgs.writeShellScript "eppur-activate" ''
    set -eu
    link="''${EPPUR_BUILT_LINK:-${builtLink}}"
    [ -L "$link" ] || exit 0
    system="$(${pkgs.coreutils}/bin/readlink -f "$link")"
    case "$system" in
      ${builtins.storeDir}/*) ;;
      *) echo "refusing to activate $system: not in ${builtins.storeDir}" >&2; exit 1 ;;
    esac
    if [ ! -x "$system/bin/switch-to-configuration" ]; then
      echo "refusing to activate $system: it is not a NixOS system" >&2
      exit 1
    fi
    if [ "${up.operation}" = switch ] && [ "$system" = "$(${pkgs.coreutils}/bin/readlink -f /run/current-system)" ]; then
      echo "already running $system"
      exit 0
    fi
    ${config.nix.package}/bin/nix-env -p /nix/var/nix/profiles/system --set "$system"
    exec "$system/bin/switch-to-configuration" ${up.operation}
  '';
in
{
  imports = [
    (lib.mkRemovedOptionModule [ "programs" "nixheal" ] "nixheal was renamed: use the eppur flake (github:Sejder/eppur) and programs.eppur.")
  ];

  options.programs.eppur =
    lib.recursiveUpdate
      (import ./options.nix {
        inherit lib self pkgs;
        defaultName = config.networking.hostName;
        defaultEnable = false;
        defaultPinsFile = null;
        defaultFlake = if nhFlake == null then null else toString nhFlake;
        defaultSetConfig = config.nixpkgs.config;
      })
      {
        autoUpgrade = {
          operation = lib.mkOption {
            type = lib.types.enum [
              "switch"
              "boot"
            ];
            default = "switch";
            description = "Activate the new system right away, or on the next boot.";
          };
          user = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            example = "alice";
            description = "Who updates, builds and commits: the owner of the flake checkout, so its files and git objects stay theirs. Activation always runs as root.";
          };
        };
      };

  config = lib.mkMerge [
    (lib.mkIf (up.enable && !cfg.enable) {
      assertions = [
        {
          assertion = false;
          message = "programs.eppur.autoUpgrade needs programs.eppur.enable";
        }
      ];
    })
    {
      programs.eppur.importPinned =
        { locked, overlays }:
        import ./import-pinned.nix {
          config = if options.nixpkgs.pkgs.isDefined then pkgs.config else config.nixpkgs.config;
          inherit lib;
          inherit (pkgs.stdenv) buildPlatform hostPlatform;
        } {
          inherit locked;
          overlays = if overlays then marked.before (pkgs.overlays or [ ]) else [ ];
        };
      programs.eppur.packageSets = {
        ${cfg.input} = pkgs;
      }
      // extraSets;
      _module.args = lib.mapAttrs' (input: set: lib.nameValuePair set.argName extraSets.${input}) (
        lib.filterAttrs (
          _: set: set.argName != null && !(builtins.elem set.argName reservedArgs)
        ) cfg.nixpkgs
      );
    }
    (lib.mkIf cfg.enable (
      {
        assertions = [
          {
            assertion = cfg.pinsFile != null;
            message = "programs.eppur.pinsFile must point at the flake's pins.json";
          }
          {
            assertion =
              cfg.pinsFile == null
              || builtins.isPath cfg.pinsFile
              || lib.hasPrefix "${builtins.storeDir}/" (toString cfg.pinsFile);
            message = "programs.eppur.pinsFile must be a path such as ./pins.json, not a string";
          }
        ]
        ++ lib.mapAttrsToList (input: set: {
          assertion = set.argName == null || !(builtins.elem set.argName reservedArgs);
          message = "programs.eppur.nixpkgs.${input}.argName = \"${toString set.argName}\" would shadow a module argument";
        }) cfg.nixpkgs;
        warnings =
          lib.optional (readOnlyPkgs)
            "programs.eppur cannot add its overlay to a read-only nixpkgs (nixosModules.readOnlyPkgs); add eppur.lib.mkOverlay to the pkgs you pass in, or its pins do not apply";
      }
      // lib.optionalAttrs (!readOnlyPkgs) {
        nixpkgs.overlays = lib.mkAfter [
          (overlay cfg.input (if options.nixpkgs.pkgs.isDefined then null else config.nixpkgs.config))
        ];
      }
    ))
    (lib.mkIf (cfg.enable && runsEppur) {
      environment.systemPackages = [
        cfg.package
      ]
      ++ lib.optional (cfg.switcher == "nh" && !(config.programs.nh.enable or false)) pkgs.nh;
      environment.etc."eppur.json".text = builtins.toJSON {
        kind = "nixos";
        self = cfg.name;
        inherit (cfg)
          flake
          hosts
          homes
          warnAfterDays
          deny
          quiet
          commit
          switcher
          input
          donors
          inputDeny
          retrySameDerivationAfterDays
          ;
        packageSets = builtins.attrNames cfg.nixpkgs;
      };
    })
    (lib.mkIf (cfg.enable && up.enable) {
      assertions = [
        {
          assertion = builtins.elem cfg.name cfg.hosts;
          message = "programs.eppur.autoUpgrade needs this host (${cfg.name}) in programs.eppur.hosts";
        }
        {
          assertion = cfg.flake != null;
          message = "programs.eppur.autoUpgrade needs programs.eppur.flake";
        }
        {
          assertion = up.user != null;
          message = "programs.eppur.autoUpgrade.user must be set to the owner of ${toString cfg.flake}";
        }
      ];
      warnings = lib.optional config.system.autoUpgrade.enable "programs.eppur.autoUpgrade and system.autoUpgrade both update this host; disable one";

      systemd.services.eppur-upgrade = {
        description = "Update the flake with eppur and activate the result";
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
        restartIfChanged = false;
        stopIfChanged = false;
        path = [
          cfg.package
          config.nix.package
          pkgs.git
          pkgs.coreutils
          config.programs.ssh.package
        ];
        environment = {
          EPPUR_CONFIG = "/etc/eppur.json";
        }
        // lib.optionalAttrs (up.user == "root") {
          GIT_AUTHOR_NAME = "eppur";
          GIT_AUTHOR_EMAIL = "eppur@${config.networking.hostName}";
          GIT_COMMITTER_NAME = "eppur";
          GIT_COMMITTER_EMAIL = "eppur@${config.networking.hostName}";
        };
        unitConfig = {
          OnFailure = up.onFailure;
          X-StopOnRemoval = false;
        };
        serviceConfig = {
          Type = "oneshot";
          User = up.user;
          RuntimeDirectory = "eppur";
          ExecStart = lib.escapeShellArgs (
            [
              (lib.getExe cfg.package)
              "build"
              "--built-link"
              builtLink
            ]
            ++ [
              (if upCommit then "--commit" else "--no-commit")
              "--timeout"
              up.timeout
            ]
            ++ [ "--update" ]
            ++ up.inputs
          );
          ExecStartPost = "+${activateScript}";
        };
      };

      systemd.timers.eppur-upgrade = {
        description = "Scheduled eppur upgrade";
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnCalendar = up.dates;
          RandomizedDelaySec = up.randomizedDelaySec;
          Persistent = true;
        };
      };
    })
  ];
}
