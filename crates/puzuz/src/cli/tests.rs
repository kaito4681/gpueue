use std::path::PathBuf;

use chrono::{Local, TimeDelta, TimeZone};
use clap::{CommandFactory, Parser, error::ErrorKind};

use super::*;

fn parse(args: &[&str]) -> Cli {
    let mut argv = vec!["puzuz"];
    argv.extend_from_slice(args);
    match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(err) => panic!("failed to parse {args:?}: {err}"),
    }
}

fn cmd(args: &[&str]) -> SubCommand {
    parse(args).subcommand()
}

fn parse_err(args: &[&str]) -> ErrorKind {
    let mut argv = vec!["puzuz"];
    argv.extend_from_slice(args);
    match Cli::try_parse_from(&argv) {
        Ok(cli) => panic!("{args:?} unexpectedly parsed as {cli:?}"),
        Err(err) => err.kind(),
    }
}

#[test]
fn command_definition_is_valid() {
    Cli::command().debug_assert();
}

// --- Global options -------------------------------------------------------------------------

#[test]
fn no_subcommand_defaults_to_status() {
    let cli = parse(&[]);
    assert_eq!(cli.cmd, None);
    assert_eq!(cli.subcommand(), SubCommand::default_status());
    assert_eq!(cli.verbose, 0);
    assert_eq!(cli.color, ColorChoice::Auto);
}

#[test]
fn global_options() {
    let cli = parse(&[
        "-vvv",
        "--color",
        "never",
        "--socket",
        "/tmp/x.sock",
        "status",
    ]);
    assert_eq!(cli.verbose, 3);
    assert_eq!(cli.color, ColorChoice::Never);
    assert_eq!(cli.socket, PathBuf::from("/tmp/x.sock"));

    // Global options may also follow the subcommand.
    let cli = parse(&["status", "-v", "--color=always"]);
    assert_eq!(cli.verbose, 1);
    assert_eq!(cli.color, ColorChoice::Always);
}

#[test]
fn socket_default() {
    // PUZUZ_SOCKET may be set in the environment running the tests; only check when it isn't.
    if std::env::var_os("PUZUZ_SOCKET").is_none() {
        assert_eq!(parse(&[]).socket, PathBuf::from(DEFAULT_SOCKET));
    }
}

#[test]
fn pueue_config_and_profile_are_rejected() {
    assert_eq!(
        parse_err(&["-c", "foo.yml", "status"]),
        ErrorKind::UnknownArgument
    );
    assert_eq!(
        parse_err(&["--profile", "x", "status"]),
        ErrorKind::UnknownArgument
    );
}

// --- add ------------------------------------------------------------------------------------

#[test]
fn add_simple_command() {
    let SubCommand::Add {
        command,
        group,
        gpu,
        no_checkpoint,
        after,
        immediate,
        stashed,
        delay,
        ..
    } = cmd(&["add", "ls", "-la"])
    else {
        panic!("expected add");
    };
    assert_eq!(command, ["ls", "-la"]);
    assert_eq!(group, None);
    assert!(gpu.is_empty());
    assert!(!no_checkpoint && !immediate && !stashed);
    assert!(after.is_empty());
    assert_eq!(delay, None);
}

#[test]
fn add_trailing_command_keeps_its_flags() {
    // Everything after the first positional belongs to the command, even puzuz-looking flags.
    let SubCommand::Add { command, label, .. } = cmd(&[
        "add", "-l", "x", "python", "train.py", "-g", "a", "--gpu", "1",
    ]) else {
        panic!("expected add");
    };
    assert_eq!(label.as_deref(), Some("x"));
    assert_eq!(command, ["python", "train.py", "-g", "a", "--gpu", "1"]);

    // Explicit `--` separator.
    let SubCommand::Add { command, .. } = cmd(&["add", "--", "-weird", "--flag"]) else {
        panic!("expected add");
    };
    assert_eq!(command, ["-weird", "--flag"]);
}

#[test]
fn add_requires_a_command() {
    assert_eq!(parse_err(&["add"]), ErrorKind::MissingRequiredArgument);
    assert_eq!(
        parse_err(&["add", "-i"]),
        ErrorKind::MissingRequiredArgument
    );
}

