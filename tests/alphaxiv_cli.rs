#![cfg(feature = "alphaxiv")]
use std::process::Command;

#[test]
fn missing_key_exits_with_actionable_diagnostic_without_connecting() {
    let result = Command::new(env!("CARGO_BIN_EXE_fridica-agent-alphaxiv"))
        .env_remove("ALPHAXIV_TOKEN")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8(result.stderr)
        .unwrap()
        .contains("set ALPHAXIV_TOKEN"));
}

#[test]
fn arguments_never_accept_or_echo_secrets() {
    let result = Command::new(env!("CARGO_BIN_EXE_fridica-agent-alphaxiv"))
        .args(["--token", "private-test-value"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(!String::from_utf8(result.stderr)
        .unwrap()
        .contains("private-test-value"));
}
