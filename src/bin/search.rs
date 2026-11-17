#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    if std::env::args_os().len() != 1 {
        eprintln!("Usage: fridica-agent-search (set ALPHAXIV_TOKEN on this worker)");
        return std::process::ExitCode::from(2);
    }
    match fridica_agent::search::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fridica-agent-search: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
