# The test executables of puzuz-helper (its unit tests, the binary's tests
# and tests/protocol.rs), built like the package but not run: many of them
# cannot pass in the build sandbox (see `doCheck` in package.nix), so
# nix/tests/helper-tests.nix runs all of them in a NixOS VM instead.
#
# Installed as $out/bin/<target>-<kind>, e.g. protocol-test.
{ puzuz, jq }:

puzuz.overrideAttrs (old: {
  pname = "puzuz-helper-tests";
  nativeBuildInputs = old.nativeBuildInputs ++ [ jq ];
  doCheck = false;

  buildPhase = ''
    runHook preBuild
    cargo test --release --offline --no-run -p puzuz-helper \
      --message-format=json-render-diagnostics > cargo-test.json
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p "$out/bin"
    jq -r 'select(.reason == "compiler-artifact" and .profile.test and .executable != null)
           | "\(.executable) \(.target.name)-\(.target.kind[0])"' cargo-test.json |
      while read -r exe name; do
        install -Dm755 "$exe" "$out/bin/$name"
      done
    test -x "$out/bin/protocol-test"
    runHook postInstall
  '';

  # The package's postInstall (docs, completions) does not apply.
  postInstall = "";
  meta = old.meta // {
    description = "Test executables of puzuz-helper";
    mainProgram = "protocol-test";
  };
})
