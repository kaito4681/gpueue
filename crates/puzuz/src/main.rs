mod cli;
mod clock;
mod commands;
mod query;
mod render;
mod requests;
mod style;
mod users;

use std::{io, path::Path, process::ExitCode};

use clap::{CommandFactory, Parser};
use clap_complete::{Generator, generate, generate_to, shells};
use puzuz_protocol::client::Client;

use crate::cli::{Cli, Shell, SubCommand};
use crate::clock::Clock;
use crate::commands::App;
use crate::render::RenderCtx;
use crate::style::Style;
use crate::users::UserNames;

const BIN_NAME: &str = "puzuz";

fn main() -> ExitCode {
    let cli = Cli::parse();

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
            "`send` is not supported: puzuz runs every job with stdin connected to /dev/null, \
             so there is nothing to send input to. Make the command non-interactive instead \
             (e.g. `yes | cmd` or a `--yes` flag).",
        ),
        other @ (SubCommand::Daemon { .. }
        | SubCommand::Service { .. }
        | SubCommand::Doctor
        | SubCommand::Edit { .. }
        | SubCommand::Env { .. }) => fail(&format!("`{}` is not implemented yet", other.name())),
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
        match app.run(command).await {
            Ok(code) => code,
            Err(err) => fail(&commands::describe_error(err, &app.client).await),
        }
    })
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{BIN_NAME}: error: {message}");
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
