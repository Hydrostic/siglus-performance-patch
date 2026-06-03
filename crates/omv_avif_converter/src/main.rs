mod header;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read and print the OMV header.
    DumpHeader {
        /// Path to the input .omv file.
        input: PathBuf,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Command::DumpHeader { input } => {
            let header = header::OmvHeader::read_from_file(input)?;
            println!("{header}");
        }
    }

    Ok(())
}
