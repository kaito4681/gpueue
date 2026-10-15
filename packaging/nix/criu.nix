# CRIU 4.2.x. puzuz needs CRIU 4.2 or newer; nixpkgs still has 4.1.1.
# Exported as `packages.<system>.criu` (for the NixOS module); the puzuz
# package itself does not depend on it.
{
  criu,
  protobuf,
  docbook_xsl,
}:

criu.overrideAttrs (
  finalAttrs: previousAttrs: {
    version = "4.2.1";
    src = previousAttrs.src.override {
      rev = "v${finalAttrs.version}";
      hash = "sha256-Njk3RNxmKiFEu5nnVuY16XaJQxkgBL2hf7YU5XFh67Y=";
    };

    # As in nixpkgs, except for descriptor.proto: since 4.2 CRIU compiles it
    # from protobuf's include directory (found with pkg-config) instead of a
    # symlink under images/, and the old symlink now shadows it. Its protoc
    # call hard-codes /usr/include as the import path.
    postPatch = ''
      substituteInPlace ./Documentation/Makefile \
        --replace-fail "2>/dev/null" "" \
        --replace-fail "-m custom.xsl" "-m custom.xsl --skip-validation -x ${docbook_xsl}/xml/xsl/docbook/manpages/docbook.xsl"
      substituteInPlace ./Makefile \
        --replace-fail "head-name := \$(shell git tag -l v\$(CRIU_VERSION))" "head-name = ${finalAttrs.version}.0"
      substituteInPlace ./images/Makefile \
        --replace-fail "--proto_path=/usr/include" "--proto_path=${protobuf}/include"
    '';
  }
)
