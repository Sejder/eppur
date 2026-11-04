{
  description = "Pin NixOS packages that fail to build to their last good nixpkgs";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      lib.mkOverlay = import ./nix/overlay.nix;
      nixosModules.default = import ./nix/module.nix self;
      homeManagerModules.default = import ./nix/hm-module.nix self;
      homeModules.default = self.homeManagerModules.default;

      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "eppur";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postInstall = ''
              wrapProgram $out/bin/eppur --suffix PATH : ${pkgs.lib.makeBinPath [ pkgs.nix-output-monitor ]}
            '';
            meta.mainProgram = "eppur";
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              rustc
              rustfmt
              nix-output-monitor
            ];
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          locked = (builtins.fromJSON (builtins.readFile ./flake.lock)).nodes.nixpkgs.locked;
          pin = {
            inherit locked;
            since = "2026-01-01";
            reason = "build";
            checkedNarHash = "";
          };
          pins = builtins.toFile "pins.json" (
            builtins.toJSON {
              version = 2;
              hosts.test.pins.nixpkgs.hello = pin;
              hosts.test.pins.nixpkgs.figlet = pin // {
                overlays = true;
              };
              homes.alice.pins.nixpkgs.cowsay = pin;
            }
          );
          prev = {
            stdenv.hostPlatform.system = system;
            stdenv.buildPlatform.system = system;
            config = { };
            hello = "new";
            cowsay = "new";
          };
          tuned = import nixpkgs {
            localSystem = {
              inherit system;
              gcc.arch = if system == "x86_64-linux" then "x86-64-v3" else "armv8.2-a";
            };
          };
          onReal =
            p:
            self.lib.mkOverlay {
              inherit pins;
              name = "test";
              input = "nixpkgs";
            } p p;
          overlay =
            args:
            self.lib.mkOverlay (
              {
                inherit pins;
                name = "test";
                input = "nixpkgs";
              }
              // args
            ) prev prev;

          readOnly = nixpkgs.lib.nixosSystem {
            modules = [
              nixpkgs.nixosModules.readOnlyPkgs
              self.nixosModules.default
              {
                nixpkgs.pkgs = pkgs;
                networking.hostName = "test";
                programs.eppur = {
                  enable = true;
                  pinsFile = pins;
                };
                boot.loader.grub.enable = false;
                fileSystems."/" = {
                  device = "none";
                  fsType = "tmpfs";
                };
                system.stateVersion = nixpkgs.lib.trivial.release;
              }
            ];
          };
          machine = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              {
                networking.hostName = "test";
                programs.eppur = {
                  enable = true;
                  pinsFile = pins;
                  hosts = [ "test" ];
                  flake = "/home/alice/nixos";
                  autoUpgrade = {
                    enable = true;
                    user = "alice";
                    operation = "boot";
                    inputs = [ "nixpkgs" ];
                  };
                  nixpkgs.extra = {
                    source = nixpkgs;
                    argName = "extra-pkgs";
                  };
                };
                nixpkgs.overlays = [
                  (final: prev: {
                    hello = prev.hello.overrideAttrs { pname = "hello-local"; };
                    cowsay = prev.cowsay.overrideAttrs { pname = "cowsay-local"; };
                    figlet = prev.figlet.overrideAttrs { pname = "figlet-local"; };
                  })
                ];
                boot.loader.grub.enable = false;
                fileSystems."/" = {
                  device = "none";
                  fsType = "tmpfs";
                };
                system.stateVersion = nixpkgs.lib.trivial.release;
              }
              {
                nixpkgs.overlays = nixpkgs.lib.mkOrder 2000 [
                  (final: prev: { figlet = prev.figlet.overrideAttrs (o: { pname = o.pname + "-x"; }); })
                ];
              }
              (
                { extra-pkgs, ... }:
                {
                  environment.etc."extra-hello".text = extra-pkgs.hello.pname;
                }
              )
            ];
          };
          settings = builtins.fromJSON machine.config.environment.etc."eppur.json".text;
          upgrade = machine.config.systemd.services.eppur-upgrade;

          # Home Manager itself is not an input; these stand in for the few of
          # its options the module uses.
          hmStub =
            { lib, ... }:
            {
              config._module.args.osConfig = lib.mkDefault null;
              options = {
                home.username = lib.mkOption { type = lib.types.str; };
                home.profileDirectory = lib.mkOption {
                  type = lib.types.str;
                  default = "/home/alice/.nix-profile";
                };
                home.packages = lib.mkOption {
                  type = lib.types.listOf lib.types.package;
                  default = [ ];
                };
                xdg.configFile = lib.mkOption {
                  type = lib.types.attrsOf (lib.types.submodule { options.text = lib.mkOption { type = lib.types.str; }; });
                  default = { };
                };
                systemd.user.services = lib.mkOption {
                  type = lib.types.attrsOf lib.types.raw;
                  default = { };
                };
                systemd.user.timers = lib.mkOption {
                  type = lib.types.attrsOf lib.types.raw;
                  default = { };
                };
                nixpkgs.overlays = lib.mkOption {
                  type = lib.types.listOf lib.types.raw;
                  default = [ ];
                };
                nixpkgs.config = lib.mkOption {
                  type = lib.types.nullOr lib.types.attrs;
                  default = null;
                };
                xdg.configHome = lib.mkOption {
                  type = lib.types.str;
                  default = "/home/alice/.config";
                };
                assertions = lib.mkOption {
                  type = lib.types.listOf lib.types.raw;
                  default = [ ];
                };
                warnings = lib.mkOption {
                  type = lib.types.listOf lib.types.str;
                  default = [ ];
                };
              };
            };
          home = nixpkgs.lib.evalModules {
            modules = [
              hmStub
              self.homeManagerModules.default
              {
                _module.args.pkgs = pkgs;
                home.username = "alice";
                programs.eppur = {
                  enable = true;
                  pinsFile = pins;
                  homes = [ "alice" ];
                  flake = "/home/alice/nixos";
                  autoUpgrade.enable = true;
                  nixpkgs.extra.source = nixpkgs;
                };
              }
            ];
          };
          oldName = nixpkgs.lib.evalModules {
            modules = [
              hmStub
              self.homeManagerModules.default
              {
                _module.args.pkgs = pkgs;
                home.username = "alice";
                programs.nixheal.enable = true;
              }
            ];
          };
          homeInNixos = nixpkgs.lib.evalModules {
            modules = [
              hmStub
              self.homeManagerModules.default
              {
                _module.args.pkgs = pkgs;
                _module.args.osConfig = {
                  networking.hostName = "alice-laptop";
                  home-manager.useGlobalPkgs = false;
                  programs.eppur = {
                    enable = true;
                    name = "laptop";
                    input = "nixos";
                    pinsFile = pins;
                  };
                };
                home.username = "alice";
              }
            ];
          };
          shadowing = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              {
                networking.hostName = "test";
                programs.eppur = {
                  enable = true;
                  pinsFile = pins;
                  nixpkgs.extra = {
                    source = nixpkgs;
                    argName = "pkgs";
                  };
                };
                boot.loader.grub.enable = false;
                fileSystems."/" = {
                  device = "none";
                  fsType = "tmpfs";
                };
                system.stateVersion = nixpkgs.lib.trivial.release;
              }
            ];
          };
          probe = machine.config.programs.eppur;
          homeSettings = builtins.fromJSON home.config.xdg.configFile."eppur.json".text;
          homeUpgrade = home.config.systemd.user.services.eppur-upgrade;
          failedAssertions = cfg: builtins.filter (a: !a.assertion) cfg.assertions;
        in
        {
          package = self.packages.${system}.default;
          overlay =
            assert (overlay { }).hello.drvPath == pkgs.hello.drvPath;
            assert !((overlay { }) ? cowsay);
            assert overlay { input = "nixpkgs-unstable"; } == { };
            assert overlay { name = "other"; } == { };
            assert (overlay { kind = "homes"; name = "alice"; }) ? cowsay;
            assert overlay { pins = ./does-not-exist.json; } == { };
            assert overlay { pins = null; } == { };
            assert
              (self.lib.mkOverlay {
                inherit pins;
                name = "test";
                input = "nixpkgs";
              } prev (prev // { config.rewriteURL = null; config.notAnOption = null; })).hello.drvPath
              == pkgs.hello.drvPath;
            assert
              (self.lib.mkOverlay {
                inherit pins;
                name = "test";
                input = "nixpkgs";
                nixpkgsConfig = { };
              } prev prev).hello.drvPath
              == pkgs.hello.drvPath;
            assert (onReal pkgs).hello.drvPath == pkgs.hello.drvPath;
            assert (onReal pkgs.pkgsStatic).hello.drvPath == pkgs.pkgsStatic.hello.drvPath;
            assert
              (onReal pkgs.pkgsCross.riscv64).hello.drvPath == pkgs.pkgsCross.riscv64.hello.drvPath;
            assert (onReal pkgs.pkgsMusl).hello.drvPath == pkgs.pkgsMusl.hello.drvPath;
            assert
              import ./nix/platform.nix null
                {
                  system = "x86_64-linux";
                  config = "x86_64-unknown-linux-gnu";
                }
                {
                  system = "aarch64-linux";
                  config = "aarch64-unknown-linux-gnu";
                } == {
                localSystem.system = "x86_64-linux";
                crossSystem.config = "aarch64-unknown-linux-gnu";
              };
            assert (onReal pkgs.pkgsLLVM).hello.drvPath == pkgs.pkgsLLVM.hello.drvPath;
            assert (onReal tuned).hello.drvPath == tuned.hello.drvPath;
            pkgs.runCommand "eppur-overlay-check" { } "touch $out";
          module =
            assert machine.pkgs.hello.pname == "hello";
            assert machine.pkgs.cowsay.pname == "cowsay-local";
            assert machine.pkgs.figlet.pname == "figlet-local-x";
            assert
              (machine.config.programs.eppur.importPinned {
                inherit locked;
                overlays = true;
              }).figlet.pname == "figlet-local";
            assert
              (machine.config.programs.eppur.importPinned {
                inherit locked;
                overlays = false;
              }).figlet.pname == "figlet";
            assert machine.config.programs.eppur.packageSets ? extra;
            assert machine.config.environment.etc."extra-hello".text == "hello";
            assert settings.self == "test";
            assert settings.kind == "nixos";
            assert settings.quiet == false;
            assert settings.switcher == "nh";
            assert settings.commit == false;
            assert upgrade.serviceConfig.User == "alice";
            assert builtins.match ".*eppur build --built-link /run/eppur/system --no-commit --timeout 12h --update nixpkgs" upgrade.serviceConfig.ExecStart != null;
            assert builtins.match "[+]/nix/store/.*-eppur-activate" upgrade.serviceConfig.ExecStartPost != null;
            assert machine.config.systemd.timers.eppur-upgrade.timerConfig.OnCalendar == "04:40";
            assert !(settings ? inputs);
            assert machine.config.programs.eppur.packageSets ? nixpkgs;
            assert settings.input == "nixpkgs";
            assert upgrade.environment.EPPUR_CONFIG == "/etc/eppur.json";
            assert upgrade.unitConfig.OnFailure == [ ];
            assert upgrade.unitConfig.X-StopOnRemoval == false;
            assert !(upgrade.serviceConfig ? TimeoutStartSec);
            assert builtins.any (nixpkgs.lib.hasInfix "read-only nixpkgs") readOnly.config.warnings;
            assert readOnly.config.system.build.toplevel.drvPath != "";
            assert upgrade.restartIfChanged == false;
            assert builtins.any (p: (p.pname or "") == "nh") machine.config.environment.systemPackages;
            assert probe.enable && probe.name == "test";
            assert nixpkgs.lib.hasSuffix "-pins.json" (toString probe.pinsFile);
            assert builtins.attrNames probe.packageSets == [ "extra" "nixpkgs" ];
            assert builtins.elem machine.config.programs.eppur.package machine.config.environment.systemPackages;
            pkgs.runCommand "eppur-module-check" { } "touch $out";
          activate-refuses =
            let
              script = nixpkgs.lib.removePrefix "+" machine.config.systemd.services.eppur-upgrade.serviceConfig.ExecStartPost;
            in
            pkgs.runCommand "eppur-activate-check" { } ''
              mkdir -p fake/bin
              ln -s "$PWD/fake" outside
              if EPPUR_BUILT_LINK="$PWD/outside" ${script} 2>err; then
                echo "activated a system outside the store"; exit 1
              fi
              grep -q "not in /nix/store" err
              ln -s ${pkgs.hello} notnixos
              if EPPUR_BUILT_LINK="$PWD/notnixos" ${script} 2>err; then
                echo "activated a store path that is not a NixOS system"; exit 1
              fi
              grep -q "not a NixOS system" err
              EPPUR_BUILT_LINK="$PWD/missing" ${script}
              touch $out
            '';
          home-module =
            assert failedAssertions home.config == [ ];
            assert home.config.programs.eppur.name == "alice";
            assert builtins.any (a: nixpkgs.lib.hasInfix "use the eppur flake" a.message) (
              failedAssertions oldName.config
            );
            assert homeSettings.kind == "home";
            assert homeSettings.self == "alice";
            assert homeSettings.homes == [ "alice" ];
            assert builtins.length home.config.nixpkgs.overlays == 1;
            assert home.config.programs.eppur.packageSets ? nixpkgs;
            assert builtins.match ".*eppur switch --no-commit --timeout 12h --update" homeUpgrade.Service.ExecStart != null;
            assert !(homeUpgrade.Unit ? After);
            assert homeUpgrade.Unit.X-SwitchMethod == "keep-old";
            assert home.config.systemd.user.timers.eppur-upgrade.Timer.OnCalendar == "04:40";
            assert builtins.elem home.config.programs.eppur.package home.config.home.packages;
            assert home.config.programs.eppur.packageSets.extra ? hello;
            assert homeInNixos.config.programs.eppur.name == "laptop";
            assert homeInNixos.config.programs.eppur.input == "nixos";
            assert homeInNixos.config.programs.eppur.packageSets ? nixos;
            assert builtins.any (a: nixpkgs.lib.hasInfix "argName = \"pkgs\" would shadow" a.message) (
              failedAssertions shadowing.config
            );
            pkgs.runCommand "eppur-home-module-check" { } "touch $out";
        }
      );
    };
}
