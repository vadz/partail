use std::path::PathBuf;

use clap::Parser;

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

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();

    anyhow::bail!("not implemented yet")
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
