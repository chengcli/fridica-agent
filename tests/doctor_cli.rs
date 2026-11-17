use serde_json::Value;
use std::process::Command;
fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fridica-agent"));
    command.env_clear().env("PATH", "/nonexistent");
    command
}
#[test]
fn doctor_reports_both_services_and_fails_when_unconfigured() {
    let result = command()
        .args(["doctor", "--json", "--timeout", "1"])
        .env("GH_TOKEN", "private-test-token\n")
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    let checks = report["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0]["service"], "github");
    assert_eq!(checks[1]["service"], "alphaxiv");
    assert_eq!(
        checks[0]["status"],
        if cfg!(feature = "github") {
            "failed"
        } else {
            "not_built"
        }
    );
    assert_eq!(
        checks[1]["status"],
        if cfg!(feature = "search") {
            "failed"
        } else {
            "not_built"
        }
    );
    assert!(!String::from_utf8(result.stdout)
        .unwrap()
        .contains("private-test-token"));
    assert!(result.stderr.is_empty());
}
#[test]
fn selected_service_and_custom_github_variable_are_respected() {
    let result = command()
        .args([
            "doctor",
            "--only",
            "github",
            "--json",
            "--github-token-env",
            "WORKER_GH",
        ])
        .env("WORKER_GH", "private-test-token\n")
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["checks"].as_array().unwrap().len(), 1);
    assert_eq!(report["checks"][0]["service"], "github");
    assert!(!String::from_utf8(result.stdout)
        .unwrap()
        .contains("private-test-token"));
}
#[test]
fn malformed_arguments_fail_before_probing_and_are_not_echoed() {
    for args in [
        vec!["doctor", "--timeout", "0"],
        vec!["doctor", "--only", "private-invalid-value"],
        vec!["doctor", "--github-token-env", "private-invalid-value"],
    ] {
        let result = command().args(args).output().unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
        assert!(!String::from_utf8(result.stderr)
            .unwrap()
            .contains("private-invalid-value"));
    }
    assert!(command()
        .args(["doctor", "--help"])
        .output()
        .unwrap()
        .status
        .success());
}
