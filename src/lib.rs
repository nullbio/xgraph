#![deny(unsafe_code)]

#[cfg(not(target_os = "linux"))]
compile_error!("xgraph is Linux-only");

pub mod cli;
pub mod cozo;
pub mod daemon;
pub mod daemon_status;
pub mod extract;
pub mod git;
pub mod handlers;
pub mod hash;
pub mod ignore;
pub mod import_resolver;
pub mod indexes;
pub mod language;
pub mod languages;
pub mod laravel;
pub mod manifest;
pub mod mcp;
pub mod mcp_install;
pub mod mcp_protocol;
pub mod owner;
pub mod parser;
pub mod progress;
pub mod query;
pub mod react;
pub mod resolve;
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
