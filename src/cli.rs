use std::process::ExitCode;

pub fn run<I, S>(_args: I) -> ExitCode
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    ExitCode::SUCCESS
}
