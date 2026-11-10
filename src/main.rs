mod collector;
mod geo;
mod server;
mod tracker;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// See which apps on your machine phone home, and to whom.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Port of the local interface
    #[arg(long, default_value_t = 4747)]
    port: u16,

    /// Milliseconds between two readings of the socket table
    #[arg(long, default_value_t = 1000, global = true)]
    interval: u64,

    /// Also show connections to this machine and the local network
    #[arg(long, global = true)]
    include_local: bool,

    /// Never touch the network, even to fetch the location databases
    #[arg(long, global = true)]
    offline: bool,

    /// Do not open the browser
    #[arg(long)]
    no_open: bool,

    /// Show simulated traffic instead of this machine's
    #[arg(long)]
    demo: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Download the latest location databases
    Update,
    /// Watch for a few seconds, then print what was seen as JSON
    Snapshot {
        /// How long to watch
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = geo::data_dir()?;

    if let Some(Command::Update) = cli.command {
        eprintln!("Downloading location databases to {}", data_dir.display());
        geo::download_databases(&data_dir)?;
        eprintln!("Done.");
        return Ok(());
    }

    if !cli.demo && !geo::databases_present(&data_dir) {
        if cli.offline {
            eprintln!("Location databases are missing: destinations will show as unknown. Run `phonehome update`.");
        } else {
            eprintln!(
                "First run: downloading location databases (about 20 MB) to {}",
                data_dir.display()
            );
            if let Err(error) = geo::download_databases(&data_dir) {
                eprintln!("Download failed ({error:#}). Continuing without locations.");
            }
        }
    }

    let mut collector = collector::new()?;
    let mut tracker = tracker::Tracker::new(
        geo::Geo::open(&data_dir),
        cli.include_local,
        collector.byte_counts(),
    );
    let interval = Duration::from_millis(cli.interval.max(200));

    if let Some(Command::Snapshot { seconds }) = cli.command {
        let end = Instant::now() + Duration::from_secs(seconds);
        loop {
            tracker.update(&collector.poll()?, Instant::now());
            if Instant::now() >= end {
                break;
            }
            thread::sleep(interval);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&tracker.snapshot(Instant::now()))?
        );
        return Ok(());
    }

    let state: server::SharedState = Arc::new(Mutex::new(String::from("{}")));
    if cli.demo {
        *state.lock().unwrap() = String::from(r#"{"demo":true}"#);
    } else {
        let state = Arc::clone(&state);
        thread::spawn(move || loop {
            match collector.poll() {
                Ok(flows) => {
                    let now = Instant::now();
                    tracker.update(&flows, now);
                    if let Ok(json) = serde_json::to_string(&tracker.snapshot(now)) {
                        *state.lock().unwrap() = json;
                    }
                }
                Err(error) => eprintln!("reading sockets failed: {error:#}"),
            }
            thread::sleep(interval);
        });
    }

    let http = server::bind(cli.port)?;
    let url = format!("http://127.0.0.1:{}", cli.port);
    eprintln!("phonehome is watching. Open {url}  (Ctrl+C to stop)");
    if !cli.no_open {
        open_browser(&url);
    }
    server::serve(http, state);
    Ok(())
}

fn open_browser(url: &str) {
    let mut command = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
