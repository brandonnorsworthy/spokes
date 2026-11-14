use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

use spokes::config::Config;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::main]
async fn main() -> ExitCode {
    // `spokes healthcheck` probes /health on the configured port (for Docker HEALTHCHECK,
    // since the runtime image has no shell or curl).
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("healthcheck") => return healthcheck(),
        Some("backup") if args.len() == 2 => return cli_backup(&args[1]),
        Some("restore") if args.len() == 2 => return cli_restore(&args[1]),
        None => {}
        Some(_) => {
            eprintln!("usage: spokes [healthcheck | backup <dir-or-file> | restore <backup-file>]");
            return ExitCode::from(2);
        }
    }
    let level = std::env::var("SPOKES_LOG")
        .ok()
        .and_then(|l| l.parse().ok())
        .unwrap_or(tracing::Level::INFO);
    // rmcp logs every stateless request at info; keep it to warnings.
    let filter = Targets::new()
        .with_default(level)
        .with_target("rmcp", level.min(tracing::Level::WARN));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout())),
        )
        .with(filter)
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(
        bind = %config.bind,
        db = %config.db_path.display(),
        "spokes listening; MCP endpoint /mcp"
    );
    spokes::serve(listener, &config, shutdown_signal()).await?;
    tracing::info!("spokes stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}

/// Probes /health on the configured address; true if it answers 200.
fn server_running(config: &Config) -> bool {
    let mut addr = config.bind;
    if addr.ip().is_unspecified() {
        addr = SocketAddr::from(([127, 0, 0, 1], addr.port()));
    }
    let probe = || -> std::io::Result<bool> {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        write!(
            stream,
            "GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        )?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response.starts_with("HTTP/1.1 200"))
    };
    probe().unwrap_or(false)
}

fn healthcheck() -> ExitCode {
    match Config::from_env() {
        Ok(config) if server_running(&config) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

fn cli_backup(target: &str) -> ExitCode {
    let run = || -> anyhow::Result<()> {
        let config = Config::from_env()?;
        let target = std::path::Path::new(target);
        let dest = if target.is_dir() {
            target.join(spokes::backup::backup_file_name())
        } else {
            target.to_path_buf()
        };
        let bytes = spokes::backup::write_backup(&config.db_path, &dest)?;
        println!("{} ({bytes} bytes)", dest.display());
        Ok(())
    };
    finish(run())
}

fn cli_restore(backup: &str) -> ExitCode {
    let run = || -> anyhow::Result<()> {
        let config = Config::from_env()?;
        if server_running(&config) {
            anyhow::bail!("spokes is running on {}; stop it first", config.bind);
        }
        let msg = spokes::backup::restore(std::path::Path::new(backup), &config.db_path)?;
        println!("{msg}");
        Ok(())
    };
    finish(run())
}

fn finish(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
