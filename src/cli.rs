use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    // Config file location
    #[arg(short, long, value_name = "FILE")]
    pub config: PathBuf,
}
