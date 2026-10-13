# Runs every test of puzuz-helper in a NixOS VM, with nothing skipped
# (`nix build .#checks.x86_64-linux.helper-tests`).
#
# The package build skips the tests that cannot pass in the Nix build sandbox
# (see `skippedTests` in nix/package.nix): mainly those in tests/protocol.rs
# that start a job, because the helper makes the job's log directory setgid
# and the sandbox refuses setgid modes. The tests use fakes for systemd, CRIU
# and the host, and must run as an unprivileged user (tests/protocol.rs
# asserts it), so the VM only needs a normal user, a real /tmp and python3.
# A test that skips itself prints "skipped: <why>"; the check fails then, so
# that nothing is skipped here.
{ self, pkgs }:
let
  tests = self.packages.${pkgs.stdenv.hostPlatform.system}.helper-tests;
  # `sh` for the tests that start shell commands: dash, as on Debian and
  # Ubuntu. NixOS's sh is bash, which reaps a finished background child
  # before `exec`, so `sh -c "true & exec sleep 30"` (the zombie test in
  # access.rs) leaves no zombie.
  dashSh = pkgs.runCommand "dash-as-sh" { } ''
    mkdir -p "$out/bin"
    ln -s ${pkgs.dash}/bin/dash "$out/bin/sh"
  '';
in
pkgs.testers.runNixOSTest {
  name = "puzuz-helper-tests";

  nodes.machine = {
    users.users.alice.isNormalUser = true;
    # forkguard's test runs a Python script (and skips itself without it).
    environment.systemPackages = [ pkgs.python3 ];
    virtualisation.memorySize = 2048;
    virtualisation.cores = 4;
  };

  testScript = ''
    import re
    import shlex

    machine.wait_for_unit("multi-user.target")
    binaries = machine.succeed("ls ${tests}/bin").split()
    assert "protocol-test" in binaries, binaries

    totals = {"passed": 0, "failed": 0, "ignored": 0}
    for name in binaries:
        with subtest(name):
            # stdin and stdout are not the VM's console (/dev/hvc0, owned by
            # root), so the tests see what a user's shell would give them.
            out_file = f"/tmp/{name}.log"
            cmd = (
                f"cd /tmp && PATH=${dashSh}/bin:$PATH ${tests}/bin/{name} --test-threads=2 --nocapture"
                f" </dev/null >{out_file} 2>&1"
            )
            status, _ = machine.execute(f"su -l alice -c {shlex.quote(cmd)}")
            out = machine.succeed(f"cat {out_file}")
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

    print(f"puzuz-helper tests: {totals}")
    assert totals["failed"] == 0, totals
  '';
}
