use std::process::ExitCode;

fn main() -> ExitCode {
    xgraph::cli::run(std::env::args())
}
