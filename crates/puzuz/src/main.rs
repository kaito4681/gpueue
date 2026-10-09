mod cli;

use std::{io, path::Path, process::ExitCode};

use clap::{CommandFactory, Parser};
use clap_complete::{Generator, generate, generate_to, shells};

use crate::cli::{Cli, Shell, SubCommand};

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
        other => fail(&format!("`{}` is not implemented yet", other.name())),
    }
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
