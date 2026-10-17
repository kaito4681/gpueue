# Runs every test of gpueue-helper in a NixOS VM, with nothing skipped
# (`nix build .#checks.x86_64-linux.helper-tests`).
#
# The package build runs no tests (see `doCheck` in packaging/nix/package.nix): those
# in tests/protocol.rs that start a job cannot pass in the Nix build sandbox,
# because the helper makes the job's log directory setgid and the sandbox
# refuses setgid modes. The tests use fakes for systemd, CRIU
# and the host, and must run as an unprivileged user (tests/protocol.rs
# asserts it), so the VM only needs a normal user, a real /tmp and python3.
# A test that skips itself prints "skipped: <why>"; the check fails then, so
# that nothing is skipped here.
{ self, pkgs }:
let
  tests = self.packages.${pkgs.stdenv.hostPlatform.system}.helper-tests;
in
pkgs.testers.runNixOSTest {
  name = "gpueue-helper-tests";

  nodes.machine = {
    users.users.alice.isNormalUser = true;
    # forkguard's test runs a Python script (and skips itself without it).
    # tests/dump_hook.rs runs a job in a user and PID namespace of its own
    # with unshare (and skips itself where unprivileged user namespaces are
    # not available, unlike here).
    environment.systemPackages = [
      pkgs.python3
      pkgs.util-linux
    ];
    virtualisation.memorySize = 2048;
    virtualisation.cores = 4;
  };

  testScript = ''
    import re
    import shlex

    machine.wait_for_unit("multi-user.target")
    binaries = machine.succeed("ls ${tests}/bin").split()
    assert "protocol-test" in binaries, binaries
    assert "dump_hook-test" in binaries, binaries

    totals = {"passed": 0, "failed": 0, "ignored": 0}
    for name in binaries:
        with subtest(name):
            cmd = (
                "cd /tmp && GPUEUE_HELPER_EXE=${tests}/libexec/gpueue-helper "
                f"${tests}/bin/{name} --test-threads=2 --nocapture 2>&1"
            )
            status, out = machine.execute(f"su -l alice -c {shlex.quote(cmd)}")
            print(out)
            summaries = re.findall(
                r"test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored", out
            )
            for passed, failed, ignored in summaries:
                totals["passed"] += int(passed)
                totals["failed"] += int(failed)
                totals["ignored"] += int(ignored)
            assert status == 0 and summaries, f"{name} failed (exit status {status})"
            skipped = [l for l in out.splitlines() if "skipped: " in l]
            assert not skipped, f"{name}: tests skipped themselves: {skipped}"

    print(f"gpueue-helper tests: {totals}")
    assert totals["failed"] == 0, totals
  '';
}
