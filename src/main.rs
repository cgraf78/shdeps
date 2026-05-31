use std::env;
use std::io;
use std::io::IsTerminal;
use std::process;

fn main() {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let live_progress = stdout.is_terminal();

    match shdeps::cli::run_terminal(env::args().skip(1), &mut stdout, &mut stderr, live_progress) {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("error: {error}");
            process::exit(1);
        }
    }
}
