use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use clap::Parser;

use hon::cli::{Cli, Command, man_page};
use hon::commands::{self, Ctx};
use hon::config::{Config, Paths};
use hon::tui::{self, Start};
use hon_core::db::Db;
use hon_core::sources;

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        None => tui::run(context()?, Start::Search).await,
        Some(Command::Read {
            title,
            chapter,
            source,
        }) => commands::read(context()?, &title, chapter.as_deref(), source.as_deref()).await,
        Some(Command::Man) => {
            std::io::stdout().write_all(&man_page())?;
            Ok(())
        }
        Some(Command::Search { query }) => {
            commands::search(&context()?, &query, &mut std::io::stdout()).await
        }
        Some(Command::Download {
            title,
            chapters,
            source,
            format,
        }) => {
            let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            commands::download(
                &context()?,
                &title,
                &chapters,
                source.as_deref(),
                format,
                interactive,
            )
            .await
        }
    }
}

fn context() -> anyhow::Result<Ctx> {
    let paths = Paths::detect()?;
    let config = Config::load(&paths)?;
    let db = Db::open(&paths.db_file)?;
    let sources = sources::all(sources::http_client());
    Ok(Ctx {
        sources,
        db,
        config,
    })
}
