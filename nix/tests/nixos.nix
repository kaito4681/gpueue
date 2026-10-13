# NixOS VM test of services.puzuz (`nix build .#checks.x86_64-linux.nixos`).
#
# Boots a machine with the module, then:
#   1. the daemon and the helper run, /etc/puzuz/config.toml is a root-owned
#      copy, and `puzuz doctor` reports no failure (only GPU warnings);
#   2. a normal user's job runs with the user's PATH and finishes;
#   3. a job is checkpointed with CRIU and restored, and keeps counting;
#   4. switching to another package (a specialisation) restarts the daemon
#      and the helper from the new store paths while a job keeps running;
#   5. `enable = false` (another specialisation) removes the units.
{ self, pkgs }:
let
  inherit (pkgs) lib;
  puzuz = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  # The same binaries at another store path: to NixOS this is an upgrade
  # (every ExecStart= changes) without building puzuz twice.
  puzuzCopy = pkgs.runCommand "puzuz-copy-${puzuz.version}" { } ''
    cp -a ${puzuz} "$out"
  '';
in
pkgs.testers.runNixOSTest {
  name = "puzuz";

  nodes.machine = {
    imports = [ self.nixosModules.default ];

    services.puzuz = {
      enable = true;
      admins = [ "carol" ];
    };

    users.users.alice.isNormalUser = true;
    users.users.carol.isNormalUser = true;
    # A program that is only in /run/current-system/sw/bin, for the PATH check.
    environment.systemPackages = [ pkgs.hello ];

    virtualisation.memorySize = 2048;
    virtualisation.cores = 2;

    specialisation.upgraded.configuration.services.puzuz.package = puzuzCopy;
    specialisation.disabled.configuration.services.puzuz.enable = lib.mkForce false;
  };

  testScript = ''
    import json
    import shlex

    COUNTER = "i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done"

    def pz(user, args):
        cmd = "puzuz --color never " + args
        return machine.succeed(f"su -l {user} -c {shlex.quote(cmd)}").strip()

    def add(user, command):
        return pz(user, "add -p -- " + shlex.quote(command))

    def state(job_id):
        jobs = json.loads(pz("carol", "status --json"))["jobs"]
        s = next(j for j in jobs if j["id"] == int(job_id))["state"]
        return "/".join([s["state"]] + ([s["result"]] if "result" in s else []))

    def wait_state(job_id, prefix, timeout=60):
        def check(_):
            s = state(job_id)
            machine.log(f"job {job_id}: {s}")
            return s.startswith(prefix)
        with machine.nested(f"waiting for job {job_id} to be {prefix}"):
            retry(check, timeout)

    def job_log(job_id):
        return pz("alice", f"log --full {job_id}")

    def last_count(job_id):
        lines = [l for l in job_log(job_id).splitlines() if l.strip().isdigit()]
        return int(lines[-1]) if lines else -1

    def wait_counting(job_id, after):
        retry(lambda _: last_count(job_id) > after, 60)

    base = machine.succeed("readlink -f /run/current-system").strip()

    with subtest("daemon and helper run"):
        machine.wait_for_unit("puzuz-helper.service")
        machine.wait_for_unit("puzuz.service")
        machine.wait_for_file("/run/puzuz/puzuz.sock")
        # The helper only reads a regular file owned by root, not a link
        # into the store.
        machine.succeed("test ! -L /etc/puzuz/config.toml")
        machine.succeed("[ $(stat -c %U:%a /etc/puzuz/config.toml) = root:644 ]")
        machine.succeed("grep -q '^criu_path = \"/nix/store/' /etc/puzuz/config.toml")
        assert pz("alice", "service install-method") == "nix"

    with subtest("doctor"):
        out = machine.succeed("puzuz doctor 2>&1 || true")
        print(out)
        # The VM has no GPU, so the NVIDIA driver and cuda-checkpoint checks
        # warn; nothing may fail.
        assert "\n0 failed," in out, "puzuz doctor reports failures"

    with subtest("a user's job runs with the user's PATH"):
        job = add("alice", 'echo "PATH=$PATH"; hello')
        wait_state(job, "done")
        assert state(job) == "done/success", state(job)
        out = job_log(job)
        print(out)
        assert "Hello, world!" in out, out
        assert "/run/wrappers/bin" in out and "/run/current-system/sw/bin" in out, out

    with subtest("checkpoint and restore"):
        job = add("alice", COUNTER)
        wait_state(job, "running")
        wait_counting(job, 5)
        pz("alice", f"checkpoint {job}")
        wait_state(job, "checkpointed", 120)
        before = last_count(job)
        pz("alice", f"restore {job}")
        wait_state(job, "running", 120)
        wait_counting(job, before)
        pz("alice", f"kill {job}")
        wait_state(job, "done")

    with subtest("switching the package keeps jobs running"):
        job = add("alice", COUNTER)
        wait_state(job, "running")
        wait_counting(job, 5)
        machine.succeed(f"{base}/specialisation/upgraded/bin/switch-to-configuration test")
        machine.wait_for_unit("puzuz-helper.service")
        machine.wait_for_unit("puzuz.service")
        machine.succeed(
            "systemctl show -p ExecStart puzuz-helper.service | grep -q ${puzuzCopy}/bin/puzuz-helper"
        )
        machine.succeed(
            "systemctl show -p ExecStart puzuz.service | grep -q ${puzuzCopy}/bin/puzuz"
        )
        wait_state(job, "running")
        wait_counting(job, last_count(job))
        pz("alice", f"kill {job}")
        wait_state(job, "done")

    with subtest("enable = false removes the units"):
        machine.succeed(f"{base}/specialisation/disabled/bin/switch-to-configuration test")
        machine.fail("systemctl cat puzuz.service")
        machine.fail("systemctl cat puzuz-helper.service")
        machine.fail("systemctl is-active puzuz-helper.service")
        machine.succeed("test ! -e /etc/puzuz/config.toml")
  '';
}
