//! `bo` command-line front-end: the mini CLI. See [`cli`] for the surface;
//! the daemon it spawns lives in [`daemon`].

mod cli;
mod daemon;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(cli::run(args));
}
