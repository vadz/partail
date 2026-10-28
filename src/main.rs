use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, ValueEnum};

use partail::config::{self, Config, Window};
use partail::line::Escape;

mod app;
mod scheme_test;

/// Follow several log files in parallel, colouring their lines with regex rules.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Configuration file to use instead of the default one.
    #[arg(short, long, value_name = "CONFIG")]
    config: Option<PathBuf>,

    /// Show these characters as hexadecimal escapes too, like the
    /// unprintable ones, e.g. if the terminal disagrees about their width,
    /// overriding the configuration file.
    #[arg(long, value_name = "CHARS")]
    escape: Option<EscapeArg>,

    /// Test the scheme with this name from the configuration file: process
    /// the lines given on the command line, or read from the standard input
    /// if there are none, and output them coloured, instead of following
    /// any files.
    #[arg(long, value_name = "SCHEME")]
    test: Option<String>,

    /// Also output what each strip pattern and rule matched with --test.
    #[arg(short, long, requires = "test")]
    verbose: bool,

    /// Files to follow instead of the ones from the configuration, without
    /// colouring, or lines to process with --test.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,
}

/// Printable characters which can be shown as escapes.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum EscapeArg {
    /// No characters, only the unprintable ones are escaped.
    Unprintable,
    /// All characters taking two columns, e.g. CJK ideographs and emojis.
    Wide,
    /// Characters outside the Basic Multilingual Plane, e.g. most emojis.
    NonBmp,
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
    if let Some(name) = &cli.test {
        return test_scheme(cli, name);
    }

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

/// Implements --test using the scheme with the given name.
fn test_scheme(cli: &Cli, name: &str) -> anyhow::Result<()> {
    let (config, path) = load_config_file(cli)?;
    let Some(scheme) = config.schemes.get(name) else {
        let Some(path) = path else {
            bail!("no configuration file to take the scheme \"{name}\" from");
        };
        if config.schemes.is_empty() {
            bail!("no schemes defined in \"{}\"", path.display());
        }
        bail!(
            "unknown scheme \"{name}\" in \"{}\", expected one of: {}",
            path.display(),
            config
                .schemes
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    };

    let lines: Vec<_> = cli.files.iter().map(|f| f.as_os_str().as_bytes()).collect();
    scheme_test::run(scheme, config.escape, cli.verbose, &lines)
}

/// Loads the configuration file, if there is one, taking the options
/// overriding it into account. Also returns the path of the file.
fn load_config_file(cli: &Cli) -> anyhow::Result<(Config, Option<PathBuf>)> {
    let path = match &cli.config {
        Some(path) => Some(path.clone()),
        None => config::default_path().filter(|path| path.exists()),
    };

    let mut config = match &path {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };

    if let Some(escape) = cli.escape {
        config.escape = match escape {
            EscapeArg::Unprintable => Escape::Unprintable,
            EscapeArg::Wide => Escape::Wide,
            EscapeArg::NonBmp => Escape::NonBmp,
        };
    }

    Ok((config, path))
}

/// Loads the configuration to use, taking the command line into account.
fn load_config(cli: &Cli) -> anyhow::Result<Config> {
    let (mut config, path) = load_config_file(cli)?;

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
        match (path, config::default_path()) {
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
