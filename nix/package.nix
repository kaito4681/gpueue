# puzuz built from this source tree: `bin/puzuz` (CLI and daemon),
# `bin/puzuz-helper` (the root helper), shell completions and the docs.
{
  lib,
  stdenv,
  rustPlatform,
  installShellFiles,
}:

let
  cargoToml = builtins.fromTOML (builtins.readFile ../Cargo.toml);

  # puzuz-helper/tests/protocol.rs: these spawn a job, and the helper makes
  # the job's log directory setgid (mode 2770). Nix's sandbox refuses setuid
  # and setgid modes (its seccomp filter fails chmod with EPERM).
  # checks.x86_64-linux.helper-tests (nix/tests/helper-tests.nix) runs all
  # puzuz-helper tests, these included, in a NixOS VM.
  skippedTests = [
    "a_dump_interrupted_before_its_finalization_is_finished_at_start"
    "a_dump_past_its_deadline_is_killed_and_the_job_resumed"
    "a_dump_that_cannot_be_finalized_keeps_its_image"
    "a_restored_job_holding_files_its_owner_may_not_open_is_killed"
    "a_stopping_helper_abandons_queued_launches_and_refuses_new_ones"
    "a_stopping_helper_reports_an_abandoned_restore_as_not_started"
    "an_interrupted_restore_is_killed_and_reported"
    "an_interrupted_restore_without_its_unit_is_reported"
    "checkpoint_then_restore"
    "concurrent_spawns_and_restores"
    "delete_checkpoint_refuses_live_jobs"
    "dump_counts_private_tmpfs_and_refuses_reuseport_sockets"
    "dump_refuses_a_job_that_depends_on_a_hidden_mount"
    "dump_refuses_sockets_bound_on_host_filesystems"
    "dump_refuses_without_free_space_and_keeps_a_stopped_job_stopped"
    "exit_events_are_persisted_replayed_and_acked"
    "failed_dump_leaves_the_job_running_and_stopped_jobs_are_resumed_first"
    "nix_gc_roots_follow_jobs_and_their_checkpoints"
    "plugin_mode_refuses_to_dump_cuda_jobs_without_the_plugin"
    "recovery_readopts_running_jobs_and_reports_exits"
    "restore_checks_do_not_block_the_helper_and_hold_off_deletes"
    "restore_checks_the_images_paths_first"
    "restore_refuses_groups_the_owner_left_since_the_dump"
    "restore_uses_the_mode_of_the_image"
    "shutdown_checkpoints_everything_except_no_checkpoint_jobs"
    "shutdown_reports_refused_and_failed_dumps_apart"
    "spawn_validates_and_sets_cuda_env"
  ];
in
rustPlatform.buildRustPackage {
  pname = "puzuz";
  inherit (cargoToml.workspace.package) version;

  # Only what the build reads, so that editing the flake, CI files or tests
  # scripts outside this set does not rebuild the package.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      # Embedded with include_str!: unit templates, config, release key, maintainer
      # scripts, install.sh, deb-check.sh (the last ones by the marker test),
      # get-minisign.sh (by the minisign pin test).
      ../packaging
      ../scripts/install.sh
      ../scripts/dist/deb-check.sh
      ../scripts/dist/get-minisign.sh
      ../README.md
      ../LICENSE
      ../docs
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

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

  # Tests that cannot pass in the Nix build sandbox (CI runs them all).
  # `--exact`: skip these names only, not every test whose name contains one.
  checkFlags = [ "--exact" ] ++ map (t: "--skip=${t}") skippedTests;

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
