mod chart;
mod cli;
mod device;
mod gui;
mod model;
mod monitor;
mod notify;
mod protocol;
mod store;
mod theme;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("status") => cli::status(),
        Some("watch" | "monitor") => cli::watch(),
        Some("raw") => cli::raw(),
        Some("events") => cli::events(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(20)),
        Some("help" | "-h" | "--help") => {
            print!("{}", cli::USAGE);
            Ok(())
        }
        Some(arg) if !arg.starts_with('-') => {
            Err(format!("comando desconocido: {arg}\n\n{}", cli::USAGE))
        }
        _ => gui::Options::parse(&args)
            .map_err(|e| format!("{e}\n\n{}", cli::USAGE))
            .and_then(gui::run),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
