use super::*;
use serde_json::{json, Value};
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

fn response(request: &Request) -> ResponseTemplate {
    let v: Value = request.body_json().unwrap();
    if v.get("id").is_none() {
        return ResponseTemplate::new(202);
    }
    let result = match v["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"mock-alphaxiv","version":"1"}})
        }
        "tools/list" if v["params"]["cursor"] == "next" => {
            json!({"tools":[{"name":"get_paper_content","inputSchema":{"type":"object"}}]})
        }
        "tools/list" => {
            json!({"tools":[{"name":"discover_papers","description":"Discover papers","inputSchema":{"type":"object","properties":{"question":{"type":"string"}}}},{"name":"save_papers_to_folder","inputSchema":{"type":"object"}}],"nextCursor":"next"})
        }
        "tools/call" => {
            assert_eq!(v["params"]["name"], "discover_papers");
            assert_eq!(v["params"]["arguments"]["question"], "cloud microphysics");
            // Exercise Streamable HTTP's SSE response path as well as JSON.
            return ResponseTemplate::new(200)
                .set_body_raw(format!("event: message\ndata: {}\n\n", json!({"jsonrpc":"2.0","id":v["id"],"result":{"content":[{"type":"text","text":"https://arxiv.org/abs/2307.12307"}]}})), "text/event-stream");
        }
        other => panic!("unexpected method {other}"),
    };
    ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":v["id"],"result":result}))
}

#[tokio::test]
async fn http_discovery_pagination_sse_and_write_filtering_work_through_mcp() {
    let http = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp/v1"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(response)
        .mount(&http)
        .await;
    let (bridge, upstream) = connect(&format!("{}/mcp/v1", http.uri()), "test-key".into())
        .await
        .unwrap();
    assert_eq!(bridge.tools.len(), 2);
    assert!(bridge
        .get_info()
        .instructions
        .unwrap()
        .contains("fullText=true"));
    let (a, b) = tokio::io::duplex(65536);
    let task = tokio::spawn(async move { bridge.serve(a).await.unwrap().waiting().await.unwrap() });
    let local = ClientConfig::default()
        .with_protocol_version(ProtocolVersion::V_2025_06_18)
        .serve(b)
        .await
        .unwrap();
    let tools = local.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 2);
    assert!(!tools.iter().any(|t| t.name == "save_papers_to_folder"));
    let before = http.received_requests().await.unwrap().len();
    assert!(local
        .call_tool_once(CallToolRequestParams::new("save_papers_to_folder"))
        .await
        .is_err());
    assert_eq!(http.received_requests().await.unwrap().len(), before);
    let mut call = CallToolRequestParams::new("discover_papers");
    call.arguments = Some(
        json!({"question":"cloud microphysics"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let result = local.call_tool_once(call.clone()).await.unwrap();
    let CallToolResponse::Complete(result) = result else {
        panic!("expected complete research response")
    };
    assert!(serde_json::to_string(&result)
        .unwrap()
        .contains("arxiv.org"));
    http.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("test-key private failure"))
        .expect(1)
        .mount(&http)
        .await;
    let error = local.call_tool_once(call).await.unwrap_err().to_string();
    assert!(!error.contains("test-key"));
    assert!(!error.contains("private failure"));
    let _ = local.cancel().await;
    task.await.unwrap();
    let _ = upstream.cancel().await;
}

#[tokio::test]
async fn missing_invalid_credentials_and_redirects_fail_without_disclosure() {
    assert!(connect(ENDPOINT, "\nsecret".into()).await.is_err());
    let http = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", destination.uri()))
        .mount(&http)
        .await;
    let result = connect(&http.uri(), "test-key".into()).await;
    let error = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("followed redirect"),
    };
    assert!(!error.contains("test-key"));
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[test]
fn attachment_is_explicit_and_contains_only_a_credential_variable_name() {
    let spec = alphaxiv("/opt/bin/fridica-agent-alphaxiv");
    assert!(spec.valid());
    assert!(!spec.auto_approve);
    assert_eq!(spec.env_vars, vec!["ALPHAXIV_TOKEN"]);
    assert!(spec.args.is_empty());
}

#[tokio::test]
async fn doctor_probe_discovers_tools_without_performing_research() {
    let http = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(response)
        .mount(&http)
        .await;
    let tools = probe_at(&http.uri(), "test-key".into()).await.unwrap();
    assert!(tools.iter().any(|tool| tool == "discover_papers"));
    assert!(http
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| {
            let value: Value = request.body_json().unwrap();
            ["initialize", "notifications/initialized", "tools/list"]
                .contains(&value["method"].as_str().unwrap())
        }));
}
