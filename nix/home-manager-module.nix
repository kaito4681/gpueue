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

  # The hooks load the completion files installed next to whichever `puzuz`
  # is in PATH (the system's: <prefix>/bin/puzuz and <prefix>/share/...), so
  # they match its version. Only when there are none do they ask
  # `puzuz completions <shell>` (which runs puzuz at every shell start).

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
        if command -v puzuz >/dev/null 2>&1; then
          __puzuz_dir="$(dirname "$(dirname "$(readlink -f "$(command -v puzuz)")")")/share/bash-completion/completions"
          if [ -r "$__puzuz_dir/puzuz.bash" ]; then
            source "$__puzuz_dir/puzuz.bash"
          elif [ -r "$__puzuz_dir/puzuz" ]; then
            source "$__puzuz_dir/puzuz"
          else
            source <(puzuz completions bash)
          fi
          unset __puzuz_dir
        fi
      '';
    })

    (mkIf (cfg.package == null && cfg.enableZshIntegration) {
      # After compinit, which home-manager runs earlier in .zshrc when
      # programs.zsh.enableCompletion is on; without it (no compdef) the hook
      # does nothing.
      programs.zsh.initContent = ''
        if (( $+commands[puzuz] && $+functions[compdef] )); then
          __puzuz_dir=''${''${commands[puzuz]:A}:h:h}/share/zsh/site-functions
          if [[ -r $__puzuz_dir/_puzuz ]]; then
            fpath=($__puzuz_dir $fpath)
            autoload -Uz _puzuz && compdef _puzuz puzuz
          else
            source <(puzuz completions zsh)
          fi
          unset __puzuz_dir
        fi
      '';
    })

    (mkIf (cfg.package == null && cfg.enableFishIntegration) {
      programs.fish.interactiveShellInit = ''
        if command -q puzuz
          set -l __puzuz_file (path resolve (command -v puzuz) | path dirname | path dirname)/share/fish/vendor_completions.d/puzuz.fish
          if test -r $__puzuz_file
            source $__puzuz_file
          else
            puzuz completions fish | source
          end
        end
      '';
    })
  ]);
}
