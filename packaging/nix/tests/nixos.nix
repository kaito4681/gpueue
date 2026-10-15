# NixOS VM test of services.gpueue (`nix build .#checks.x86_64-linux.nixos`).
#
# Boots a machine with the module, then:
#   1. the daemon and the helper run, /etc/gpueue/config.toml is a root-owned
#      copy, and `gpueue doctor` reports no failure (only GPU warnings);
#   2. a normal user's job runs with the user's PATH and finishes;
#   3. a job is checkpointed with CRIU and restored, and keeps counting;
#   4. a checkpoint keeps the helper that ran the job: a job started by a
#      helper from a store path that no system generation refers to is
#      checkpointed, the helper goes back to the system's package,
#      `nix-collect-garbage -d` runs, and the job is restored (the helper's
#      GC root kept that store path; once the job is gone, GC deletes it);
#   5. switching to another package (a specialisation) restarts the daemon
#      and the helper from the new store paths while a job keeps running;
#   6. `enable = false` (another specialisation) removes the units.
{ self, pkgs }:
let
  inherit (pkgs) lib;
  gpueue = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  # The same binaries at another store path: to NixOS this is an upgrade
  # (every ExecStart= changes) without building gpueue twice.
  gpueueCopy = pkgs.runCommand "gpueue-copy-${gpueue.version}" { } ''
    cp -a ${gpueue} "$out"
  '';
  # Another copy that is in the VM's store but in no system closure (a
  # specialisation is part of the booted system, so GC never deletes it):
  # only a GC root of the helper keeps it.
  gpueueOld = pkgs.runCommand "gpueue-old-${gpueue.version}" { } ''
    cp -a ${gpueue} "$out"
  '';
in
pkgs.testers.runNixOSTest {
  name = "gpueue";

  nodes.machine = {
    imports = [ self.nixosModules.default ];

    services.gpueue = {
      enable = true;
      admins = [ "carol" ];
    };

    users.users.alice.isNormalUser = true;
    users.users.carol.isNormalUser = true;
    # A program that is only in /run/current-system/sw/bin, for the PATH check.
    environment.systemPackages = [ pkgs.hello ];

    virtualisation.memorySize = 2048;
    virtualisation.cores = 2;
    virtualisation.additionalPaths = [ gpueueOld ];

    specialisation.upgraded.configuration.services.gpueue.package = gpueueCopy;
    specialisation.disabled.configuration.services.gpueue.enable = lib.mkForce false;
  };

  testScript = ''
    import json
    import shlex

    COUNTER = "i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done"

    def pz(user, args):
        cmd = "gpueue --color never " + args
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
        machine.wait_for_unit("gpueue-helper.service")
        machine.wait_for_unit("gpueue.service")
        machine.wait_for_file("/run/gpueue/gpueue.sock")
        # The helper only reads a regular file owned by root, not a link
        # into the store.
        machine.succeed("test ! -L /etc/gpueue/config.toml")
        machine.succeed("[ $(stat -c %U:%a /etc/gpueue/config.toml) = root:644 ]")
        machine.succeed("grep -q '^criu_path = \"/nix/store/' /etc/gpueue/config.toml")
        assert pz("alice", "service install-method") == "nix"

    with subtest("doctor"):
        out = machine.succeed("gpueue doctor 2>&1 || true")
        print(out)
        # The VM has no GPU, so the NVIDIA driver and cuda-checkpoint checks
        # warn; nothing may fail.
        assert "\n0 failed," in out, "gpueue doctor reports failures"

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

    with subtest("a checkpoint keeps the helper that ran the job across GC"):
        dropin = "/run/systemd/system/gpueue-helper.service.d"

        def restart_helper():
            machine.succeed("systemctl daemon-reload")
            machine.succeed("systemctl restart gpueue-helper.service")
            machine.wait_for_unit("gpueue-helper.service")
            retry(lambda _: "helper: connected" in machine.succeed("gpueue doctor 2>&1 || true"), 60)

        machine.succeed(f"mkdir -p {dropin}")
        machine.succeed(
            f"printf '[Service]\\nExecStart=\\nExecStart=${gpueueOld}/bin/gpueue-helper serve\\n'"
            f" > {dropin}/old.conf"
        )
        restart_helper()
        job = add("alice", COUNTER)
        wait_state(job, "running")
        wait_counting(job, 5)
        root = f"/nix/var/nix/gcroots/gpueue/job-{job}"
        assert machine.succeed(f"readlink {root}").strip() == "${gpueueOld}"
        pz("alice", f"checkpoint {job}")
        wait_state(job, "checkpointed", 120)
        before = last_count(job)
        # Back to the system's helper, then collect everything unrooted.
        machine.succeed(f"rm -r {dropin}")
        restart_helper()
        machine.succeed("nix-collect-garbage -d")
        machine.succeed("test -x ${gpueueOld}/bin/gpueue-helper")
        pz("alice", f"restore {job}")
        wait_state(job, "running", 120)
        wait_counting(job, before)
        pz("alice", f"kill {job}")
        wait_state(job, "done")
        # Gone with the job: now GC may delete the old helper.
        machine.succeed(f"test ! -L {root}")
        machine.succeed("nix-collect-garbage -d")
        machine.succeed("test ! -e ${gpueueOld}")

    with subtest("switching the package keeps jobs running"):
        job = add("alice", COUNTER)
        wait_state(job, "running")
        wait_counting(job, 5)
        machine.succeed(f"{base}/specialisation/upgraded/bin/switch-to-configuration test")
        machine.wait_for_unit("gpueue-helper.service")
        machine.wait_for_unit("gpueue.service")
        machine.succeed(
            "systemctl show -p ExecStart gpueue-helper.service | grep -q ${gpueueCopy}/bin/gpueue-helper"
        )
        machine.succeed(
            "systemctl show -p ExecStart gpueue.service | grep -q ${gpueueCopy}/bin/gpueue"
        )
        wait_state(job, "running")
        wait_counting(job, last_count(job))
        pz("alice", f"kill {job}")
        wait_state(job, "done")

    with subtest("enable = false removes the units"):
        machine.succeed(f"{base}/specialisation/disabled/bin/switch-to-configuration test")
        machine.fail("systemctl cat gpueue.service")
        machine.fail("systemctl cat gpueue-helper.service")
        machine.fail("systemctl is-active gpueue-helper.service")
        machine.succeed("test ! -e /etc/gpueue/config.toml")
  '';
}
