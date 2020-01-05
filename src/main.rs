use common_mark_language_server::lsp;

use {anyhow::Result, log::info, lsp_server::Connection, structopt::StructOpt};

#[derive(StructOpt)]
struct Opt {
    #[structopt(long="verbosity", possible_values=&["error", "warn", "info", "debug"], default_value="info")]
    verbosity: String,

    #[structopt(long = "log-directory", default_value = "/tmp")]
    log_directory: String,
}

fn main() -> Result<()> {
    let opt = Opt::from_args();

    // Set up logging. Because `stdio_transport` gets a lock on stdout and stdin, we must have
    // our logging only write out to stderr.
    flexi_logger::Logger::with_env_or_str(opt.verbosity)
        .log_to_file()
        .directory(opt.log_directory)
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
