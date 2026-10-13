# home-manager module: programs.puzuz (the CLI and its shell completions).
#
# The daemon and the root helper need root and the system's systemd, so they
# cannot come from home-manager: an administrator installs them (NixOS
# module, .deb, install.sh, ...). The CLI must have the daemon's version;
# with `package = null` (the default) only the completions are set up, for
# the system's `puzuz`, whose version always matches.
{ config, lib, ... }:
let
  cfg = config.programs.puzuz;
  inherit (lib)
    literalExpression
    mkIf
    mkMerge
    mkOption
    types
    ;

  # Generates the completions of whichever `puzuz` is in PATH (the system's)
  # when the shell starts, so they match its version.
  hasPuzuz = "command -v puzuz >/dev/null 2>&1";

  integrationOption =
    shell:
    mkOption {
      type = types.bool;
      default = true;
      description = ''
        Whether to load the completions of the system's `puzuz` in ${shell}
        when `package` is `null`. (A package brings its own completion
        files, which home-manager links into the profile.)
      '';
    };
in
{
  options.programs.puzuz = {
    enable = lib.mkEnableOption "the puzuz CLI and its shell completions";

    package = mkOption {
      type = types.nullOr types.package;
      default = null;
      example = literalExpression "inputs.puzuz.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = ''
        The puzuz package to install into the profile. `null` installs
        nothing and only sets up the completions of the system's `puzuz`
        (which an administrator installed together with the daemon). A
        package must have the daemon's version: commands fail with a version
        mismatch otherwise.
      '';
    };

    enableBashIntegration = integrationOption "Bash";
    enableZshIntegration = integrationOption "Zsh";
    enableFishIntegration = integrationOption "Fish";
  };

  config = mkIf cfg.enable (mkMerge [
    (mkIf (cfg.package != null) { home.packages = [ cfg.package ]; })

    (mkIf (cfg.package == null && cfg.enableBashIntegration) {
      programs.bash.initExtra = ''
        if ${hasPuzuz}; then
          source <(puzuz completions bash)
        fi
      '';
    })

    (mkIf (cfg.package == null && cfg.enableZshIntegration) {
      # After compinit (home-manager runs it earlier in .zshrc).
      programs.zsh.initContent = ''
        if ${hasPuzuz}; then
          source <(puzuz completions zsh)
        fi
      '';
    })

    (mkIf (cfg.package == null && cfg.enableFishIntegration) {
      programs.fish.interactiveShellInit = ''
        if command -q puzuz
          puzuz completions fish | source
        end
      '';
    })
  ]);
}
