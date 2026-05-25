#![deny(unsafe_code)]

#[cfg(not(target_os = "linux"))]
compile_error!("xgraph is Linux-only");

pub mod cli;
pub mod daemon;
pub mod git;
pub mod indexes;
pub mod language;
pub mod languages;
pub mod laravel;
pub mod manifest;
pub mod mcp;
pub mod parser;
pub mod query;
pub mod runtime;
pub mod scanner;
pub mod storage;
pub mod watcher;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn exposes_package_version() {
        assert_eq!(VERSION, "0.1.0");
    }
}
