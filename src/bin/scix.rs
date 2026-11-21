fn main() -> std::process::ExitCode {
    if std::env::args_os().len() != 1 {
        eprintln!("Usage: fridica-agent-scix (set ADS_TOKEN on this worker)");
        return std::process::ExitCode::from(2);
    }
    match fridica_agent::scix::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fridica-agent-scix: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
