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

  skippedTests = [
    # Uses /etc/passwd as "a root-owned system file"; in the sandbox it is a
    # file that Nix generates for the build user.
    "access::tests::restored_objects_must_be_the_owners_to_open"
  ]
  # puzuz-helper/tests/protocol.rs: these spawn a job, and the helper makes
  # the job's log directory setgid (mode 2770). Nix's sandbox refuses setuid
  # and setgid modes (its seccomp filter fails chmod with EPERM).
  ++ [
    "a_dump_past_its_deadline_is_killed_and_the_job_resumed"
    "a_restored_job_holding_files_its_owner_may_not_open_is_killed"
    "a_stopping_helper_abandons_queued_launches_and_refuses_new_ones"
    "a_stopping_helper_reports_an_abandoned_restore_as_not_started"
    "an_interrupted_restore_is_killed_and_reported"
    "checkpoint_then_restore"
    "concurrent_spawns_and_restores"
    "delete_checkpoint_refuses_live_jobs"
    "dump_refuses_a_job_that_depends_on_a_hidden_mount"
    "dump_refuses_sockets_bound_on_host_filesystems"
    "dump_refuses_without_free_space_and_keeps_a_stopped_job_stopped"
    "exit_events_are_persisted_replayed_and_acked"
    "failed_dump_leaves_the_job_running_and_stopped_jobs_are_resumed_first"
    "plugin_mode_refuses_to_dump_cuda_jobs_without_the_plugin"
    "recovery_readopts_running_jobs_and_reports_exits"
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
      # Embedded with include_str!: unit templates, config, release key, install.sh.
      ../packaging
      ../scripts/install.sh
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
