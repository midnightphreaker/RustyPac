#[path = "../src/cli.rs"]
mod cli;

use std::path::PathBuf;

use cli::{parse_args, Command};

fn parse(arguments: &[&str]) -> Result<Command, cli::CliError> {
    parse_args(arguments.iter().copied())
}

#[test]
fn parses_download_and_preserves_output_spelling() {
    let output = "./cache/../cache/package.pkg.tar.zst.part";

    let command = parse(&["RustyPac", "https://mirror.example/package", output]).unwrap();

    assert_eq!(
        command,
        Command::Download {
            url: "https://mirror.example/package".to_owned(),
            output: PathBuf::from(output),
        }
    );
}

#[test]
fn parses_enable() {
    assert_eq!(parse(&["RustyPac", "--enable"]).unwrap(), Command::Enable);
}

#[test]
fn parses_disable() {
    assert_eq!(parse(&["RustyPac", "--disable"]).unwrap(), Command::Disable);
}

#[test]
fn rejects_missing_arguments() {
    for arguments in [
        ["RustyPac"].as_slice(),
        ["RustyPac", "https://mirror.example/package"].as_slice(),
    ] {
        assert!(parse(arguments).is_err(), "{arguments:?} must be rejected");
    }
}

#[test]
fn rejects_excess_arguments() {
    for arguments in [
        ["RustyPac", "--enable", "extra"].as_slice(),
        ["RustyPac", "--disable", "extra"].as_slice(),
        [
            "RustyPac",
            "https://mirror.example/package",
            "package.part",
            "extra",
        ]
        .as_slice(),
    ] {
        assert!(parse(arguments).is_err(), "{arguments:?} must be rejected");
    }
}

#[test]
fn rejects_mixed_option_and_download_arguments() {
    for arguments in [
        ["RustyPac", "--enable", "package.part"].as_slice(),
        ["RustyPac", "https://mirror.example/package", "--disable"].as_slice(),
        ["RustyPac", "--disable", "https://mirror.example/package"].as_slice(),
    ] {
        assert!(parse(arguments).is_err(), "{arguments:?} must be rejected");
    }
}
