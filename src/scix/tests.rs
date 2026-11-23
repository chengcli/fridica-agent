use super::*;
use wiremock::{
    matchers::{body_json, header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

async fn client(ads: Ads) -> rmcp::service::RunningService<rmcp::RoleClient, ClientConfig> {
    let (a, b) = tokio::io::duplex(65536);
    tokio::spawn(async move { ads.serve(a).await.unwrap().waiting().await.unwrap() });
    ClientConfig::default().serve(b).await.unwrap()
}

async fn call(
    local: &rmcp::service::RunningService<rmcp::RoleClient, ClientConfig>,
    name: &'static str,
    args: Value,
) -> (bool, String) {
    let mut request = CallToolRequestParams::new(name);
    request.arguments = args.as_object().cloned();
    let CallToolResponse::Complete(result) = local.call_tool_once(request).await.unwrap() else {
        panic!("expected a complete result")
    };
    let text = result.content[0].as_text().unwrap().text.clone();
    (result.is_error == Some(true), text)
}

#[tokio::test]
async fn tools_reach_the_ads_api_through_mcp() {
    let http = MockServer::start().await;
    let hit = json!({"response": {"numFound": 1, "docs": [{"bibcode": "2023Natur.616..443D"}]}});
    Mock::given(method("GET"))
        .and(path("/search/query"))
        .and(header("authorization", "Bearer test-key"))
        .and(query_param("q", "title:\"DART\""))
        .and(query_param("fl", format!("{LIST},abstract")))
        .and(query_param("rows", "10"))
        .and(query_param("sort", "score desc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&hit))
        .mount(&http)
        .await;
    Mock::given(method("GET"))
        .and(path("/search/query"))
        .and(query_param(
            "q",
            "citations(identifier:\"10.1038/s41586-023-05805-2\")",
        ))
        .and(query_param("sort", "citation_count desc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&hit))
        .mount(&http)
        .await;
    Mock::given(method("POST"))
        .and(path("/export/bibtex"))
        .and(body_json(
            json!({"bibcode": ["2023Natur.616..443D"], "keyformat": ["%1H%Y"]}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"export": "@ARTICLE{Daly2023,}"})),
        )
        .mount(&http)
        .await;
    Mock::given(method("GET"))
        .and(path("/search/query"))
        .and(query_param("fl", "bibcode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&hit))
        .mount(&http)
        .await;
    Mock::given(method("POST"))
        .and(path("/biblib/documents/abc_DEF-1"))
        .and(body_json(
            json!({"bibcode": ["2023Natur.616..443D"], "action": "add"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"number_added": 1})))
        .expect(1)
        .mount(&http)
        .await;

    let local = client(Ads::new(&http.uri(), "test-key".into()).unwrap()).await;
    let tools = local.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), TOOLS.len());
    assert!(local
        .peer_info()
        .unwrap()
        .instructions
        .as_deref()
        .unwrap()
        .contains("ADS syntax"));

    let (error, text) = call(&local, "search", json!({"query": "title:\"DART\""})).await;
    assert!(!error && text.contains("2023Natur.616..443D"), "{text}");
    let (error, _) = call(
        &local,
        "get_citations",
        json!({"bibcode": "10.1038/s41586-023-05805-2"}),
    )
    .await;
    assert!(!error);
    let args =
        json!({"bibcodes": ["2023Natur.616..443D"], "format": "bibtex", "keyformat": "%1H%Y"});
    assert_eq!(
        call(&local, "export", args).await,
        (false, "@ARTICLE{Daly2023,}".into())
    );
    let (error, text) = call(
        &local,
        "add_documents_by_query",
        json!({"library_id": "abc_DEF-1", "query": "DART"}),
    )
    .await;
    assert!(!error && text.contains("\"number_added\":1"), "{text}");

    let before = http.received_requests().await.unwrap().len();
    let (error, text) = call(&local, "get_library", json!({"library_id": "../export"})).await;
    assert!(error && text.contains("library_id"));
    let (error, _) = call(
        &local,
        "export",
        json!({"bibcodes": ["x"], "format": "../metrics"}),
    )
    .await;
    assert!(error);
    assert_eq!(http.received_requests().await.unwrap().len(), before);
    assert!(local
        .call_tool_once(CallToolRequestParams::new("delete_library"))
        .await
        .is_err());
    let _ = local.cancel().await;
}

#[tokio::test]
async fn ads_errors_are_reported_without_the_token() {
    let http = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("q", "bad"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"msg": "syntax error at 'bad'", "code": 400}})),
        )
        .mount(&http)
        .await;
    Mock::given(method("GET"))
        .and(query_param("q", "busy"))
        .respond_with(ResponseTemplate::new(429).insert_header("x-ratelimit-reset", "1790000000"))
        .mount(&http)
        .await;
    Mock::given(method("GET"))
        .and(query_param("q", "year:2000"))
        .respond_with(ResponseTemplate::new(401).set_body_string("test-key rejected"))
        .mount(&http)
        .await;
    Mock::given(method("POST"))
        .and(path("/biblib/documents/gone"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"error": "Library does not exist"})),
        )
        .mount(&http)
        .await;
    let ads = Ads::new(&http.uri(), "test-key".into()).unwrap();
    let remove = json!({"library_id": "gone", "bibcodes": ["x"], "action": "remove"});
    let error = ads
        .call("manage_documents", remove.as_object().unwrap())
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "ADS error 404: Library does not exist");
    let args = |q: &str| json!({"query": q}).as_object().cloned().unwrap();
    let error = ads
        .call("search", &args("bad"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "ADS error 400: syntax error at 'bad'");
    let error = ads
        .call("search", &args("busy"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("limit") && error.contains("1790000000"),
        "{error}"
    );
    let error = ads.check().await.unwrap_err().to_string();
    assert_eq!(error, "ADS rejected the token; check ADS_TOKEN");
}

#[tokio::test]
async fn bad_tokens_and_redirects_fail() {
    assert!(Ads::new(ENDPOINT, "\nsecret".into()).is_err());
    assert!(Ads::new(ENDPOINT, " ".into()).is_err());
    let http = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", destination.uri()))
        .mount(&http)
        .await;
    assert!(Ads::new(&http.uri(), "test-key".into())
        .unwrap()
        .check()
        .await
        .is_err());
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[test]
fn attachment_names_only_the_credential_variable() {
    let spec = scix("/opt/bin/fridica-agent-scix");
    assert!(spec.valid());
    assert!(!spec.auto_approve);
    assert_eq!(spec.env_vars, vec!["ADS_TOKEN"]);
    assert!(spec.args.is_empty());
}
