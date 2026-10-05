use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Parser;

use partail::config::{self, Config, Window};

mod app;

/// Follow several log files in parallel, colouring their lines with regex rules.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Configuration file to use instead of the default one.
    #[arg(short, long, value_name = "CONFIG")]
    config: Option<PathBuf>,

    /// Files to follow instead of the ones from the configuration, without
    /// colouring.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,
}

fn main() -> ExitCode {
    match run(&Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("partail: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<()> {
    let config = load_config(cli)?;

    let mut terminal = ratatui::try_init().context("failed to initialize the terminal")?;
    let result = terminal
        .size()
        .map_err(anyhow::Error::from)
        .and_then(|size| {
            app::run(
                &mut terminal,
                &mut app::App::new(config, size.height.into()),
            )
        });
    ratatui::restore();
    result
}

/// Loads the configuration to use, taking the command line into account.
fn load_config(cli: &Cli) -> anyhow::Result<Config> {
    let default_path = config::default_path();
    let path = match &cli.config {
        Some(path) => Some(path.clone()),
        None => default_path.clone().filter(|path| path.exists()),
    };

    let mut config = match &path {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };

    if !cli.files.is_empty() {
        config.windows = cli
            .files
            .iter()
            .map(|file| Window {
                file: file.clone(),
                height: None,
                scheme: Arc::default(),
                wrap: true,
            })
            .collect();
    }

    if config.windows.is_empty() {
        match (path, default_path) {
            (Some(path), _) => bail!(
                "nothing to follow: no files given and no windows defined in \"{}\"",
                path.display()
            ),
            (None, Some(path)) => bail!(
                "nothing to follow: no files given and no configuration file \"{}\"",
                path.display()
            ),
            (None, None) => bail!("nothing to follow: no files given"),
        }
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_valid() {
        Cli::command().debug_assert();
    }
}
