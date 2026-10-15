# The test executables of gpueue-helper (its unit tests, the binary's tests
# and tests/protocol.rs), built like the package but not run: many of them
# cannot pass in the build sandbox (see `doCheck` in package.nix), so
# packaging/nix/tests/helper-tests.nix runs all of them in a NixOS VM instead.
#
# Installed as $out/bin/<target>-<kind>, e.g. protocol-test, and the
# helper binary the tests run as $out/libexec/gpueue-helper.
{ gpueue, jq }:

gpueue.overrideAttrs (old: {
  pname = "gpueue-helper-tests";
  nativeBuildInputs = old.nativeBuildInputs ++ [ jq ];
  doCheck = false;

  buildPhase = ''
    runHook preBuild
    cargo test --release --offline --no-run -p gpueue-helper \
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
    # The helper binary itself, which some tests run (as `verify-access`);
    # they find it through GPUEUE_HELPER_EXE (see
    # packaging/nix/tests/helper-tests.nix).
    helper=$(jq -r 'select(.reason == "compiler-artifact" and (.profile.test | not)
                            and .target.name == "gpueue-helper"
                            and .target.kind[0] == "bin" and .executable != null)
                     | .executable' cargo-test.json | head -n1)
    install -Dm755 "$helper" "$out/libexec/gpueue-helper"
    runHook postInstall
  '';

  # The package's postInstall (docs, completions) does not apply.
  postInstall = "";
  meta = old.meta // {
    description = "Test executables of gpueue-helper";
    mainProgram = "protocol-test";
  };
})
