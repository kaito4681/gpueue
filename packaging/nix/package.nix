# puzuz built from this source tree: `bin/puzuz` (CLI and daemon),
# `bin/puzuz-helper` (the root helper), shell completions and the docs.
{
  lib,
  stdenv,
  rustPlatform,
  installShellFiles,
}:

let
  cargoToml = builtins.fromTOML (builtins.readFile ../../Cargo.toml);

in
rustPlatform.buildRustPackage {
  pname = "puzuz";
  inherit (cargoToml.workspace.package) version;

  # Only what the build reads, so that editing the flake, CI files or tests
  # scripts outside this set does not rebuild the package.
  src = lib.fileset.toSource {
    root = ../..;
    # This directory (packaging/nix) is left out, so that editing the Nix
    # expressions alone does not rebuild the package either.
    fileset = lib.fileset.difference (lib.fileset.unions [
      ../../Cargo.toml
      ../../Cargo.lock
      ../../crates
      # Embedded with include_str!: unit templates, config, release key, maintainer
      # scripts, install.sh, deb-check.sh (the last ones by the marker test),
      # get-minisign.sh (by the minisign pin test).
      ../../packaging
      ../../scripts/install.sh
      ../../scripts/dist/deb-check.sh
      ../../scripts/dist/get-minisign.sh
      ../../README.md
      ../../LICENSE
      ../../docs
    ]) ./.;
  };

  cargoLock.lockFile = ../../Cargo.lock;

  # The default workspace member is only `puzuz`; the helper is a separate package.
  cargoBuildFlags = [
    "-p"
    "puzuz"
    "-p"
    "puzuz-helper"
  ];
  cargoTestFlags = [ "--workspace" ];

  # The helper's build script compiles its fork guard library with stdenv's
  # C compiler (`cc`); nothing else is needed at build time.
  nativeBuildInputs = [ installShellFiles ];

  # No tests in the build: many puzuz-helper tests (those in
  # tests/protocol.rs that start a job) cannot pass in the build sandbox,
  # because the helper makes the job's log directory setgid (mode 2770) and
  # the sandbox's seccomp filter fails setuid and setgid chmods with EPERM.
  # Skipping them by name needed a list that every new such test broke.
  # Instead CI runs the whole suite with this toolchain outside the sandbox
  # (`nix develop -c cargo test --workspace --locked`, on x86_64 and aarch64),
  # and checks.x86_64-linux.helper-tests (packaging/nix/tests/helper-tests.nix) runs
  # every puzuz-helper test in a NixOS VM.
  doCheck = false;

  # stdenv adds `-rpath $out/lib` to every link, including the helper's fork
  # guard library, which the helper embeds. A check build (`--rebuild`) runs
  # with a temporary output path that Nix rewrites in the result afterwards,
  # but not in the ThinLTO symbol suffixes derived from the hash of the module
  # that holds the library's bytes, so the helper differed between identical
  # builds. Nothing is installed in $out/lib anyway.
  NIX_NO_SELF_RPATH = true;

  # Panic messages embed the absolute source paths of the dependencies, which
  # are vendored under the build directory. In the sandbox that is always
  # /build, but without it (e.g. in the nixos/nix Docker image) it is
  # /nix/var/nix/builds/nix-<pid>-<random>, so the binaries differed between
  # identical builds. Map it to /build so that every build gives the same
  # output (a no-op in the sandbox). NIX_RUSTFLAGS is appended by nixpkgs'
  # rustc wrapper; RUSTFLAGS would replace the target's rustflags that
  # cargoSetupHook writes into the cargo configuration.
  preConfigure = ''
    export NIX_RUSTFLAGS="''${NIX_RUSTFLAGS:-} --remap-path-prefix=$NIX_BUILD_TOP=/build"
  '';

  postInstall = ''
    install -Dm644 -t $out/share/doc/puzuz README.md docs/*.md
    install -Dm644 -t $out/share/licenses/puzuz LICENSE
  ''
  + lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
    installShellCompletion --cmd puzuz \
      --bash <($out/bin/puzuz completions bash) \
      --zsh <($out/bin/puzuz completions zsh) \
      --fish <($out/bin/puzuz completions fish)
  '';

  meta = {
    description = "Multi-user GPU job queue with CRIU and cuda-checkpoint checkpoint/restore";
    homepage = "https://github.com/kaito4681/puzuz";
    license = lib.licenses.mit;
    mainProgram = "puzuz";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
}