#[test]
fn add_after_takes_multiple_ids() {
    let SubCommand::Add { after, command, .. } = cmd(&["add", "-a", "1", "2", "3", "--", "echo"])
    else {
        panic!("expected add");
    };
    assert_eq!(after, [1, 2, 3]);
    assert_eq!(command, ["echo"]);

    let SubCommand::Add { after, command, .. } =
        cmd(&["add", "--after", "4", "--after", "5", "--", "echo", "hi"])
    else {
        panic!("expected add");
    };
    assert_eq!(after, [4, 5]);
    assert_eq!(command, ["echo", "hi"]);

    // Like pueue, without `--` the command would be swallowed as a dependency id.
    assert_eq!(
        parse_err(&["add", "-a", "1", "echo"]),
        ErrorKind::ValueValidation
    );
}

#[test]
fn add_all_pueue_flags() {
    let SubCommand::Add {
        command,
        working_directory,
        escape,
        immediate,
        follow,
        stashed,
        group,
        priority,
        label,
        print_task_id,
        ..
    } = cmd(&[
        "add", "-w", "/tmp", "-e", "-i", "--follow", "-g", "gpu", "-o", "-3", "-l", "lbl", "make",
    ])
    else {
        panic!("expected add");
    };
    assert_eq!(command, ["make"]);
    assert_eq!(working_directory, Some(PathBuf::from("/tmp")));
    assert!(escape && immediate && follow && !stashed && !print_task_id);
    assert_eq!(group.as_deref(), Some("gpu"));
    assert_eq!(priority, Some(-3));
    assert_eq!(label.as_deref(), Some("lbl"));

    let SubCommand::Add {
        stashed,
        print_task_id,
        ..
    } = cmd(&["add", "-s", "-p", "true"])
    else {
        panic!("expected add");
    };
    assert!(stashed && print_task_id);
}

