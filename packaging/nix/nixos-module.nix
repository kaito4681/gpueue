# NixOS module: services.gpueue (the daemon, the root helper and the CLI).
#
# The systemd units and the tmpfiles.d entries are generated from the files
# in packaging/ (the same ones the .deb and `gpueue service install` use),
# with ExecStart= pointing into the store, so they cannot drift apart.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.gpueue;
  inherit (lib)
    literalExpression
    mkDefault
    mkIf
    mkMerge
    mkOption
    types
    ;

  flakePackages = self.packages.${pkgs.stdenv.hostPlatform.system} or { };
  toml = pkgs.formats.toml { };
  configFile = toml.generate "gpueue-config.toml" cfg.settings;

  # True with `services.xserver.videoDrivers = [ "nvidia" ]` (also on
  # headless machines) or `hardware.nvidia.datacenter.enable`.
  nvidia = config.hardware.nvidia;

  # The packaged units and tmpfiles.d entries with the binaries' store paths.
  # `substitute --replace-fail` makes the build fail if a template stops
  # using ExecStart=/usr/bin/.
  units = pkgs.runCommand "gpueue-systemd-units" { } ''
    mkdir -p "$out/lib/systemd/system" "$out/lib/tmpfiles.d"
    for unit in ${../systemd}/*.service; do
      substitute "$unit" "$out/lib/systemd/system/$(basename "$unit")" \
        --replace-fail "ExecStart=/usr/bin/" "ExecStart=${cfg.package}/bin/"
    done
    cp ${../tmpfiles.d/gpueue.conf} "$out/lib/tmpfiles.d/gpueue.conf"
    # Raises JobTimeoutSec= of the shutdown targets, which otherwise force the
    # machine down 30 minutes into a shutdown, also in the middle of the
    # helper's dumps. NixOS links the <unit>.d directories of systemd.packages.
    for target in reboot poweroff halt kexec soft-reboot; do
      mkdir -p "$out/lib/systemd/system/$target.target.d"
      cp ${../systemd/shutdown-target-timeout.conf} \
        "$out/lib/systemd/system/$target.target.d/gpueue.conf"
    done
  '';

  serviceOverrides = {
    wantedBy = [ "multi-user.target" ];
    # Both read /etc/gpueue/config.toml at start-up. Restarting either one
    # leaves running jobs alone (they run in their own units).
    restartTriggers = [ configFile ];
    # `systemctl restart` instead of stop + start on nixos-rebuild switch.
    stopIfChanged = false;
  };
in
{
  options.services.gpueue = {
    enable = lib.mkEnableOption "the gpueue job queue (daemon, root helper and CLI)";

    package = mkOption {
      type = types.package;
      default = flakePackages.default;
      defaultText = literalExpression "inputs.gpueue.packages.\${system}.default";
      description = ''
        The gpueue package. Its `bin/gpueue` runs the daemon and its
        `bin/gpueue-helper` the root helper; it is also added to
        `environment.systemPackages`.
      '';
    };

    admins = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "alice" ];
      description = ''
        Users added to the `gpueue-admin` group. Its members administer the
        queue: all users' jobs, groups, GPU limits and shutdown.
      '';
    };

    settings = mkOption {
      type = types.submodule { freeformType = toml.type; };
      default = { };
      example = literalExpression ''
        {
          grace_secs = 60;
          checkpoint_size_limit_bytes = 500000000000;
        }
      '';
      description = ''
        Contents of `/etc/gpueue/config.toml`; see `packaging/config.toml`
        in the gpueue repository (or `docs/install.md`, "Configuration") for
        the keys. The module sets `criu_path`, `cuda_checkpoint_path`,
        `nvidia_smi_path`, `nvml_library` and `shell` (with
        `lib.mkDefault`) from the options below.
      '';
    };

    criuPackage = mkOption {
      type = types.package;
      default = flakePackages.criu;
      defaultText = literalExpression "inputs.gpueue.packages.\${system}.criu";
      description = ''
        CRIU (4.2 or later) used by the helper to checkpoint and restore
        jobs. The default is the CRIU 4.2 package of the gpueue flake.
      '';
    };

    cudaCheckpointPackage = mkOption {
      type = types.nullOr types.package;
      default = null;
      example = literalExpression "pkgs.callPackage ./cuda-checkpoint.nix { }";
      description = ''
        A package with `bin/cuda-checkpoint` (NVIDIA's utility; not in
        nixpkgs, see `docs/install.md` for an example derivation). When it
        is `null`, jobs that use a GPU cannot be checkpointed.
      '';
    };

    nvidiaSmiPath = mkOption {
      type = types.nullOr types.str;
      default = if nvidia.enabled then "${lib.getBin nvidia.package}/bin/nvidia-smi" else null;
      defaultText = literalExpression ''
        if config.hardware.nvidia.enabled
        then "''${lib.getBin config.hardware.nvidia.package}/bin/nvidia-smi"
        else null
      '';
      description = ''
        Path of `nvidia-smi`, which the helper uses to find a job's CUDA
        processes and GPU memory. `null` leaves the default (a lookup
        in the helper's PATH, which has no `nvidia-smi` on NixOS).
      '';
    };

    nvmlLibrary = mkOption {
      type = types.nullOr types.str;
      default = if nvidia.enabled then "${nvidia.package}/lib/libnvidia-ml.so.1" else null;
      defaultText = literalExpression ''
        if config.hardware.nvidia.enabled
        then "''${config.hardware.nvidia.package}/lib/libnvidia-ml.so.1"
        else null
      '';
      description = ''
        Path of NVML (`libnvidia-ml.so.1`), which the daemon loads to detect
        the GPUs. The dynamic loader does not search the NVIDIA driver's
        directory on NixOS, so without it GPU detection falls back to the
        `gpus` list of `settings`.
      '';
    };
  };

  config = mkIf cfg.enable {
    services.gpueue.settings = mkMerge [
      {
        criu_path = mkDefault "${cfg.criuPackage}/bin/criu";
        # NixOS keeps /bin/sh (environment.binsh) but nothing else in /bin
        # and /usr/bin. A job runs with the environment of the `gpueue add`
        # that submitted it, so the rest of its PATH is the user's
        # (/run/wrappers/bin, /run/current-system/sw/bin, ...); the shell
        # itself does not depend on it.
        shell = mkDefault [
          "/bin/sh"
          "-c"
        ];
      }
      (mkIf (cfg.cudaCheckpointPackage != null) {
        cuda_checkpoint_path = mkDefault "${cfg.cudaCheckpointPackage}/bin/cuda-checkpoint";
      })
      (mkIf (cfg.nvidiaSmiPath != null) {
        nvidia_smi_path = mkDefault cfg.nvidiaSmiPath;
      })
      (mkIf (cfg.nvmlLibrary != null) {
        nvml_library = mkDefault cfg.nvmlLibrary;
      })
    ];

    # The helper runs as root and reads the file only if neither it nor a
    # directory above it is a symlink or writable by anyone but root. A
    # `mode` makes NixOS copy the file into /etc (owned by root) instead of
    # linking it into the store through /etc/static.
    environment.etc."gpueue/config.toml" = {
      source = configFile;
      mode = "0644";
    };

    environment.systemPackages = [ cfg.package ];

    systemd.packages = [ units ];
    systemd.tmpfiles.packages = [ units ];
    systemd.services.gpueue-helper = serviceOverrides;
    systemd.services.gpueue = serviceOverrides;

    # packaging/sysusers.d/gpueue.conf
    users.users.gpueue = {
      isSystemUser = true;
      group = "gpueue";
      home = "/var/lib/gpueue/daemon";
      description = "gpueue job queue daemon";
    };
    users.groups.gpueue = { };
    users.groups.gpueue-admin.members = cfg.admins;
  };
}
