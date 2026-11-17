use anyhow::{bail, Context, Result};
use base64::Engine;
use octocrab::Octocrab;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{json, Value};
use std::path::Path;

// Keep RFC 3986 unreserved characters; encode separators and query delimiters.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn text<'a>(a: &'a Value, key: &str) -> Result<&'a str> {
    a[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing or invalid string argument")
}
fn number(a: &Value, key: &str) -> Result<u64> {
    a[key]
        .as_u64()
        .filter(|n| *n > 0)
        .context("missing or invalid positive integer")
}
fn segment(s: &str) -> Result<String> {
    if s.is_empty() || s == "." || s == ".." || s.chars().any(char::is_control) {
        bail!("invalid path segment");
    }
    Ok(utf8_percent_encode(s, PATH_SEGMENT).to_string())
}
fn file_path(s: &str) -> Result<String> {
    if s.starts_with('/') || s.contains('\\') {
        bail!("invalid repository path");
    }
    s.split('/')
        .map(segment)
        .collect::<Result<Vec<_>>>()
        .map(|p| p.join("/"))
}
fn sha(s: &str) -> Result<&str> {
    if s.len() != 40 {
        bail!("expected full SHA-1 commit or blob ID");
    }
    gix::hash::ObjectId::from_hex(s.as_bytes()).context("invalid object ID")?;
    Ok(s)
}
fn fields(a: &Value, keys: &[&str]) -> Value {
    let mut v = json!({});
    for key in keys {
        if let Some(value) = a.get(*key) {
            v[*key] = value.clone();
        }
    }
    v
}
fn page(a: &Value) -> Result<Value> {
    let p = a
        .get("page")
        .map(|_| number(a, "page"))
        .transpose()?
        .unwrap_or(1);
    let n = a
        .get("perPage")
        .map(|_| number(a, "perPage"))
        .transpose()?
        .unwrap_or(30);
    if n > 100 {
        bail!("perPage exceeds 100");
    }
    Ok(json!({"page":p,"per_page":n}))
}
fn branch(s: &str) -> Result<&str> {
    // Ref validation is shared with gitoxide, not a shell or `git check-ref-format`.
    let full = format!("refs/heads/{s}");
    gix::refs::FullName::try_from(full.as_str()).context("invalid branch")?;
    Ok(s)
}
async fn get(client: &Octocrab, route: &str, query: Value) -> Result<Value> {
    Ok(client.get(route, Some(&query)).await?)
}
async fn post(client: &Octocrab, route: &str, body: Value) -> Result<Value> {
    Ok(client.post(route, Some(&body)).await?)
}
async fn head(client: &Octocrab, root: &str, name: &str) -> Result<Value> {
    get(
        client,
        &format!("{root}/git/ref/heads/{}", segment(branch(name)?)?),
        json!({}),
    )
    .await
}
async fn check_review_head(client: &Octocrab, pull: &str, commit: &str) -> Result<()> {
    sha(commit)?;
    let pr = get(client, pull, json!({})).await?;
    if pr["head"]["sha"] != commit {
        bail!("PR head changed; inspect the new head before reviewing");
    }
    Ok(())
}

