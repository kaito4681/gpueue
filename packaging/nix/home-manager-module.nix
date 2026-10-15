# home-manager module: programs.gpueue (the CLI and its shell completions).
#
# The daemon and the root helper need root and the system's systemd, so they
# cannot come from home-manager: an administrator installs them (NixOS
# module, .deb, install.sh, ...). The CLI must have the daemon's version;
# with `package = null` (the default) only the completions are set up, for
# the system's `gpueue`, whose version always matches.
{ config, lib, ... }:
let
  cfg = config.programs.gpueue;
  inherit (lib)
    literalExpression
    mkIf
    mkMerge
    mkOption
    types
    ;

  # The hooks load the completion files installed next to whichever `gpueue`
  # is in PATH (the system's: <prefix>/bin/gpueue and <prefix>/share/...), so
  # they match its version. Only when there are none do they ask
  # `gpueue completions <shell>` (which runs gpueue at every shell start).

  integrationOption =
    shell:
    mkOption {
      type = types.bool;
      default = true;
      description = ''
        Whether to load the completions of the system's `gpueue` in ${shell}
        when `package` is `null`. (A package brings its own completion
        files, which home-manager links into the profile.)
      '';
    };
in
{
  options.programs.gpueue = {
    enable = lib.mkEnableOption "the gpueue CLI and its shell completions";

    package = mkOption {
      type = types.nullOr types.package;
      default = null;
      example = literalExpression "inputs.gpueue.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = ''
        The gpueue package to install into the profile. `null` installs
        nothing and only sets up the completions of the system's `gpueue`
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
        if command -v gpueue >/dev/null 2>&1; then
          __gpueue_dir="$(dirname "$(dirname "$(readlink -f "$(command -v gpueue)")")")/share/bash-completion/completions"
          if [ -r "$__gpueue_dir/gpueue.bash" ]; then
            source "$__gpueue_dir/gpueue.bash"
          elif [ -r "$__gpueue_dir/gpueue" ]; then
            source "$__gpueue_dir/gpueue"
          else
            source <(gpueue completions bash)
          fi
          unset __gpueue_dir
        fi
      '';
    })

    (mkIf (cfg.package == null && cfg.enableZshIntegration) {
      # After compinit, which home-manager runs earlier in .zshrc when
      # programs.zsh.enableCompletion is on; without it (no compdef) the hook
      # does nothing.
      programs.zsh.initContent = ''
        if (( $+commands[gpueue] && $+functions[compdef] )); then
          __gpueue_dir=''${''${commands[gpueue]:A}:h:h}/share/zsh/site-functions
          if [[ -r $__gpueue_dir/_gpueue ]]; then
            fpath=($__gpueue_dir $fpath)
            autoload -Uz _gpueue && compdef _gpueue gpueue
          else
            source <(gpueue completions zsh)
          fi
          unset __gpueue_dir
        fi
      '';
    })

    (mkIf (cfg.package == null && cfg.enableFishIntegration) {
      programs.fish.interactiveShellInit = ''
        if command -q gpueue
          set -l __gpueue_file (path resolve (command -v gpueue) | path dirname | path dirname)/share/fish/vendor_completions.d/gpueue.fish
          if test -r $__gpueue_file
            source $__gpueue_file
          else
            gpueue completions fish | source
          end
        end
      '';
    })
  ]);
}
