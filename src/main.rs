use std::env;
use std::io;
use std::io::IsTerminal;
use std::io::Write as _;
use std::process;

fn main() {
    let signals = match shdeps::cancellation::Signals::install() {
        Ok(signals) => signals,
        Err(error) => {
            eprintln!("error: could not install signal handling: {error}");
            process::exit(1);
        }
    };
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let live_progress = stdout.is_terminal();

    let result =
        shdeps::cli::run_terminal(env::args().skip(1), &mut stdout, &mut stderr, live_progress);
    for diagnostic in signals.take_cleanup_diagnostics() {
        let _ = writeln!(stderr, "error: subprocess cleanup: {diagnostic}");
    }
    let fallback = match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    };
    let _ = stdout.flush();
    let _ = stderr.flush();
    signals.exit_process(fallback)
}
