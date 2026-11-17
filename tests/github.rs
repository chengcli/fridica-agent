#![cfg(feature = "github")]

use fridica_agent::github::{tools, Server};
use serde_json::{json, Value};
use wiremock::{
    matchers::{body_json, header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

const HEAD: &str = "1111111111111111111111111111111111111111";
const TREE: &str = "2222222222222222222222222222222222222222";
const NEW: &str = "3333333333333333333333333333333333333333";
async fn server(http: &MockServer, readonly: bool) -> Server {
    let client = octocrab::Octocrab::builder()
        .base_uri(http.uri())
        .unwrap()
        .personal_token("test-private-token".to_owned())
        .build()
        .unwrap();
    let mut server = Server::new(readonly, std::env::current_dir().unwrap()).with_client(client);
    let response=server.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),"UNUSED").await.unwrap();
    assert_eq!(response["result"]["capabilities"], json!({"tools":{}}));
    assert!(server
        .handle(
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            "UNUSED"
        )
        .await
        .is_none());
    server
}
async fn call(s: &mut Server, name: &str, mut args: Value) -> Value {
    args["owner"] = json!("o");
    args["repo"] = json!("r");
    s.handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":args}}),"UNUSED").await.unwrap()
}
async fn mock(http: &MockServer, verb: &str, route: &str, body: Value, result: Value) {
    let mut m = Mock::given(method(verb))
        .and(path(route))
        .and(header("authorization", "Bearer test-private-token"));
    if verb != "GET" {
        m = m.and(body_json(body));
    }
    m.respond_with(ResponseTemplate::new(200).set_body_json(result))
        .expect(1)
        .mount(http)
        .await;
}
#[tokio::test]
async fn read_only_catalog_and_dispatch_refuse_every_write_without_network() {
    let http = MockServer::start().await;
    let mut s = server(&http, true).await;
    let list = s
        .handle(
            json!({"jsonrpc":"2.0","id":"list","method":"tools/list"}),
            "UNUSED",
        )
        .await
        .unwrap();
    assert_eq!(
        list["result"]["tools"].as_array().unwrap().len(),
        tools::READ.len()
    );
    for name in tools::WRITE {
        assert_eq!(call(&mut s, name, json!({})).await["error"]["code"], -32602);
    }
    assert_eq!(
        call(&mut s, "merge_pull_request", json!({})).await["error"]["code"],
        -32602
    );
    assert!(http.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn identity_paginated_reads_and_comments_use_expected_endpoints() {
    let http = MockServer::start().await;
    let mut s = server(&http, false).await;
    mock(
        &http,
        "GET",
        "/user",
        json!(null),
        json!({"login":"reviewer"}),
    )
    .await;
    assert_eq!(
        call(&mut s, "get_me", json!({})).await["result"]["isError"],
        false
    );
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/7/files"))
        .and(query_param("page", "2"))
        .and(query_param("per_page", "50"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{ "filename":"file.rs" }])))
        .expect(1)
        .mount(&http)
        .await;
    assert_eq!(
        call(
            &mut s,
            "pull_request_read",
            json!({"pullNumber":7,"method":"get_files","page":2,"perPage":50})
        )
        .await["result"]["isError"],
        false
    );
    mock(
        &http,
        "POST",
        "/repos/o/r/issues/7/comments",
        json!({"body":"finding"}),
        json!({"html_url":"https://github.com/o/r/pull/7#comment"}),
    )
    .await;
    let r = call(
        &mut s,
        "add_issue_comment",
        json!({"issue_number":7,"body":"finding"}),
    )
    .await;
    assert!(r.to_string().contains("#comment"));
}
#[tokio::test]
async fn review_pins_inspected_head_and_rejects_a_stale_one() {
    let http = MockServer::start().await;
    let mut s = server(&http, false).await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"head":{"sha":HEAD}})))
        .expect(2)
        .mount(&http)
        .await;
    mock(
        &http,
        "POST",
        "/repos/o/r/pulls/7/reviews",
        json!({"commit_id":HEAD,"event":"APPROVE","body":"checked"}),
        json!({"state":"APPROVED","html_url":"https://github.com/o/r/pull/7#review"}),
    )
    .await;
    assert_eq!(call(&mut s,"pull_request_review_write",json!({"method":"create","pullNumber":7,"commitID":HEAD,"event":"APPROVE","body":"checked"})).await["result"]["isError"],false);
    let stale = call(
        &mut s,
        "pull_request_review_write",
        json!({"method":"create","pullNumber":7,"commitID":NEW,"event":"APPROVE"}),
    )
    .await;
    assert_eq!(stale["result"]["isError"], true);
    assert!(stale.to_string().contains("head changed"));
}
#[tokio::test]
async fn pending_review_comment_uses_graphql_variables() {
    let http = MockServer::start().await;
    let mut s = server(&http, false).await;
    mock(
        &http,
        "GET",
        "/repos/o/r/pulls/7/reviews/9",
        json!(null),
        json!({"node_id":"PRR_test","state":"PENDING","commit_id":HEAD}),
    )
    .await;
    mock(
        &http,
        "GET",
        "/repos/o/r/pulls/7",
        json!(null),
        json!({"head":{"sha":HEAD}}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data":{"addPullRequestReviewThread":{"thread":{"id":"thread"}}}}),
        ))
        .expect(1)
        .mount(&http)
        .await;
    assert_eq!(
        call(
            &mut s,
            "add_comment_to_pending_review",
            json!({"pullNumber":7,"reviewId":9,"path":"a.rs","line":3,"body":"check this"})
        )
        .await["result"]["isError"],
        false
    );
    let req = http.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&req.last().unwrap().body).unwrap();
    assert_eq!(
        body["variables"],
        json!({"review":"PRR_test","path":"a.rs","line":3,"side":"RIGHT","body":"check this"})
    );
}
#[tokio::test]
async fn multi_file_commit_is_one_non_force_ref_update() {
    let http = MockServer::start().await;
    let mut s = server(&http, false).await;
    mock(
        &http,
        "GET",
        "/repos/o/r/git/ref/heads/task",
        json!(null),
        json!({"object":{"sha":HEAD}}),
    )
    .await;
    mock(
        &http,
        "GET",
        &format!("/repos/o/r/git/commits/{HEAD}"),
        json!(null),
        json!({"tree":{"sha":TREE}}),
    )
    .await;
    mock(
        &http,
        "GET",
        &format!("/repos/o/r/git/trees/{TREE}"),
        json!(null),
        json!({"tree":[{"path":"a.rs","mode":"100755"}],"truncated":false}),
    )
    .await;
    mock(&http,"POST","/repos/o/r/git/trees",json!({"base_tree":TREE,"tree":[{"path":"a.rs","mode":"100755","type":"blob","content":"new"}]}),json!({"sha":NEW})).await;
    mock(
        &http,
        "POST",
        "/repos/o/r/git/commits",
        json!({"message":"fix","tree":NEW,"parents":[HEAD]}),
        json!({"sha":NEW}),
    )
    .await;
    mock(
        &http,
        "PATCH",
        "/repos/o/r/git/refs/heads/task",
        json!({"sha":NEW,"force":false}),
        json!({"object":{"sha":NEW}}),
    )
    .await;
    assert_eq!(call(&mut s,"push_files",json!({"branch":"task","expectedHead":HEAD,"message":"fix","files":[{"path":"a.rs","content":"new"}]})).await["result"]["isError"],false);
}
#[tokio::test]
async fn failures_are_not_retried_and_diagnostics_do_not_leak_credentials() {
    let http = MockServer::start().await;
    let mut s = server(&http, false).await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/7/comments"))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(json!({"message":"test-private-token"})),
        )
        .expect(1)
        .mount(&http)
        .await;
    let result = call(
        &mut s,
        "add_issue_comment",
        json!({"issue_number":7,"body":"finding"}),
    )
    .await;
    assert_eq!(result["result"]["isError"], true);
    assert!(!result.to_string().contains("test-private-token"));
    assert!(result.to_string().contains("uncertain"));
}
#[tokio::test]
async fn invalid_arguments_and_notification_writes_never_reach_github() {
    let http = MockServer::start().await;
    let mut s = server(&http, false).await;
    assert_eq!(
        call(
            &mut s,
            "add_issue_comment",
            json!({"issue_number":0,"body":"x"})
        )
        .await["error"]["code"],
        -32602
    );
    assert_eq!(
        call(&mut s, "push_files", json!({"force":true})).await["error"]["code"],
        -32602
    );
    assert_eq!(
        call(
            &mut s,
            "create_or_update_file",
            json!({"branch":"task","message":"x","path":"../escape","content":"eA=="})
        )
        .await["result"]["isError"],
        true
    );
    assert!(s.handle(json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"add_issue_comment","arguments":{"owner":"o","repo":"r","issue_number":1,"body":"x"}}}),"UNUSED").await.is_none());
    assert!(http.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn gitoxide_reads_local_head_without_a_git_subprocess_in_the_server() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    assert!(std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "initial"
        ])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    let mut s = Server::new(true, tmp.path().to_owned());
    s.handle(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        "UNUSED",
    )
    .await;
    s.handle(
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        "UNUSED",
    )
    .await;
    let value = call(&mut s, "git_context", json!({"path":"repo"})).await;
    assert_eq!(value["result"]["isError"], false);
    let context: Value =
        serde_json::from_str(value["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(context["head"].as_str().unwrap().len(), 40);
    let escaped = call(&mut s, "git_context", json!({"path":".."})).await;
    assert_eq!(escaped["result"]["isError"], true);
    assert!(escaped.to_string().contains("inside the worker workspace"));
}

#[tokio::test]
async fn file_and_pull_request_writes_send_exact_github_payloads() {
    let http = MockServer::start().await;
    let mut s = server(&http, false).await;
    let cases = [
        (
            "create_or_update_file",
            "PUT",
            "/repos/o/r/contents/a.txt",
            json!({"branch":"task","path":"a.txt","message":"update","content":"aGk=","sha":HEAD}),
            json!({"branch":"task","message":"update","content":"aGk=","sha":HEAD}),
        ),
        (
            "delete_file",
            "DELETE",
            "/repos/o/r/contents/a.txt",
            json!({"branch":"task","path":"a.txt","message":"delete","sha":HEAD}),
            json!({"branch":"task","message":"delete","sha":HEAD}),
        ),
        (
            "create_pull_request",
            "POST",
            "/repos/o/r/pulls",
            json!({"title":"fix","head":"task","base":"main","draft":true}),
            json!({"title":"fix","head":"task","base":"main","draft":true}),
        ),
        (
            "update_pull_request",
            "PATCH",
            "/repos/o/r/pulls/7",
            json!({"pullNumber":7,"body":"updated"}),
            json!({"body":"updated"}),
        ),
        (
            "add_reply_to_pull_request_comment",
            "POST",
            "/repos/o/r/pulls/7/comments/8/replies",
            json!({"pullNumber":7,"commentId":8,"body":"fixed"}),
            json!({"body":"fixed"}),
        ),
    ];
    for (tool, verb, route, args, payload) in cases {
        mock(&http, verb, route, payload, json!({"id":1})).await;
        let response = call(&mut s, tool, args).await;
        assert_eq!(
            response["result"]["isError"],
            false,
            "{tool}: {response}; requests: {:?}",
            http.received_requests().await.unwrap()
        );
    }
}

#[tokio::test]
async fn pending_review_submission_and_ci_checks_use_the_inspected_head() {
    let http = MockServer::start().await;
    let mut s = server(&http, false).await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"head":{"sha":HEAD}})))
        .expect(2)
        .mount(&http)
        .await;
    mock(
        &http,
        "GET",
        "/repos/o/r/pulls/7/reviews/9",
        json!(null),
        json!({"state":"PENDING","commit_id":HEAD}),
    )
    .await;
    mock(
        &http,
        "POST",
        "/repos/o/r/pulls/7/reviews/9/events",
        json!({"event":"REQUEST_CHANGES","body":"fix the test"}),
        json!({"state":"CHANGES_REQUESTED"}),
    )
    .await;
    assert_eq!(call(&mut s,"pull_request_review_write",json!({"method":"submit_pending","pullNumber":7,"reviewId":9,"event":"REQUEST_CHANGES","body":"fix the test"})).await["result"]["isError"],false);
    mock(
        &http,
        "GET",
        &format!("/repos/o/r/commits/{HEAD}/check-runs"),
        json!(null),
        json!({"check_runs":[{"conclusion":"failure"}]}),
    )
    .await;
    let response = call(
        &mut s,
        "pull_request_read",
        json!({"method":"get_check_runs","pullNumber":7}),
    )
    .await;
    assert_eq!(response["result"]["isError"], false);
    assert!(response.to_string().contains("failure"));
}