#[test]
fn add_flag_conflicts() {
    assert_eq!(
        parse_err(&["add", "-i", "-s", "true"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse_err(&["add", "-i", "-d", "10", "true"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse_err(&["add", "--follow", "true"]),
        ErrorKind::MissingRequiredArgument
    );
    assert_eq!(
        parse_err(&["add", "-i", "--follow", "-p", "true"]),
        ErrorKind::ArgumentConflict
    );
}

#[test]
fn add_gpu_and_no_checkpoint() {
    let SubCommand::Add {
        gpu,
        no_checkpoint,
        command,
        ..
    } = cmd(&[
        "add",
        "--gpu",
        "0,1",
        "--no-checkpoint",
        "python",
        "train.py",
    ])
    else {
        panic!("expected add");
    };
    assert_eq!(gpu, [0, 1]);
    assert!(no_checkpoint);
    assert_eq!(command, ["python", "train.py"]);

    let SubCommand::Add { gpu, .. } = cmd(&["add", "--gpu", "2", "--gpu=3", "x"]) else {
        panic!("expected add");
    };
    assert_eq!(gpu, [2, 3]);

    assert_eq!(
        parse_err(&["add", "--gpu", "a", "x"]),
        ErrorKind::ValueValidation
    );
}

#[test]
fn add_delay_seconds_and_expressions() {
    let before = Local::now();
    let SubCommand::Add { delay, .. } = cmd(&["add", "-d", "60", "x"]) else {
        panic!("expected add");
    };
    let delay = delay.expect("delay");
    assert!(delay >= before + TimeDelta::seconds(60));
    assert!(delay <= Local::now() + TimeDelta::seconds(60));

    let SubCommand::Add { delay, .. } = cmd(&["add", "--delay", "3h", "x"]) else {
        panic!("expected add");
    };
    assert!(delay.expect("delay") > before + TimeDelta::minutes(179));

    assert_eq!(
        parse_err(&["add", "--delay", "not a date", "x"]),
        ErrorKind::ValueValidation
    );
}

#[test]
fn delay_parser() {
    let now = Local.with_ymd_and_hms(2024, 1, 10, 12, 0, 0).unwrap();
    assert_eq!(
        parse_delay_until_from("90", now).unwrap(),
        now + TimeDelta::seconds(90)
    );
    assert_eq!(
        parse_delay_until_from("3600s", now).unwrap(),
        now + TimeDelta::hours(1)
    );
    assert_eq!(
        parse_delay_until_from("2024-04-01T18:30:00", now).unwrap(),
        Local.with_ymd_and_hms(2024, 4, 1, 18, 30, 0).unwrap()
    );
    assert_eq!(
        parse_delay_until_from("1 week", now).unwrap(),
        now + TimeDelta::weeks(1)
    );
    assert!(parse_delay_until_from("tomorrowish", now).is_err());
    assert!(parse_delay_until_from(&i64::MAX.to_string(), now).is_err());
}

// --- remove / switch / stash / enqueue ------------------------------------------------------

#[test]
fn remove_requires_ids_and_has_rm_alias() {
    assert_eq!(
        cmd(&["remove", "1", "2", "3"]),
        SubCommand::Remove {
            task_ids: vec![1, 2, 3]
        }
    );
    assert_eq!(cmd(&["rm", "4"]), SubCommand::Remove { task_ids: vec![4] });
    assert_eq!(parse_err(&["remove"]), ErrorKind::MissingRequiredArgument);
    assert_eq!(parse_err(&["remove", "x"]), ErrorKind::ValueValidation);
}

#[test]
fn switch_takes_two_ids() {
    assert_eq!(
        cmd(&["switch", "1", "2"]),
        SubCommand::Switch {
            task_id_1: 1,
            task_id_2: 2
        }
    );
    assert_eq!(
        parse_err(&["switch", "1"]),
        ErrorKind::MissingRequiredArgument
    );
}

#[test]
fn stash_selection() {
    assert_eq!(
        cmd(&["stash", "1", "2"]),
        SubCommand::Stash {
            task_ids: vec![1, 2],
            group: None,
            all: false,
            delay: None
        }
    );
    let SubCommand::Stash { group, all, .. } = cmd(&["stash", "-g", "g1"]) else {
        panic!("expected stash");
    };
    assert_eq!(group.as_deref(), Some("g1"));
    assert!(!all);
    let SubCommand::Stash { all, delay, .. } = cmd(&["stash", "-a", "-d", "5"]) else {
        panic!("expected stash");
    };
    assert!(all);
    assert!(delay.is_some());
    assert_eq!(
        parse_err(&["stash", "-a", "-g", "x"]),
        ErrorKind::ArgumentConflict
    );
}

#[test]
fn enqueue_selection() {
    // No arguments means "the default group"; the command layer resolves that.
    assert_eq!(
        cmd(&["enqueue"]),
        SubCommand::Enqueue {
            task_ids: vec![],
            group: None,
            all: false,
            delay: None
        }
    );
    let SubCommand::Enqueue {
        task_ids, delay, ..
    } = cmd(&["enqueue", "3", "4", "--delay", "wednesday 10:30pm"])
    else {
        panic!("expected enqueue");
    };
    assert_eq!(task_ids, [3, 4]);
    assert!(delay.is_some());
    assert_eq!(
        parse_err(&["enqueue", "-g", "x", "-a"]),
        ErrorKind::ArgumentConflict
    );
}

// --- start / restart / pause / kill ---------------------------------------------------------

#[test]
fn start_selection() {
    assert_eq!(
        cmd(&["start"]),
        SubCommand::Start {
            task_ids: vec![],
            group: None,
            all: false
        }
    );
    assert_eq!(
        cmd(&["start", "-g", "gpu"]),
        SubCommand::Start {
            task_ids: vec![],
            group: Some("gpu".into()),
            all: false
        }
    );
    assert_eq!(
        cmd(&["start", "--all"]),
        SubCommand::Start {
            task_ids: vec![],
            group: None,
            all: true
        }
    );
    assert_eq!(
        cmd(&["start", "5", "6"]),
        SubCommand::Start {
            task_ids: vec![5, 6],
            group: None,
            all: false
        }
    );
}

#[test]
fn restart_flags() {
    let SubCommand::Restart {
        task_ids,
        in_place,
        immediate,
        stashed,
        not_in_place,
        edit,
        all_failed,
        failed_in_group,
    } = cmd(&["re", "1", "2", "-i", "-k"])
    else {
        panic!("expected restart");
    };
    assert_eq!(task_ids, [1, 2]);
    assert!(in_place && immediate);
    assert!(!stashed && !not_in_place && !edit && !all_failed);
    assert_eq!(failed_in_group, None);

    let SubCommand::Restart {
        in_place,
        stashed,
        all_failed,
        edit,
        ..
    } = cmd(&["restart", "--in-place", "-s", "-a", "-e"])
    else {
        panic!("expected restart");
    };
    assert!(in_place && stashed && all_failed && edit);

    let SubCommand::Restart {
        failed_in_group, ..
    } = cmd(&["restart", "-g", "train"])
    else {
        panic!("expected restart");
    };
    assert_eq!(failed_in_group.as_deref(), Some("train"));

    assert_eq!(
        parse_err(&["restart", "-a", "-g", "x"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse_err(&["restart", "-k", "-s", "1"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse_err(&["restart", "-i", "--not-in-place", "1"]),
        ErrorKind::ArgumentConflict
    );
}

#[test]
fn pause_selection() {
    assert_eq!(
        cmd(&["pause", "-g", "a", "-w"]),
        SubCommand::Pause {
            task_ids: vec![],
            group: Some("a".into()),
            all: false,
            wait: true
        }
    );
    assert_eq!(
        cmd(&["pause", "7"]),
        SubCommand::Pause {
            task_ids: vec![7],
            group: None,
            all: false,
            wait: false
        }
    );
    assert_eq!(
        parse_err(&["pause", "-a", "-g", "x"]),
        ErrorKind::ArgumentConflict
    );
}

#[test]
fn kill_with_signal() {
    assert_eq!(
        cmd(&["kill", "1", "2", "-s", "SIGINT"]),
        SubCommand::Kill {
            task_ids: vec![1, 2],
            group: None,
            all: false,
            signal: Some(Signal::SigInt)
        }
    );
    for spelling in ["sigterm", "term", "TERM", "SigTerm", "15"] {
        let SubCommand::Kill { signal, .. } = cmd(&["kill", "-a", "--signal", spelling]) else {
            panic!("expected kill");
        };
        assert_eq!(signal, Some(Signal::SigTerm), "spelling {spelling}");
    }
    assert_eq!(
        parse_err(&["kill", "-s", "SIGFOO", "1"]),
        ErrorKind::ValueValidation
    );
    assert_eq!(
        parse_err(&["kill", "-s", "64", "1"]),
        ErrorKind::ValueValidation
    );
    assert_eq!(
        parse_err(&["kill", "-a", "-g", "x"]),
        ErrorKind::ArgumentConflict
    );
}

#[test]
fn signal_round_trip() {
    for signal in Signal::ALL {
        assert_eq!(signal.to_string().parse::<Signal>(), Ok(signal));
        assert_eq!(signal.number().to_string().parse::<Signal>(), Ok(signal));
    }
    assert_eq!(Signal::SigKill.number(), 9);
}

// --- send / edit / env ----------------------------------------------------------------------

#[test]
fn send_edit_env_parse() {
    assert_eq!(
        cmd(&["send", "1", "y\n"]),
        SubCommand::Send {
            task_id: 1,
            input: "y\n".into()
        }
    );
    assert_eq!(
        cmd(&["edit", "1", "2"]),
        SubCommand::Edit {
            task_ids: vec![1, 2]
        }
    );
    assert_eq!(
        cmd(&["env", "set", "3", "KEY", "value"]),
        SubCommand::Env {
            cmd: EnvCommand::Set {
                task_id: 3,
                key: "KEY".into(),
                value: "value".into()
            }
        }
    );
    assert_eq!(
        cmd(&["env", "unset", "3", "KEY"]),
        SubCommand::Env {
            cmd: EnvCommand::Unset {
                task_id: 3,
                key: "KEY".into()
            }
        }
    );
    assert_eq!(
        parse_err(&["env"]),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
}

// --- group / parallel -----------------------------------------------------------------------

#[test]
fn group_subcommands() {
    assert_eq!(
        cmd(&["group"]),
        SubCommand::Group {
            json: false,
            cmd: None
        }
    );
    assert_eq!(
        cmd(&["group", "-j"]),
        SubCommand::Group {
            json: true,
            cmd: None
        }
    );
    assert_eq!(
        cmd(&["group", "add", "train", "-p", "2"]),
        SubCommand::Group {
            json: false,
            cmd: Some(GroupCommand::Add {
                name: "train".into(),
                parallel: Some(2)
            })
        }
    );
    assert_eq!(
        cmd(&["group", "remove", "train"]),
        SubCommand::Group {
            json: false,
            cmd: Some(GroupCommand::Remove {
                name: "train".into()
            })
        }
    );
}

#[test]
fn parallel_defaults_to_default_group() {
    assert_eq!(
        cmd(&["parallel", "4"]),
        SubCommand::Parallel {
            parallel_tasks: Some(4),
            group: None
        }
    );
    assert_eq!(
        cmd(&["parallel", "0", "-g", "x"]),
        SubCommand::Parallel {
            parallel_tasks: Some(0),
            group: Some("x".into())
        }
    );
    assert_eq!(
        cmd(&["parallel"]),
        SubCommand::Parallel {
            parallel_tasks: None,
            group: None
        }
    );
}

// --- status / log / follow / wait -----------------------------------------------------------

#[test]
fn status_query_tokens_are_kept_verbatim() {
    assert_eq!(
        cmd(&["status", "-j", "-g", "train"]),
        SubCommand::Status {
            query: vec![],
            json: true,
            group: Some("train".into())
        }
    );
    let SubCommand::Status { query, .. } = cmd(&[
        "status",
        "columns=id,status,command",
        "status=running",
        "start",
        ">",
        "2023-05-2112:03:17",
        "order_by",
        "command",
        "first",
        "5",
    ]) else {
        panic!("expected status");
    };
    assert_eq!(
        query,
        [
            "columns=id,status,command",
            "status=running",
            "start",
            ">",
            "2023-05-2112:03:17",
            "order_by",
            "command",
            "first",
            "5"
        ]
    );
}

#[test]
fn log_options() {
    assert_eq!(
        cmd(&["log", "1", "2", "--lines", "20"]),
        SubCommand::Log {
            task_ids: vec![1, 2],
            group: None,
            all: false,
            json: false,
            lines: Some(20),
            full: false
        }
    );
    assert_eq!(
        cmd(&["log", "-a", "-j", "-f"]),
        SubCommand::Log {
            task_ids: vec![],
            group: None,
            all: true,
            json: true,
            lines: None,
            full: true
        }
    );
    let SubCommand::Log { group, .. } = cmd(&["log", "-g", "g"]) else {
        panic!("expected log");
    };
    assert_eq!(group.as_deref(), Some("g"));
    assert_eq!(
        parse_err(&["log", "-l", "5", "-f"]),
        ErrorKind::ArgumentConflict
    );
}

#[test]
fn follow_optional_id_and_alias() {
    assert_eq!(
        cmd(&["follow"]),
        SubCommand::Follow {
            task_id: None,
            lines: None
        }
    );
    assert_eq!(
        cmd(&["fo", "3", "-l", "10"]),
        SubCommand::Follow {
            task_id: Some(3),
            lines: Some(10)
        }
    );
    assert_eq!(parse_err(&["follow", "1", "2"]), ErrorKind::UnknownArgument);
}

#[test]
fn wait_options() {
    assert_eq!(
        cmd(&["wait"]),
        SubCommand::Wait {
            task_ids: vec![],
            group: None,
            all: false,
            quiet: false,
            status: None
        }
    );
    assert_eq!(
        cmd(&["wait", "1", "2", "-q", "-s", "Success"]),
        SubCommand::Wait {
            task_ids: vec![1, 2],
            group: None,
            all: false,
            quiet: true,
            status: Some(WaitTargetStatus::Success)
        }
    );
    let SubCommand::Wait { status, all, .. } = cmd(&["wait", "-a", "--status", "running"]) else {
        panic!("expected wait");
    };
    assert_eq!(status, Some(WaitTargetStatus::Running));
    assert!(all);
    assert_eq!(parse_err(&["wait", "-s", "nope"]), ErrorKind::InvalidValue);
    assert_eq!(
        parse_err(&["wait", "-a", "-g", "x"]),
        ErrorKind::ArgumentConflict
    );
}

// --- clean / reset / shutdown ---------------------------------------------------------------

#[test]
fn clean_and_aliases() {
    for name in ["clean", "cleanup", "clear"] {
        assert_eq!(
            cmd(&[name, "-s", "-g", "x"]),
            SubCommand::Clean {
                successful_only: true,
                group: Some("x".into())
            }
        );
    }
}

#[test]
fn reset_groups_and_force() {
    assert_eq!(
        cmd(&["reset"]),
        SubCommand::Reset {
            groups: vec![],
            force: false
        }
    );
    assert_eq!(
        cmd(&["reset", "-g", "a", "--groups", "b", "-f"]),
        SubCommand::Reset {
            groups: vec!["a".into(), "b".into()],
            force: true
        }
    );
}

#[test]
fn shutdown_checkpoints_by_default() {
    assert_eq!(
        cmd(&["shutdown"]),
        SubCommand::Shutdown {
            no_checkpoint: false
        }
    );
    assert_eq!(
        cmd(&["shutdown", "--no-checkpoint"]),
        SubCommand::Shutdown {
            no_checkpoint: true
        }
    );
}

// --- completions ----------------------------------------------------------------------------

#[test]
fn completions_arguments() {
    assert_eq!(
        cmd(&["completions", "zsh"]),
        SubCommand::Completions {
            shell: Shell::Zsh,
            output_directory: None
        }
    );
    assert_eq!(
        cmd(&["completions", "power-shell", "/tmp/out"]),
        SubCommand::Completions {
            shell: Shell::PowerShell,
            output_directory: Some(PathBuf::from("/tmp/out"))
        }
    );
    assert_eq!(
        cmd(&["completions", "nushell"]),
        SubCommand::Completions {
            shell: Shell::Nushell,
            output_directory: None
        }
    );
    assert_eq!(parse_err(&["completions", "tcsh"]), ErrorKind::InvalidValue);
}

#[test]
fn completion_scripts_can_be_generated() {
    use clap_complete::{generate, shells};

    let mut command = Cli::command();
    let mut out = Vec::new();
    generate(shells::Bash, &mut command, "puzuz", &mut out);
    let script = String::from_utf8(out).expect("utf-8");
    assert!(script.contains("checkpoint"));
    assert!(script.contains("--gpu"));

    let mut out = Vec::new();
    generate(
        clap_complete_nushell::Nushell,
        &mut command,
        "puzuz",
        &mut out,
    );
    assert!(!out.is_empty());
}

// --- puzuz extensions -----------------------------------------------------------------------

#[test]
fn checkpoint_selection() {
    assert_eq!(
        cmd(&["checkpoint", "1", "2"]),
        SubCommand::Checkpoint {
            task_ids: vec![1, 2],
            group: None,
            all: false
        }
    );
    assert_eq!(
        cmd(&["checkpoint", "-g", "train"]),
        SubCommand::Checkpoint {
            task_ids: vec![],
            group: Some("train".into()),
            all: false
        }
    );
    assert_eq!(
        cmd(&["checkpoint", "-a"]),
        SubCommand::Checkpoint {
            task_ids: vec![],
            group: None,
            all: true
        }
    );
    assert_eq!(
        parse_err(&["checkpoint", "-a", "-g", "x"]),
        ErrorKind::ArgumentConflict
    );
}

#[test]
fn restore_selection_and_immediate() {
    assert_eq!(
        cmd(&["restore", "4", "-i"]),
        SubCommand::Restore {
            task_ids: vec![4],
            group: None,
            all: false,
            immediate: true
        }
    );
    assert_eq!(
        cmd(&["restore", "--all", "--immediate"]),
        SubCommand::Restore {
            task_ids: vec![],
            group: None,
            all: true,
            immediate: true
        }
    );
    assert_eq!(
        cmd(&["restore", "-g", "x"]),
        SubCommand::Restore {
            task_ids: vec![],
            group: Some("x".into()),
            all: false,
            immediate: false
        }
    );
}

#[test]
fn gpu_list_and_parallel() {
    assert_eq!(
        cmd(&["gpu"]),
        SubCommand::Gpu {
            json: false,
            cmd: None
        }
    );
    assert_eq!(
        cmd(&["gpu", "-j"]),
        SubCommand::Gpu {
            json: true,
            cmd: None
        }
    );
    assert_eq!(
        cmd(&["gpu", "parallel", "2"]),
        SubCommand::Gpu {
            json: false,
            cmd: Some(GpuCommand::Parallel {
                parallel_jobs: 2,
                gpu: vec![]
            })
        }
    );
    assert_eq!(
        cmd(&["gpu", "parallel", "3", "--gpu", "0,2"]),
        SubCommand::Gpu {
            json: false,
            cmd: Some(GpuCommand::Parallel {
                parallel_jobs: 3,
                gpu: vec![0, 2]
            })
        }
    );
    assert_eq!(
        parse_err(&["gpu", "parallel"]),
        ErrorKind::MissingRequiredArgument
    );
}

#[test]
fn daemon_defaults_and_overrides() {
    assert_eq!(
        cmd(&["daemon"]),
        SubCommand::Daemon {
            config: PathBuf::from("/etc/puzuz/config.toml"),
            data_dir: None,
            helper_socket: None,
        }
    );
    let cli = parse(&[
        "--socket",
        "/tmp/t/puzuz.sock",
        "daemon",
        "--config",
        "/tmp/t/config.toml",
        "--data-dir",
        "/tmp/t/data",
        "--helper-socket",
        "/tmp/t/helper.sock",
    ]);
    assert_eq!(cli.socket, PathBuf::from("/tmp/t/puzuz.sock"));
    assert_eq!(
        cli.subcommand(),
        SubCommand::Daemon {
            config: PathBuf::from("/tmp/t/config.toml"),
            data_dir: Some(PathBuf::from("/tmp/t/data")),
            helper_socket: Some(PathBuf::from("/tmp/t/helper.sock")),
        }
    );
    // puzuz never daemonizes itself.
    assert_eq!(parse_err(&["daemon", "-d"]), ErrorKind::UnknownArgument);
}

#[test]
fn service_subcommands() {
    assert_eq!(
        cmd(&["service", "install"]),
        SubCommand::Service {
            cmd: ServiceCommand::Install {
                no_start: false,
                bin_dir: None
            }
        }
    );
    assert_eq!(
        cmd(&[
            "service",
            "install",
            "--no-start",
            "--bin-dir",
            "/usr/local/bin"
        ]),
        SubCommand::Service {
            cmd: ServiceCommand::Install {
                no_start: true,
                bin_dir: Some(PathBuf::from("/usr/local/bin"))
            }
        }
    );
    assert_eq!(
        cmd(&["service", "uninstall"]),
        SubCommand::Service {
            cmd: ServiceCommand::Uninstall {
                purge: false,
                yes: false
            }
        }
    );
    assert_eq!(
        cmd(&["service", "uninstall", "--purge", "-y"]),
        SubCommand::Service {
            cmd: ServiceCommand::Uninstall {
                purge: true,
                yes: true
            }
        }
    );
    assert_eq!(
        cmd(&["service", "status"]),
        SubCommand::Service {
            cmd: ServiceCommand::Status
        }
    );
    assert_eq!(
        parse_err(&["service"]),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
}

#[test]
fn doctor() {
    assert_eq!(cmd(&["doctor"]), SubCommand::Doctor);
}

#[test]
fn subcommand_names_match_clap() {
    let command = Cli::command();
    let samples: [&[&str]; 28] = [
        &["add", "x"],
        &["remove", "1"],
        &["switch", "1", "2"],
        &["stash"],
        &["enqueue"],
        &["start"],
        &["restart"],
        &["pause"],
        &["kill"],
        &["send", "1", "y"],
        &["edit"],
        &["env", "unset", "1", "K"],
        &["group"],
        &["status"],
        &["log"],
        &["follow"],
        &["wait"],
        &["clean"],
        &["reset"],
        &["shutdown"],
        &["parallel"],
        &["completions", "bash"],
        &["checkpoint"],
        &["restore"],
        &["gpu"],
        &["daemon"],
        &["service", "status"],
        &["doctor"],
    ];
    assert_eq!(samples.len(), command.get_subcommands().count());
    for args in samples {
        let name = cmd(args).name();
        assert_eq!(name, args[0]);
        assert!(command.find_subcommand(name).is_some());
    }
}
