#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    if std::env::args_os().len() != 1 {
        eprintln!("Usage: fridica-agent-alphaxiv (set ALPHAXIV_TOKEN on this worker)");
        return std::process::ExitCode::from(2);
    }
    match fridica_agent::alphaxiv::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fridica-agent-alphaxiv: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
