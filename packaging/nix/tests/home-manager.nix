# Evaluation test of programs.puzuz (`nix build .#checks.<system>.home-manager`).
#
# Evaluates the module with lib.evalModules against stand-ins for the few
# home-manager options it sets (so the flake needs no home-manager input),
# checks what it sets for `package = null` and for a package, and parses the
# shell snippets with bash, zsh and fish.
{ self, pkgs }:
let
  inherit (pkgs) lib;
  puzuz = self.packages.${pkgs.stdenv.hostPlatform.system}.default;

  homeManagerStubs = {
    options = {
      home.packages = lib.mkOption {
        type = lib.types.listOf lib.types.package;
        default = [ ];
      };
      programs.bash.initExtra = lib.mkOption {
        type = lib.types.lines;
        default = "";
      };
      programs.zsh.initContent = lib.mkOption {
        type = lib.types.lines;
        default = "";
      };
      programs.fish.interactiveShellInit = lib.mkOption {
        type = lib.types.lines;
        default = "";
      };
    };
  };

  eval =
    module:
    (lib.evalModules {
      modules = [
        self.homeManagerModules.default
        homeManagerStubs
        module
      ];
    }).config;

  disabled = eval { };
  systemCli = eval { programs.puzuz.enable = true; };
  noFish = eval {
    programs.puzuz = {
      enable = true;
      enableFishIntegration = false;
    };
  };
  withPackage = eval {
    programs.puzuz = {
      enable = true;
      package = puzuz;
    };
  };

  hooks = c: [
    c.programs.bash.initExtra
    c.programs.zsh.initContent
    c.programs.fish.interactiveShellInit
  ];
in
assert disabled.home.packages == [ ] && lib.all (h: h == "") (hooks disabled);
assert systemCli.home.packages == [ ];
assert lib.hasInfix "puzuz completions bash" systemCli.programs.bash.initExtra;
assert lib.hasInfix "puzuz completions zsh" systemCli.programs.zsh.initContent;
assert lib.hasInfix "puzuz completions fish" systemCli.programs.fish.interactiveShellInit;
assert noFish.programs.fish.interactiveShellInit == "" && noFish.programs.bash.initExtra != "";
# A package brings its own completion files; no hooks then.
assert withPackage.home.packages == [ puzuz ] && lib.all (h: h == "") (hooks withPackage);
pkgs.runCommand "puzuz-home-manager-module"
  {
    nativeBuildInputs = [
      pkgs.bash
      pkgs.zsh
      pkgs.fish
    ];
    bashInit = systemCli.programs.bash.initExtra;
    zshInit = systemCli.programs.zsh.initContent;
    fishInit = systemCli.programs.fish.interactiveShellInit;
    passAsFile = [
      "bashInit"
      "zshInit"
      "fishInit"
    ];
  }
  ''
    bash -n "$bashInitPath"
    zsh -n "$zshInitPath"
    export HOME=$TMPDIR
    fish --no-execute "$fishInitPath"

    # A "system" puzuz whose completion files are installed next to it, and
    # that fails when asked to generate them: the hooks must use the files.
    sys=$TMPDIR/system
    mkdir -p "$sys/bin"
    cp -r ${puzuz}/share "$sys/share"
    printf '#!/bin/sh\nexit 1\n' > "$sys/bin/puzuz"
    chmod +x "$sys/bin/puzuz"
    # One that has no completion files: the hooks generate them.
    gen=$TMPDIR/generated
    mkdir -p "$gen/bin"
    printf '#!/bin/sh\nexec ${puzuz}/bin/puzuz "$@"\n' > "$gen/bin/puzuz"
    chmod +x "$gen/bin/puzuz"

    for dir in "$sys" "$gen"; do
      PATH=$dir/bin:$PATH bash -c "source $bashInitPath; complete -p puzuz"
      PATH=$dir/bin:$PATH zsh -f -c \
        "autoload -Uz compinit && compinit -u -d $TMPDIR/zcompdump; source $zshInitPath; [[ \''${_comps[puzuz]} == _puzuz ]]"
      PATH=$dir/bin:$PATH fish -c "source $fishInitPath; complete -c puzuz | grep -q ."
    done

    # Without compinit (no compdef), the zsh hook does nothing and says
    # nothing.
    msg=$(PATH=$sys/bin:$PATH zsh -f -c "source $zshInitPath" 2>&1)
    test -z "$msg"
    touch "$out"
  ''
