{
  description = "puzuz: multi-user GPU job queue with checkpoint/restore";

  # The latest stable branch (Rust 1.95; puzuz needs 1.88 or newer).
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
        puzuz = pkgs.callPackage ./nix/package.nix { };
        default = puzuz;
        # CRIU 4.2.x (nixpkgs has 4.1.1); not a dependency of puzuz.
        criu = pkgs.callPackage ./nix/criu.nix { };
        # The puzuz-helper test executables, for checks.x86_64-linux.helper-tests.
        helper-tests = pkgs.callPackage ./nix/helper-tests.nix { inherit puzuz; };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.puzuz ];
          packages = with pkgs; [
            clippy
            rustfmt
            rust-analyzer
            shellcheck
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);

      # services.puzuz: the daemon, the root helper and the CLI.
      nixosModules.default = import ./nix/nixos-module.nix { inherit self; };

      # programs.puzuz: the CLI's completions (or the CLI) for one user.
      homeManagerModules.default = import ./nix/home-manager-module.nix;

      checks = forAllSystems (
        pkgs:
        {
          home-manager = import ./nix/tests/home-manager.nix { inherit self pkgs; };
        }
        # NixOS VM tests (they need KVM): the NixOS module, and every
        # puzuz-helper test, including those the package build skips.
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          nixos = import ./nix/tests/nixos.nix { inherit self pkgs; };
          helper-tests = import ./nix/tests/helper-tests.nix { inherit self pkgs; };
        }
      );
    };
}
