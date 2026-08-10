mod cli;
pub mod progress;
pub mod render;

use std::process::ExitCode;

fn main() -> ExitCode {
    match cli::parse_args(std::env::args()) {
        Ok(_command) => {
            eprintln!("RustyPac: command is not yet wired");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("RustyPac: {error}");
            ExitCode::from(2)
        }
    }
}
