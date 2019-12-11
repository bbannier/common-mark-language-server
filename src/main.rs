use common_mark_language_server::lsp;

use {log::info, lsp_server::Connection, std::error::Error, structopt::StructOpt};

#[derive(StructOpt)]
struct Opt {}

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    let _ = Opt::from_args();

    // Set up logging. Because `stdio_transport` gets a lock on stdout and stdin, we must have
    // our logging only write out to stderr.
    flexi_logger::Logger::with_env_or_str("debug")
        .log_to_file()
        .directory("/tmp")
        .start()?;
    info!("starting generic LSP server");

    // Create the transport. Includes the stdio (stdin and stdout) versions but this could
    // also be implemented to use sockets or HTTP.
    let (connection, io_threads) = Connection::stdio();

    // Run the server and wait for the two threads to end (typically by trigger LSP Exit event).
    lsp::run_server(connection)?;
    io_threads.join()?;

    // Shut down gracefully.
    info!("shutting down server");
    Ok(())
}
