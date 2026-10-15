{
  description = "gpueue: multi-user GPU job queue with checkpoint/restore";

  # The latest stable branch (Rust 1.95; gpueue needs 1.88 or newer).
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        gpueue = pkgs.callPackage ./packaging/nix/package.nix { };
        default = gpueue;
        # CRIU 4.2.x (nixpkgs has 4.1.1); not a dependency of gpueue.
        criu = pkgs.callPackage ./packaging/nix/criu.nix { };
        # The gpueue-helper test executables, for checks.x86_64-linux.helper-tests.
        helper-tests = pkgs.callPackage ./packaging/nix/helper-tests.nix { inherit gpueue; };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.gpueue ];
          packages = with pkgs; [
            clippy
            rustfmt
            rust-analyzer
            shellcheck
            # forkguard's test runs a Python script (and skips itself without it).
            python3
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);

      # services.gpueue: the daemon, the root helper and the CLI.
      nixosModules.default = import ./packaging/nix/nixos-module.nix { inherit self; };

      # programs.gpueue: the CLI's completions (or the CLI) for one user.
      homeManagerModules.default = import ./packaging/nix/home-manager-module.nix;

      checks = forAllSystems (
        pkgs:
        {
          home-manager = import ./packaging/nix/tests/home-manager.nix { inherit self pkgs; };
        }
        # NixOS VM tests (they need KVM): the NixOS module, and every
        # gpueue-helper test, including those the package build skips.
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          nixos = import ./packaging/nix/tests/nixos.nix { inherit self pkgs; };
          helper-tests = import ./packaging/nix/tests/helper-tests.nix { inherit self pkgs; };
        }
      );
    };
}