pub async fn call(client: &Octocrab, name: &str, a: &Value, workspace: &Path) -> Result<Value> {
    if !a.is_object() {
        bail!("arguments must be an object");
    }
    if name == "get_me" {
        return get(client, "/user", json!({})).await;
    }
    if name == "git_context" {
        return git_context(a, workspace);
    }
    if name == "search_pull_requests" {
        let mut query = page(a)?;
        query["q"] = json!(format!("{} is:pr", text(a, "query")?));
        return get(client, "/search/issues", query).await;
    }
    let root = format!(
        "/repos/{}/{}",
        segment(text(a, "owner")?)?,
        segment(text(a, "repo")?)?
    );
    match name {
        "get_file_contents" => {
            let path = a
                .get("path")
                .map(|value| value.as_str().context("invalid path"))
                .transpose()?
                .unwrap_or("");
            let path = if path.is_empty() {
                String::new()
            } else {
                format!("/{}", file_path(path)?)
            };
            get(
                client,
                &format!("{root}/contents{path}"),
                fields(a, &["ref"]),
            )
            .await
        }
        "get_commit" => {
            get(
                client,
                &format!("{root}/commits/{}", segment(text(a, "sha")?)?),
                page(a)?,
            )
            .await
        }
        "list_branches" => get(client, &format!("{root}/branches"), page(a)?).await,
        "list_pull_requests" => {
            let mut query = fields(a, &["state", "head", "base"]);
            query
                .as_object_mut()
                .unwrap()
                .extend(page(a)?.as_object().unwrap().clone());
            get(client, &format!("{root}/pulls"), query).await
        }
        "issue_read" => {
            let suffix = match text(a, "method")? {
                "get" => "",
                "get_comments" => "/comments",
                _ => bail!("unsupported issue read method"),
            };
            get(
                client,
                &format!("{root}/issues/{}{suffix}", number(a, "issue_number")?),
                page(a)?,
            )
            .await
        }
        "pull_request_read" => {
            let pull = format!("{root}/pulls/{}", number(a, "pullNumber")?);
            match text(a, "method")? {
                "get" => get(client, &pull, json!({})).await,
                "get_files" => get(client, &format!("{pull}/files"), page(a)?).await,
                "get_reviews" => get(client, &format!("{pull}/reviews"), page(a)?).await,
                "get_comments" => get(client, &format!("{pull}/comments"), page(a)?).await,
                "get_diff" => {
                    // Octocrab's typed PR diff helper handles the GitHub media type.
                    let diff = client
                        .pulls(text(a, "owner")?, text(a, "repo")?)
                        .get_diff(number(a, "pullNumber")?)
                        .await?;
                    Ok(json!({"diff":diff}))
                }
                "get_status" | "get_check_runs" => {
                    let endpoint = if text(a, "method")? == "get_check_runs" {
                        "check-runs"
                    } else {
                        "status"
                    };
                    let pr = get(client, &pull, json!({})).await?;
                    get(
                        client,
                        &format!(
                            "{root}/commits/{}/{endpoint}",
                            segment(text(&pr["head"], "sha")?)?
                        ),
                        page(a)?,
                    )
                    .await
                }
                _ => bail!("unsupported pull request read method"),
            }
        }
        "add_issue_comment" => {
            text(a, "body")?;
            post(
                client,
                &format!("{root}/issues/{}/comments", number(a, "issue_number")?),
                fields(a, &["body"]),
            )
            .await
        }
        "add_reply_to_pull_request_comment" => {
            text(a, "body")?;
            post(
                client,
                &format!(
                    "{root}/pulls/{}/comments/{}/replies",
                    number(a, "pullNumber")?,
                    number(a, "commentId")?
                ),
                fields(a, &["body"]),
            )
            .await
        }
        "pull_request_review_write" => review(client, &root, a).await,
        "add_comment_to_pending_review" => pending_comment(client, &root, a).await,
        "create_branch" => {
            let base = head(client, &root, text(a, "from_branch")?).await?;
            post(client,&format!("{root}/git/refs"),json!({"ref":format!("refs/heads/{}",branch(text(a,"branch")?)?),"sha":sha(text(&base["object"],"sha")?)?})).await
        }
        "create_or_update_file" | "delete_file" => {
            branch(text(a, "branch")?)?;
            text(a, "message")?;
            let route = format!("{root}/contents/{}", file_path(text(a, "path")?)?);
            let body = fields(a, &["branch", "message", "sha", "content"]);
            if name == "delete_file" {
                sha(text(a, "sha")?)?;
                Ok(client
                    .delete(route, Some(&fields(a, &["branch", "message", "sha"])))
                    .await?)
            } else {
                if let Some(s) = a.get("sha") {
                    sha(s.as_str().context("invalid sha")?)?;
                }
                base64::engine::general_purpose::STANDARD
                    .decode(a["content"].as_str().context("content must be base64")?)
                    .context("content must be base64")?;
                Ok(client.put(route, Some(&body)).await?)
            }
        }
        "push_files" => push_files(client, &root, a).await,
        "create_pull_request" => {
            for key in ["title", "head", "base"] {
                text(a, key)?;
            }
            post(
                client,
                &format!("{root}/pulls"),
                fields(a, &["title", "head", "base", "body", "draft"]),
            )
            .await
        }
        "update_pull_request" => {
            let body = fields(a, &["title", "body", "base", "state"]);
            if body.as_object().unwrap().is_empty() {
                bail!("no PR updates supplied");
            }
            Ok(client
                .patch(
                    format!("{root}/pulls/{}", number(a, "pullNumber")?),
                    Some(&body),
                )
                .await?)
        }
        _ => bail!("unknown tool"),
    }
}

