use clap::Parser;

use std::path::PathBuf;

#[derive(Debug, Parser)]
struct Cli {
    path: Option<PathBuf>,
}

fn main() {
    let _cli = Cli::parse();
    println!("Hello, world!");
}
