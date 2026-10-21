// Remove this once the stubs are replaced by real code.
#![allow(dead_code)]

mod cgi;
mod config;
mod connection;
mod error;
mod http;
mod router;
mod server;
mod spool;
mod static_files;
mod upload;
mod utils;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.conf".to_string());

    let config = match config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = server::run(config) {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
