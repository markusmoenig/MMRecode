//! `MMRecode` command-line entry point.

fn main() {
    let command = std::env::args().nth(1);
    match command.as_deref() {
        None | Some("help" | "--help" | "-h") => print_help(),
        Some("version" | "--version" | "-V") => {
            println!("mmrecode {}", env!("CARGO_PKG_VERSION"));
        }
        Some(other) => {
            eprintln!("mmrecode: command '{other}' is not implemented yet");
            eprintln!("run 'mmrecode help' for the planned command set");
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!(
        "MMRecode media-codec tools\n\n\
         Usage: mmrecode <command>\n\n\
         Planned commands:\n  encode\n  decode\n  inspect\n  verify\n  compare\n  edit\n  benchmark"
    );
}
