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
    };
}
