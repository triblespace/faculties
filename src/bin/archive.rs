fn main() -> std::process::ExitCode {
    let result = faculties::archive::cli::run();
    if let Err(error) = &result {
        eprintln!("Error: {error:?}");
    }
    faculties::archive::cli::exit_status(&result).into()
}
