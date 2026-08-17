use std::process::ExitCode;

use clap::Parser;
use symbol::Cli;
use symbol::run;

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("s: {error:#}");
            ExitCode::FAILURE
        }
    }
}
