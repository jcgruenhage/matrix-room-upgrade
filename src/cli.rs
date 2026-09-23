use std::path::PathBuf;

use clap::{ArgAction, Parser};

#[derive(Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Config file location
    #[arg(short, long, value_name = "FILE")]
    pub config: PathBuf,

    /// Log more, can be repeated
    #[arg(short, long, action = ArgAction::Count)]
    pub verbose: u8,

    /// Log less, can be repeated
    #[arg(short, long, action = ArgAction::Count, conflicts_with = "verbose")]
    pub quiet: u8,
}
