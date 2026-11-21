fn main() -> std::process::ExitCode {
    if std::env::args_os().len() != 1 {
        eprintln!("Usage: fridica-agent-overleaf");
        return std::process::ExitCode::from(2);
    }
    match fridica_agent::overleaf::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fridica-agent-overleaf: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
