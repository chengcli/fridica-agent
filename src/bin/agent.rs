use fridica_agent::doctor::{self, Options, Service};
use std::{process::ExitCode, time::Duration};

const USAGE: &str = "Usage: fridica-agent doctor [--only github|alphaxiv|scix|overleaf] [--json] [--timeout 1..120] [--github-token-env NAME]\nChecks target-local credentials and connectivity using read-only probes. Install with --features github,alphaxiv,scix,overleaf to check all services.";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("fridica-agent {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args == ["--help"] || args == ["doctor", "--help"] {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let Some((options, json)) = parse(&args) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let report = doctor::run(&options).await;
    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        for check in &report.checks {
            println!("{}: {:?} — {}", check.service, check.status, check.message);
        }
    }
    if report.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
fn parse(args: &[String]) -> Option<(Options, bool)> {
    if args.first()?.as_str() != "doctor" {
        return None;
    }
    let mut options = Options::default();
    let mut json = false;
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--only" => {
                options.only = Some(match rest.next()?.as_str() {
                    "github" => Service::Github,
                    "alphaxiv" => Service::Alphaxiv,
                    "scix" => Service::Scix,
                    "overleaf" => Service::Overleaf,
                    _ => return None,
                })
            }
            "--timeout" => {
                let seconds: u64 = rest.next()?.parse().ok()?;
                if !(1..=120).contains(&seconds) {
                    return None;
                }
                options.timeout = Duration::from_secs(seconds);
            }
            "--github-token-env" => {
                let name = rest.next()?;
                if name.is_empty()
                    || !name.bytes().enumerate().all(|(i, b)| {
                        b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                    })
                {
                    return None;
                }
                options.github_token_env = name.clone();
            }
            _ => return None,
        }
    }
    Some((options, json))
}
