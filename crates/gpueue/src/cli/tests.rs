use std::path::PathBuf;

use chrono::{Local, TimeDelta, TimeZone};
use clap::{CommandFactory, FromArgMatches, Parser, error::ErrorKind};

use super::*;

/// Parses `args` like the real parser, but without reading `GPUEUE_SOCKET` from the process
/// environment, so the tests give the same results wherever they run.
fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
    let mut argv = vec!["gpueue"];
    argv.extend_from_slice(args);
    let command = Cli::command().mut_arg("socket", |arg| arg.env(None::<&str>));
    let mut matches = command.try_get_matches_from(argv)?;
    Cli::from_arg_matches_mut(&mut matches)
}

fn parse(args: &[&str]) -> Cli {
    match try_parse(args) {
        Ok(cli) => cli,
        Err(err) => panic!("failed to parse {args:?}: {err}"),
    }
}

fn cmd(args: &[&str]) -> SubCommand {
    parse(args).subcommand()
}

fn parse_err(args: &[&str]) -> ErrorKind {
    match try_parse(args) {
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
    assert_eq!(parse(&[]).socket, PathBuf::from(DEFAULT_SOCKET));
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
    // Everything after the first positional belongs to the command, even gpueue-looking flags.
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
            force: false,
            keep_checkpoints: false
        }
    );
    assert_eq!(
        cmd(&[
            "reset",
            "-g",
            "a",
            "--groups",
            "b",
            "-f",
            "--keep-checkpoints"
        ]),
        SubCommand::Reset {
            groups: vec!["a".into(), "b".into()],
            force: true,
            keep_checkpoints: true
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
    generate(shells::Bash, &mut command, "gpueue", &mut out);
    let script = String::from_utf8(out).expect("utf-8");
    assert!(script.contains("checkpoint"));
    assert!(script.contains("--gpu"));

    let mut out = Vec::new();
    generate(
        clap_complete_nushell::Nushell,
        &mut command,
        "gpueue",
        &mut out,
    );
    assert!(!out.is_empty());
}

// --- gpueue extensions -----------------------------------------------------------------------

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
            config: PathBuf::from("/etc/gpueue/config.toml"),
            data_dir: None,
            helper_socket: None,
        }
    );
    let cli = parse(&[
        "--socket",
        "/tmp/t/gpueue.sock",
        "daemon",
        "--config",
        "/tmp/t/config.toml",
        "--data-dir",
        "/tmp/t/data",
        "--helper-socket",
        "/tmp/t/helper.sock",
    ]);
    assert_eq!(cli.socket, PathBuf::from("/tmp/t/gpueue.sock"));
    assert_eq!(
        cli.subcommand(),
        SubCommand::Daemon {
            config: PathBuf::from("/tmp/t/config.toml"),
            data_dir: Some(PathBuf::from("/tmp/t/data")),
            helper_socket: Some(PathBuf::from("/tmp/t/helper.sock")),
        }
    );
    // gpueue never daemonizes itself.
    assert_eq!(parse_err(&["daemon", "-d"]), ErrorKind::UnknownArgument);
}

