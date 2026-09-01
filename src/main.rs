//! `bo` command-line front-end: one command per invocation, meant to be driven
//! by an agent assembling a broadcast. See [`cli`] for the command surface.

mod cli;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(cli::run(args));
}
