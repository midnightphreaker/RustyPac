use std::error::Error;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Eq, PartialEq)]
pub enum Command {
    Download { url: String, output: PathBuf },
    Enable,
    Disable,
}

#[derive(Debug, Eq, PartialEq)]
pub struct CliError;

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("usage: RustyPac URL OUTPUT | RustyPac --enable | RustyPac --disable")
    }
}

impl Error for CliError {}

pub fn parse_args<I, S>(args: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut values = args.into_iter();
    let _program = values.next().ok_or(CliError)?;
    let arguments: Vec<String> = values.map(|value| value.as_ref().to_owned()).collect();

    match arguments.as_slice() {
        [option] if option == "--enable" => Ok(Command::Enable),
        [option] if option == "--disable" => Ok(Command::Disable),
        [url, output]
            if !url.starts_with("--") && output != "--enable" && output != "--disable" =>
        {
            Ok(Command::Download {
                url: url.clone(),
                output: PathBuf::from(output),
            })
        }
        _ => Err(CliError),
    }
}
