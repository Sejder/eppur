self:
{
  config,
  lib,
  pkgs,
  osConfig ? null,
  ...
}:
let
  cfg = config.programs.eppur;
  inNixos = osConfig != null;
  osCfg = if inNixos then osConfig.programs.eppur or { } else { };
  globalPkgs = inNixos && (osConfig.home-manager.useGlobalPkgs or false);
  nhFlake = config.programs.nh.flake or null;

  marked = import ./marked.nix;
  overlay =
    input: nixpkgsConfig:
    marked.wrap self.lib.mkOverlay cfg.pinnedOverlays {
      pins = cfg.pinsFile;
      kind = if inNixos then "hosts" else "homes";
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

  runsEppur = !inNixos && (cfg.hosts != [ ] || cfg.homes != [ ]);
  upCommit = if cfg.autoUpgrade.commit == null then cfg.commit else cfg.autoUpgrade.commit;
  hmNixpkgsConfig =
    let
      c = config.nixpkgs.config or null;
    in
    if c == null then { } else c;
  reservedArgs = [
    "pkgs"
    "lib"
    "config"
    "options"
    "osConfig"
    "specialArgs"
  ];
in
{
  imports = [
    (lib.mkRemovedOptionModule [ "programs" "nixheal" ] "nixheal was renamed: use the eppur flake (github:Sejder/eppur) and programs.eppur.")
  ];

  options.programs.eppur = import ./options.nix {
    inherit lib self pkgs;
    defaultName = if inNixos then osCfg.name or osConfig.networking.hostName else config.home.username;
    defaultEnable = osCfg.enable or false;
    defaultInput = osCfg.input or "nixpkgs";
    defaultPinsFile = osCfg.pinsFile or null;
    defaultFlake = if nhFlake == null then null else toString nhFlake;
    defaultSetConfig = hmNixpkgsConfig;
  };

  config = lib.mkMerge [
    (lib.mkIf (cfg.autoUpgrade.enable && !cfg.enable) {
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
          config =
            if (config.nixpkgs.config or null) == null then pkgs.config else config.nixpkgs.config;
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
      warnings = lib.optional (
        inNixos && (cfg.hosts != [ ] || cfg.homes != [ ] || cfg.autoUpgrade.enable)
      ) "programs.eppur.hosts, homes and autoUpgrade in Home Manager only apply to a standalone Home Manager; inside NixOS, set them in the NixOS configuration";
      _module.args = lib.mapAttrs' (input: set: lib.nameValuePair set.argName extraSets.${input}) (
        lib.filterAttrs (
          _: set: set.argName != null && !(builtins.elem set.argName reservedArgs)
        ) cfg.nixpkgs
      );
    }
    (lib.mkIf (cfg.enable && !globalPkgs) {
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
      nixpkgs.overlays = lib.mkAfter [ (overlay cfg.input (config.nixpkgs.config or null)) ];
    })
    (lib.mkIf (cfg.enable && runsEppur) {
      home.packages = [
        cfg.package
      ]
      ++ lib.optional (cfg.switcher == "nh" && !(config.programs.nh.enable or false)) pkgs.nh;
      xdg.configFile."eppur.json".text = builtins.toJSON {
        kind = "home";
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
          pinInsecure
          nixArgs
          evalJobs
          ;
        packageSets = builtins.attrNames cfg.nixpkgs;
      };
    })
    (lib.mkIf (cfg.enable && cfg.autoUpgrade.enable && !inNixos && !runsEppur) {
      assertions = [
        {
          assertion = false;
          message = "programs.eppur.autoUpgrade needs this home (${cfg.name}) in programs.eppur.homes";
        }
      ];
    })
    (lib.mkIf (cfg.enable && cfg.autoUpgrade.enable && runsEppur) {
      assertions = [
        {
          assertion = builtins.elem cfg.name cfg.homes;
          message = "programs.eppur.autoUpgrade needs this home (${cfg.name}) in programs.eppur.homes";
        }
        {
          assertion = cfg.flake != null;
          message = "programs.eppur.autoUpgrade needs programs.eppur.flake";
        }
      ];

      systemd.user.services.eppur-upgrade = {
        Unit = {
          Description = "Update the flake with eppur and switch Home Manager";
          OnFailure = cfg.autoUpgrade.onFailure;
          X-SwitchMethod = "keep-old";
        };
        Service = {
          Type = "oneshot";
          Environment = [
            "PATH=${config.home.profileDirectory}/bin:${
              lib.makeBinPath [
                pkgs.git
                pkgs.openssh
              ]
            }:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/bin:/bin"
            "EPPUR_CONFIG=${config.xdg.configHome or "${config.home.homeDirectory}/.config"}/eppur.json"
          ];
          ExecStart = lib.escapeShellArgs (
            [
              (lib.getExe cfg.package)
              "switch"
            ]
            ++ [
              (if upCommit then "--commit" else "--no-commit")
              "--timeout"
              cfg.autoUpgrade.timeout
            ]
            ++ [ "--update" ]
            ++ cfg.autoUpgrade.inputs
          );
        };
      };

      systemd.user.timers.eppur-upgrade = {
        Unit.Description = "Scheduled eppur upgrade";
        Install.WantedBy = [ "timers.target" ];
        Timer = {
          OnCalendar = cfg.autoUpgrade.dates;
          RandomizedDelaySec = cfg.autoUpgrade.randomizedDelaySec;
          Persistent = true;
        };
      };
    })
  ];
}
