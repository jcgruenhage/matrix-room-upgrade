use clap_allgen::{render_manpages, render_shell_completions};

pub mod cli {
    include!("src/cli.rs");
}

fn main() -> anyhow::Result<()> {
    render_shell_completions::<cli::Cli>("generated/completions")?;
    render_manpages::<cli::Cli>("generated/man")?;

    Ok(())
}