#[test]
fn service_subcommands() {
    assert_eq!(
        cmd(&["service", "install"]),
        SubCommand::Service {
            cmd: ServiceCommand::Install {
                no_start: false,
                bin_dir: None,
                force: false
            }
        }
    );
    assert_eq!(
        cmd(&[
            "service",
            "install",
            "--no-start",
            "--bin-dir",
            "/usr/local/bin",
            "--force"
        ]),
        SubCommand::Service {
            cmd: ServiceCommand::Install {
                no_start: true,
                bin_dir: Some(PathBuf::from("/usr/local/bin")),
                force: true
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
    // Hidden, for install.sh.
    assert_eq!(
        cmd(&["service", "install-method"]),
        SubCommand::Service {
            cmd: ServiceCommand::InstallMethod
        }
    );
    assert_eq!(
        parse_err(&["service"]),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
}

#[test]
fn uninstall() {
    assert_eq!(
        cmd(&["uninstall"]),
        SubCommand::Uninstall {
            purge: false,
            yes: false
        }
    );
    assert_eq!(
        cmd(&["uninstall", "--purge", "--yes"]),
        SubCommand::Uninstall {
            purge: true,
            yes: true
        }
    );
    assert_eq!(
        cmd(&["uninstall", "-y"]),
        SubCommand::Uninstall {
            purge: false,
            yes: true
        }
    );
    // No pueue subcommand or alias starts with it, so nothing else is shadowed.
    assert_eq!(parse_err(&["uninstall", "1"]), ErrorKind::UnknownArgument);
}

#[test]
fn doctor() {
    assert_eq!(cmd(&["doctor"]), SubCommand::Doctor);
}

#[test]
fn subcommand_names_match_clap() {
    let command = Cli::command();
    let samples: [&[&str]; 35] = [
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
        &["prio"],
        &["nice", "1", "2"],
        &["top", "1"],
        &["site", "1", "2"],
        &["share"],
        &["daemon"],
        &["service", "status"],
        &["uninstall"],
        &["doctor"],
        &["update"],
    ];
    assert_eq!(samples.len(), command.get_subcommands().count());
    for args in samples {
        let name = cmd(args).name();
        assert_eq!(name, args[0]);
        assert!(command.find_subcommand(name).is_some());
    }
}

// --- Examples in help texts -----------------------------------------------------------------

/// The subcommand at `path` (e.g. `["gpu", "parallel"]`).
fn subcommand_at(path: &[impl AsRef<str>]) -> clap::Command {
    let mut command = Cli::command();
    for name in path {
        command = command
            .find_subcommand(name.as_ref())
            .unwrap_or_else(|| panic!("no subcommand {}", name.as_ref()))
            .clone();
    }
    command
}

/// Every help text of `command` and its subcommands, with the subcommand path.
fn collect_help_texts(
    command: &clap::Command,
    path: &[String],
    out: &mut Vec<(Vec<String>, String)>,
) {
    let texts = [
        command.get_about(),
        command.get_long_about(),
        command.get_after_help(),
        command.get_after_long_help(),
    ];
    let arg_texts = command
        .get_arguments()
        .flat_map(|arg| [arg.get_help(), arg.get_long_help()]);
    for text in texts.into_iter().chain(arg_texts).flatten() {
        out.push((path.to_vec(), text.to_string()));
    }
    for sub in command.get_subcommands() {
        let mut sub_path = path.to_vec();
        sub_path.push(sub.get_name().to_owned());
        collect_help_texts(sub, &sub_path, out);
    }
}

fn all_help_texts() -> Vec<(Vec<String>, String)> {
    let mut out = Vec::new();
    collect_help_texts(&Cli::command(), &[], &mut out);
    out
}

/// The `quoted` snippets of a text.
fn backticked(text: &str) -> Vec<&str> {
    text.split('`').skip(1).step_by(2).collect()
}

/// The quoted snippets after "e.g." in a help text, up to the end of the line.
fn examples_after_eg(text: &str) -> Vec<&str> {
    text.split("e.g. ")
        .skip(1)
        .flat_map(|rest| {
            let line = rest.split('\n').next().unwrap();
            backticked(line.split(". ").next().unwrap())
        })
        .collect()
}

/// The short and long help of one argument of the subcommand at `path`.
fn arg_help(path: &[&str], arg: &str) -> String {
    let command = subcommand_at(path);
    let arg = command
        .get_arguments()
        .find(|a| a.get_id() == arg)
        .unwrap_or_else(|| panic!("no argument {arg} in {path:?}"));
    let help = arg.get_help().map(ToString::to_string).unwrap_or_default();
    let long = arg
        .get_long_help()
        .map(ToString::to_string)
        .unwrap_or_default();
    format!("{help}\n{long}")
}

/// Splits a shell command line with simple single/double quoting.
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote = None;
    for c in line.chars() {
        match (quote, c) {
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_default();
            }
            (Some(q), c) if c == q => quote = None,
            (None, ' ') => words.extend(word.take()),
            (_, c) => word.get_or_insert_default().push(c),
        }
    }
    assert_eq!(quote, None, "unbalanced quotes in {line}");
    words.extend(word);
    words
}

#[test]
fn shell_words_handles_quotes() {
    assert_eq!(
        shell_words(r#"gpueue add 'ls $HOME && echo "Some string"'"#),
        ["gpueue", "add", r#"ls $HOME && echo "Some string""#]
    );
    assert_eq!(shell_words("a  \"b c\" ''"), ["a", "b c", ""]);
}

#[test]
fn status_help_examples_parse() {
    let help = arg_help(&["status"], "query");
    let mut examples: Vec<&str> = help
        .split_once("Examples:")
        .expect("status --help has examples")
        .1
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- `")?.strip_suffix('`'))
        .collect();
    assert!(examples.len() >= 4, "{examples:?}");
    // The short help's example (`status -h`).
    let short = examples_after_eg(&help);
    assert_eq!(short, ["status=running first 5"]);
    examples.extend(short);
    for example in examples {
        crate::query::Query::parse(example)
            .unwrap_or_else(|e| panic!("`{example}` from `status --help`: {e}"));
        // Typed on the command line, the words are joined again.
        let mut args = vec!["status"];
        args.extend(example.split(' '));
        let SubCommand::Status { query, .. } = cmd(&args) else {
            panic!("expected status");
        };
        assert_eq!(query.join(" "), example);
    }
}

#[test]
fn delay_help_examples_parse() {
    let now = Local.with_ymd_and_hms(2024, 1, 10, 12, 0, 0).unwrap();
    let examples: Vec<&str> = DELAY_FORMAT_HELP
        .lines()
        .filter_map(|line| Some(line.split_once("//")?.0.trim()))
        .collect();
    assert!(examples.len() >= 15, "{examples:?}");
    let failed: Vec<&str> = examples
        .into_iter()
        .filter(|example| parse_delay_until_from(example, now).is_err())
        .collect();
    assert!(
        failed.is_empty(),
        "delay examples that do not parse: {failed:?}"
    );
    // Every subcommand that takes --delay shows the format.
    let paths: std::collections::BTreeSet<Vec<String>> =
        all_help_texts().into_iter().map(|(path, _)| path).collect();
    let mut with_delay = 0;
    for path in paths {
        let command = subcommand_at(&path[..]);
        if command.get_arguments().any(|a| a.get_id() == "delay") {
            assert_eq!(
                command.get_after_help().map(ToString::to_string).as_deref(),
                Some(DELAY_FORMAT_HELP),
                "{path:?} takes --delay but does not show DELAY FORMAT"
            );
            with_delay += 1;
        }
    }
    assert_eq!(with_delay, 3); // add, stash, enqueue
}

#[test]
fn value_examples_in_help_parse() {
    let signal_help = arg_help(&["kill"], "signal");
    let signals = examples_after_eg(&signal_help);
    assert!(!signals.is_empty());
    for signal in signals {
        assert_eq!(
            cmd(&["kill", "--signal", signal, "1"]),
            cmd(&["kill", "--signal", "SIGINT", "1"]),
            "{signal}"
        );
    }
    let add_help = arg_help(&["add"], "gpu");
    let parallel_help = arg_help(&["gpu", "parallel"], "gpu");
    let add_gpus = examples_after_eg(&add_help);
    let parallel_gpus = examples_after_eg(&parallel_help);
    assert!(!add_gpus.is_empty() && !parallel_gpus.is_empty());
    for gpus in add_gpus {
        parse(&["add", "--gpu", gpus, "x"]);
    }
    for gpus in parallel_gpus {
        parse(&["gpu", "parallel", "1", "--gpu", gpus]);
    }
}

#[test]
fn commands_and_options_in_help_texts_exist() {
    // Options of other programs that help texts mention on purpose.
    let foreign_options: [(&[&str], &str); 4] = [
        (&[], "config"),             // pueue's, which gpueue replaces with --socket
        (&[], "profile"),            // likewise
        (&["send"], "yes"),          // a job command's own flag
        (&["update"], "save-token"), // of install.sh
    ];
    let root = Cli::command();
    let mut commands = 0;
    for (path, text) in all_help_texts() {
        // References such as `gpueue group add` or `gpueue start <id>` name
        // existing subcommands.
        for snippet in backticked(&text) {
            let Some(rest) = snippet.strip_prefix("gpueue ") else {
                continue;
            };
            let mut words = rest.split(' ');
            let first = words.next().unwrap();
            let mut command = root
                .find_subcommand(first)
                .unwrap_or_else(|| panic!("{path:?}: `{snippet}` names no subcommand"));
            for word in words {
                if word.starts_with(['-', '<']) || command.get_subcommands().next().is_none() {
                    break;
                }
                command = command
                    .find_subcommand(word)
                    .unwrap_or_else(|| panic!("{path:?}: `{snippet}`: no subcommand `{word}`"));
            }
            commands += 1;
        }
        // Example command lines parse completely.
        for line in text.lines().map(str::trim) {
            let is_example = line
                .strip_prefix("gpueue ")
                .and_then(|rest| rest.split(' ').next())
                .is_some_and(|word| root.find_subcommand(word).is_some());
            if is_example {
                Cli::try_parse_from(shell_words(line))
                    .unwrap_or_else(|e| panic!("{path:?}: example `{line}` does not parse: {e}"));
                commands += 1;
            }
        }
        // Options mentioned as --name exist on this command or are global.
        let command = subcommand_at(&path[..]);
        for word in text.split(|c: char| c.is_whitespace() || "`(),.;:/\"'[]".contains(c)) {
            let Some(long) = word.strip_prefix("--") else {
                continue;
            };
            if !long.starts_with(|c: char| c.is_ascii_lowercase()) {
                continue;
            }
            let long = long.split('=').next().unwrap();
            let known = long == "help"
                || command
                    .get_arguments()
                    .chain(root.get_arguments())
                    .any(|arg| arg.get_long() == Some(long))
                || foreign_options
                    .iter()
                    .any(|(p, option)| *p == path.as_slice() && *option == long);
            assert!(
                known,
                "{path:?}: help mentions --{long}, which does not exist there"
            );
        }
    }
    assert!(
        commands >= 5,
        "only {commands} commands found in help texts"
    );
}

// --- Priority (gpueue extensions) ------------------------------------------------------------

#[test]
fn add_nice() {
    let SubCommand::Add { nice, priority, .. } = cmd(&["add", "--nice", "5", "-o", "-2", "true"])
    else {
        panic!()
    };
    assert_eq!(nice, Some(5));
    assert_eq!(priority, Some(-2));
    let SubCommand::Add { nice, .. } = cmd(&["add", "--nice", "-5", "true"]) else {
        panic!()
    };
    assert_eq!(nice, Some(-5));
    let SubCommand::Add { nice, .. } = cmd(&["add", "true"]) else {
        panic!()
    };
    assert_eq!(nice, None);
}

#[test]
fn nice_top_and_site() {
    assert_eq!(
        cmd(&["nice", "3", "10"]),
        SubCommand::Nice {
            task_ids: vec![3],
            nice: 10
        }
    );
    assert_eq!(
        cmd(&["nice", "3", "4", "-5"]),
        SubCommand::Nice {
            task_ids: vec![3, 4],
            nice: -5
        }
    );
    assert_eq!(
        parse_err(&["nice", "3"]),
        ErrorKind::MissingRequiredArgument
    );
    assert_eq!(
        cmd(&["top", "4", "2"]),
        SubCommand::Top {
            task_ids: vec![4, 2]
        }
    );
    assert_eq!(parse_err(&["top"]), ErrorKind::MissingRequiredArgument);
    assert_eq!(
        cmd(&["site", "1", "-100"]),
        SubCommand::Site {
            task_ids: vec![1],
            site: -100
        }
    );
}

#[test]
fn prio_and_share() {
    assert_eq!(
        cmd(&["prio"]),
        SubCommand::Prio {
            task_ids: vec![],
            json: false
        }
    );
    assert_eq!(
        cmd(&["prio", "-j", "1", "2"]),
        SubCommand::Prio {
            task_ids: vec![1, 2],
            json: true
        }
    );
    assert_eq!(
        cmd(&["share", "--json"]),
        SubCommand::Share {
            json: true,
            cmd: None
        }
    );
    assert_eq!(
        cmd(&["share", "set", "alice", "3"]),
        SubCommand::Share {
            json: false,
            cmd: Some(ShareCommand::Set {
                user: "alice".into(),
                shares: 3
            })
        }
    );
    assert_eq!(
        parse_err(&["share", "set", "alice", "-1"]),
        ErrorKind::UnknownArgument
    );
    assert_eq!(
        cmd(&["group", "priority", "gpu", "10"]),
        SubCommand::Group {
            json: false,
            cmd: Some(GroupCommand::Priority {
                name: "gpu".into(),
                priority: 10
            })
        }
    );
    for name in ["prio", "nice", "top", "site", "share"] {
        assert!(Cli::command().find_subcommand(name).is_some(), "{name}");
    }
}
