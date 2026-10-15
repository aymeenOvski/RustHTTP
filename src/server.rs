//! Owner: Person A — mio event loop, listeners, timeouts.
use crate::config::Config;

pub fn run(config: Config) -> std::io::Result<()> {
    // TODO(A): register listeners with mio::Poll and run the event loop.
    for s in &config.servers {
        println!("[stub] would listen on {}:{:?}", s.host, s.ports);
    }
    Ok(())
}
