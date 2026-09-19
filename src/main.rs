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
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Open the terminal UI (also the default).
    Tui,
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
    /// Run the same agent without the terminal UI.
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
    let config = Config::load(&cli.config, &BTreeMap::new())?;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => mnemoarc::ui::run(config, cli.config).await?,
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
