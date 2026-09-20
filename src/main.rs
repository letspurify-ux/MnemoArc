use clap::{Parser, Subcommand};
use mnemoarc::{
    config::{Config, Project},
    session::Session,
};
use std::{collections::BTreeMap, path::PathBuf};
#[derive(Parser)]
#[command(
    version,
    about = "Session-local memory agent for evidence-based source documentation"
)]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Serve the React browser UI and local agent API (also the default).
    Web {
        #[arg(long)]
        port: Option<u16>,
        /// Override embedded UI files for development.
        #[arg(long)]
        frontend: Option<PathBuf>,
        /// Print the URL without opening a browser.
        #[arg(long)]
        no_open: bool,
        /// Shut down cleanly when the launcher closes its stdin pipe.
        #[arg(long, hide = true)]
        shutdown_on_stdin: bool,
    },
    /// Print a full example configuration without credentials.
    Config,
    /// Verify plain response, streaming and tool-call round trip.
    Check,
    /// Repeat fixed-source documentation cases with full and restricted memory reuse.
    Evaluate {
        #[arg(long, default_value = "eval/suite.toml")]
        suite: PathBuf,
        #[arg(long, default_value = "eval/results")]
        output: PathBuf,
    },
    /// Run the same agent without the browser UI.
    Run {
        #[arg(long, default_value = ".")]
        project: PathBuf,
        #[arg(long, default_value = "docs/source-summary.md")]
        output: PathBuf,
        #[arg(long)]
        prompt: String,
    },
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Some(Command::Config)) {
        println!("{}", toml::to_string_pretty(&Config::default())?);
        return Ok(());
    }
    let command = cli.command.unwrap_or(Command::Web {
        port: None,
        frontend: None,
        no_open: false,
        shutdown_on_stdin: false,
    });
    let path = match cli.config {
        Some(path) => path,
        None if matches!(command, Command::Web { .. }) => mnemoarc::desktop::config_path()?,
        None => PathBuf::from("config.toml"),
    };
    let config = Config::load(&path, &BTreeMap::new())?;
    match command {
        Command::Web {
            port,
            frontend,
            shutdown_on_stdin,
            no_open,
        } => {
            mnemoarc::web::serve_app(config, path, port, frontend, shutdown_on_stdin, !no_open)
                .await?
        }
        Command::Config => unreachable!(),
        Command::Check => println!("{}", mnemoarc::llm::OpenAiClient.probe(&config).await?),
        Command::Evaluate { suite, output } => {
            mnemoarc::evaluation::run(config, &suite, &output).await?
        }
        Command::Run {
            project,
            output,
            prompt,
        } => {
            let p = Project {
                root: project.canonicalize()?,
                output,
                ..Default::default()
            };
            let s = mnemoarc::agent::headless(Session::new(p, config), prompt).await?;
            if s.status == "blocked" || s.status == "partial" {
                std::process::exit(2);
            }
        }
    }
    Ok(())
}
