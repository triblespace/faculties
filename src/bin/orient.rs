fn main() -> std::process::ExitCode {
    match faculties::orient::cli::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            std::process::ExitCode::from(faculties::orient::delivery::exit_code(&error))
        }
    }
}
