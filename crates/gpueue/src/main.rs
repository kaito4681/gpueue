mod cli;
mod clock;
mod commands;
mod doctor;
mod install_method;
mod query;
mod release_key;
mod render;
mod requests;
mod service;
mod style;
mod system;
mod update;
mod users;

use std::{io, path::Path, process::ExitCode};

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches};
use clap_complete::{Generator, generate, generate_to, shells};
use gpueue_protocol::client::Client;

use crate::cli::{Cli, ServiceCommand, Shell, SubCommand};
use crate::clock::Clock;
use crate::commands::App;
use crate::render::RenderCtx;
use crate::style::Style;
use crate::users::UserNames;

const BIN_NAME: &str = "gpueue";

fn main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };
    if let Some(name) = cli.json_unsupported_by() {
        Cli::command()
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                format!("`{name}` has no JSON output; --json only applies to status, log, group, gpu, prio and share"),
            )
            .exit();
    }
    // Only an explicit --socket / GPUEUE_SOCKET overrides the daemon's config file.
    let socket_is_explicit = !matches!(
        matches.value_source("socket"),
        None | Some(ValueSource::DefaultValue)
    );

    match cli.subcommand() {
        SubCommand::Completions {
            shell,
            output_directory,
        } => match write_completions(shell, output_directory.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => fail(&format!(
                "failed to generate completions for {shell:?}: {err}"
            )),
        },
        SubCommand::Send { .. } => fail(
            "`send` is not supported: gpueue runs every job with stdin connected to /dev/null, \
             so there is nothing to send input to. Make the command non-interactive instead \
             (e.g. `yes | cmd` or a `--yes` flag).",
        ),
        SubCommand::Daemon {
            config,
            data_dir,
            helper_socket,
        } => run_daemon(DaemonArgs {
            config,
            data_dir,
            helper_socket,
            socket: socket_is_explicit.then(|| cli.socket.clone()),
        }),
        SubCommand::Service { cmd } => {
            run_service(cmd, socket_is_explicit.then_some(cli.socket.as_path()))
        }
        SubCommand::Uninstall { purge, yes } => {
            run_service(ServiceCommand::Uninstall { purge, yes }, None)
        }
        SubCommand::Doctor => {
            let report = doctor::run(
                &system::RealSystem,
                socket_is_explicit.then_some(cli.socket.as_path()),
            );
            let _ = report.print(&mut io::stdout());
            if report.has_failures() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        SubCommand::Update {
            check,
            version,
            yes,
        } => {
            let options = update::UpdateOptions {
                check,
                version,
                yes,
            };
            match update::update(&system::RealSystem, &options, &mut io::stdout()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => fail(&err),
            }
        }
        other @ (SubCommand::Edit { .. } | SubCommand::Env { .. }) => {
            fail(&format!("`{}` is not implemented yet", other.name()))
        }
        other => run_remote(&cli, other),
    }
}

/// Runs a command that talks to the daemon.
fn run_remote(cli: &Cli, command: SubCommand) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fail(&format!("failed to start the async runtime: {err}")),
    };
    let app = App {
        client: Client::new(&cli.socket),
        ctx: RenderCtx {
            style: Style::for_stdout(cli.color),
            clock: Clock::system(),
            users: UserNames::system(),
            detect_width: true,
        },
    };
    runtime.block_on(async {
        let code = match app.run(command).await {
            Ok(code) => code,
            // The reader of our output is gone (e.g. `| grep -q`).
            Err(commands::CliError::StdoutClosed) => ExitCode::SUCCESS,
            Err(err) => fail(&commands::describe_error(err, &app.client).await),
        };
        // Warnings the daemon keeps for this user (e.g. a failed checkpoint).
        for warning in app.client.take_warnings() {
            eprintln!(
                "{BIN_NAME}: warning: {}",
                render::escape_controls(&warning.to_string())
            );
        }
        code
    })
}

/// `gpueue service ...`.
fn run_service(cmd: ServiceCommand, socket: Option<&Path>) -> ExitCode {
    let sys = system::RealSystem;
    let mut out = io::stdout();
    let result = match cmd {
        ServiceCommand::Install {
            no_start,
            bin_dir,
            force,
        } => {
            let options = service::InstallOptions {
                no_start,
                bin_dir,
                force,
            };
            service::install(&sys, &options, &mut out).map(|report| {
                let ok = service::install_succeeded(&report, &options);
                if !ok {
                    eprintln!(
                        "{BIN_NAME}: error: installed, but the checks above failed; fix them and \
                         re-run `gpueue doctor`"
                    );
                }
                ok
            })
        }
        ServiceCommand::Uninstall { purge, yes } => {
            service::uninstall(&sys, &service::UninstallOptions { purge, yes }, &mut out)
                .map(|()| true)
        }
        ServiceCommand::Status => {
            service::status(&sys, socket, &mut out).map_err(|e| format!("writing output: {e}"))
        }
        ServiceCommand::InstallMethod => {
            println!("{}", install_method::detect(&sys).keyword());
            Ok(true)
        }
        ServiceCommand::VersionInfo => serde_json::to_string(&update::VersionInfo::this_binary())
            .map(|json| {
                println!("{json}");
                true
            })
            .map_err(|e| e.to_string()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => fail(&err),
    }
}

struct DaemonArgs {
    config: std::path::PathBuf,
    data_dir: Option<std::path::PathBuf>,
    helper_socket: Option<std::path::PathBuf>,
    socket: Option<std::path::PathBuf>,
}

/// `gpueue daemon`: run the daemon in the foreground until it is stopped.
fn run_daemon(args: DaemonArgs) -> ExitCode {
    // The same trust check as the helper's: the file can name a library the daemon loads
    // (`nvml_library`) and programs, so one that anyone but root could have changed is refused.
    let mut config = match gpueue_daemon::Config::load_trusted(&args.config) {
        Ok(config) => config,
        Err(err) => return fail(&format!("{}: {err}", args.config.display())),
    };
    if let Some(data_dir) = args.data_dir {
        config = config.with_data_dir(data_dir);
    }
    if let Some(helper_socket) = args.helper_socket {
        config.helper_socket = helper_socket;
    }
    if let Some(socket) = args.socket {
        config.api_socket = socket;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fail(&format!("failed to start the async runtime: {err}")),
    };
    match runtime.block_on(gpueue_daemon::run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail(&format!("daemon failed: {err}")),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{BIN_NAME}: error: {}", render::escape_controls(message));
    ExitCode::FAILURE
}

/// Print a completion script to stdout, or write it into `output_directory`.
fn write_completions(shell: Shell, output_directory: Option<&Path>) -> io::Result<()> {
    match shell {
        Shell::Bash => emit(shells::Bash, output_directory),
        Shell::Elvish => emit(shells::Elvish, output_directory),
        Shell::Fish => emit(shells::Fish, output_directory),
        Shell::PowerShell => emit(shells::PowerShell, output_directory),
        Shell::Zsh => emit(shells::Zsh, output_directory),
        Shell::Nushell => emit(clap_complete_nushell::Nushell, output_directory),
    }
}

fn emit<G: Generator>(generator: G, output_directory: Option<&Path>) -> io::Result<()> {
    let mut command = Cli::command();
    command.set_bin_name(BIN_NAME);
    match output_directory {
        Some(dir) => generate_to(generator, &mut command, BIN_NAME, dir).map(|_| ()),
        None => {
            generate(generator, &mut command, BIN_NAME, &mut io::stdout());
            Ok(())
        }
    }
}
