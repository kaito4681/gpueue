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
    HOME=$TMPDIR fish --no-execute "$fishInitPath"
    # With a system puzuz in PATH, the bash hook registers its completion.
    PATH=${puzuz}/bin:$PATH bash -c "complete -r; source $bashInitPath; complete -p puzuz"
    touch "$out"
  ''