async fn review(client: &Octocrab, root: &str, a: &Value) -> Result<Value> {
    let pull = format!("{root}/pulls/{}", number(a, "pullNumber")?);
    let method = text(a, "method")?;
    let event = a.get("event").map(|_| text(a, "event")).transpose()?;
    if event.is_some_and(|e| !["APPROVE", "REQUEST_CHANGES", "COMMENT"].contains(&e)) {
        bail!("invalid review event");
    }
    if event == Some("REQUEST_CHANGES") || event == Some("COMMENT") {
        text(a, "body")?;
    }
    match method {
        "create" => {
            let commit = text(a, "commitID")?;
            check_review_head(client, &pull, commit).await?;
            let mut body = fields(a, &["body", "event"]);
            body["commit_id"] = json!(commit);
            post(client, &format!("{pull}/reviews"), body).await
        }
        "submit_pending" => {
            let id = number(a, "reviewId")?;
            text(a, "event")?;
            let review = get(client, &format!("{pull}/reviews/{id}"), json!({})).await?;
            if review["state"] != "PENDING" {
                bail!("review is not pending");
            }
            check_review_head(client, &pull, text(&review, "commit_id")?).await?;
            post(
                client,
                &format!("{pull}/reviews/{id}/events"),
                fields(a, &["body", "event"]),
            )
            .await
        }
        "delete_pending" => Ok(client
            .delete(
                format!("{pull}/reviews/{}", number(a, "reviewId")?),
                None::<&Value>,
            )
            .await?),
        _ => bail!("unsupported review method"),
    }
}
async fn pending_comment(client: &Octocrab, root: &str, a: &Value) -> Result<Value> {
    let pull = format!("{root}/pulls/{}", number(a, "pullNumber")?);
    let review = get(
        client,
        &format!("{pull}/reviews/{}", number(a, "reviewId")?),
        json!({}),
    )
    .await?;
    if review["state"] != "PENDING" {
        bail!("review is not pending");
    }
    check_review_head(client, &pull, text(&review, "commit_id")?).await?;
    let side = a.get("side").and_then(Value::as_str).unwrap_or("RIGHT");
    if !["LEFT", "RIGHT"].contains(&side) {
        bail!("invalid diff side");
    }
    file_path(text(a, "path")?)?;
    let variables = json!({"review":text(&review,"node_id")?,"path":text(a,"path")?,"line":number(a,"line")?,"side":side,"body":text(a,"body")?});
    Ok(client.graphql(&json!({"query":"mutation($review:ID!,$path:String!,$line:Int!,$side:DiffSide!,$body:String!){addPullRequestReviewThread(input:{pullRequestReviewId:$review,path:$path,line:$line,side:$side,body:$body}){thread{id comments(first:1){nodes{id url body}}}}}","variables":variables})).await?)
}
async fn push_files(client: &Octocrab, root: &str, a: &Value) -> Result<Value> {
    let name = branch(text(a, "branch")?)?;
    let expected = sha(text(a, "expectedHead")?)?;
    let message = text(a, "message")?;
    let files = a["files"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 100)
        .context("provide 1 to 100 files")?;
    let mut paths = std::collections::HashSet::new();
    let mut tree = Vec::new();
    for file in files {
        let path = text(file, "path")?;
        file_path(path)?;
        if !paths.insert(path) {
            bail!("duplicate file path");
        }
        let content = file["content"]
            .as_str()
            .context("file content must be UTF-8 text")?;
        tree.push(json!({"path":path,"mode":"100644","type":"blob","content":content}));
    }
    let current = head(client, root, name).await?;
    if current["object"]["sha"] != expected {
        bail!("branch head changed; inspect it before committing");
    }
    let commit = get(client, &format!("{root}/git/commits/{expected}"), json!({})).await?;
    let base_tree = sha(text(&commit["tree"], "sha")?)?;
    let existing = get(
        client,
        &format!("{root}/git/trees/{base_tree}"),
        json!({"recursive":"1"}),
    )
    .await?;
    if existing["truncated"] == true {
        bail!("repository tree is truncated; use individual file updates");
    }
    let entries = existing["tree"]
        .as_array()
        .context("invalid Git tree response")?;
    for entry in &mut tree {
        if let Some(old) = entries.iter().find(|old| old["path"] == entry["path"]) {
            match old["mode"].as_str() {
                Some("100644" | "100755") => entry["mode"] = old["mode"].clone(),
                _ => bail!("push_files cannot replace a symlink, tree or submodule"),
            }
        }
    }
    let created = post(
        client,
        &format!("{root}/git/trees"),
        json!({"base_tree":base_tree,"tree":tree}),
    )
    .await?;
    let commit = post(
        client,
        &format!("{root}/git/commits"),
        json!({"message":message,"tree":sha(text(&created,"sha")?)?,"parents":[expected]}),
    )
    .await?;
    // A non-fast-forward response protects a branch that moved during these calls.
    let updated: Value = client
        .patch(
            format!("{root}/git/refs/heads/{}", segment(name)?),
            Some(&json!({"sha":sha(text(&commit,"sha")?)?,"force":false})),
        )
        .await?;
    Ok(json!({"commit":commit,"reference":updated}))
}

pub fn git_context(a: &Value, workspace: &Path) -> Result<Value> {
    let base = workspace.canonicalize()?;
    let requested = a
        .get("path")
        .map(|_| text(a, "path"))
        .transpose()?
        .unwrap_or(".");
    let path = base.join(requested).canonicalize()?;
    if !path.starts_with(&base) {
        bail!("repository must be inside the worker workspace");
    }
    let repo = gix::open_opts(&path, gix::open::Options::isolated())?;
    if !repo.git_dir().canonicalize()?.starts_with(&base) {
        bail!("Git directory must be inside the worker workspace");
    }
    let id = repo.head_id()?.to_string();
    let name = repo.head_name()?.map(|n| n.as_bstr().to_string());
    Ok(json!({"head":id,"reference":name}))
}
