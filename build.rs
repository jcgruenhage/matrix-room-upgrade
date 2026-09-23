use clap_allgen::{render_manpages, render_shell_completions};

pub mod cli {
    include!("src/cli.rs");
}

fn main() -> anyhow::Result<()> {
    println!("cargo:rerun-if-changed=src/cli.rs");
    render_shell_completions::<cli::Cli>("generated/completions")?;
    render_manpages::<cli::Cli>("generated/man")?;

    Ok(())
}
