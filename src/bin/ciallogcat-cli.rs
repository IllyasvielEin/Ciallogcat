#[path = "../cli/mod.rs"]
mod cli;

fn main() {
    std::process::exit(cli::run());
}
