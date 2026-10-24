fn main() -> std::process::ExitCode {
    match faculties::archive::cli::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            std::process::ExitCode::from(
                if error.is::<faculties::archive::sessions::SessionBusy>() {
                    3
                } else {
                    1
                },
            )
        }
    }
}
