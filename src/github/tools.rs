use serde_json::{json, Value};

pub const READ: &[&str] = &[
    "get_me",
    "get_file_contents",
    "get_commit",
    "list_branches",
    "list_pull_requests",
    "search_pull_requests",
    "pull_request_read",
    "issue_read",
    "git_context",
];
pub const WRITE: &[&str] = &[
    "add_issue_comment",
    "add_comment_to_pending_review",
    "add_reply_to_pull_request_comment",
    "pull_request_review_write",
    "create_branch",
    "create_or_update_file",
    "delete_file",
    "push_files",
    "create_pull_request",
    "update_pull_request",
];

pub fn catalog(read_only: bool) -> Vec<Value> {
    READ.iter().chain(if read_only { [].iter() } else { WRITE.iter() }).map(|name| {
        let mut properties = json!({"owner":{"type":"string"},"repo":{"type":"string"},"page":{"type":"integer","minimum":1},"perPage":{"type":"integer","minimum":1,"maximum":100}});
        let mut required = vec!["owner", "repo"];
        let mut add = |key: &str, value: Value| { properties[key] = value; };
        let string = || json!({"type":"string"});
        let integer = || json!({"type":"integer","minimum":1});
        let description = match *name {
            "get_me" => { required.clear(); "Read the authenticated GitHub identity." }
            "git_context" => { required.clear(); add("path",string()); "Read the local repository HEAD and branch using gitoxide. Path is relative to the worker directory; no shell or network." }
            "get_file_contents" => { add("path",string()); add("ref",string()); "Read file contents or list a directory at an optional Git ref." }
            "get_commit" => { add("sha",string()); required.push("sha"); "Read a GitHub commit and its diff." }
            "list_branches" => "List repository branches, paginated.",
            "list_pull_requests" => { add("state",json!({"enum":["open","closed","all"]})); add("head",string()); add("base",string()); "List pull requests, paginated." }
            "search_pull_requests" => { required = vec!["query"]; add("query",string()); "Search GitHub pull requests. The query is restricted to pull requests." }
            "pull_request_read" => { add("pullNumber",integer()); add("method",json!({"enum":["get","get_diff","get_files","get_reviews","get_comments","get_status","get_check_runs"]})); required.extend(["pullNumber","method"]); "Read PR metadata, diff, files, reviews, review comments combined commit status or CI check runs." }
            "issue_read" => { add("issue_number",integer()); add("method",json!({"enum":["get","get_comments"]})); required.extend(["issue_number","method"]); "Read an issue or its comments." }
            "add_issue_comment" => { add("issue_number",integer()); add("body",string()); required.extend(["issue_number","body"]); "Post a general issue or PR comment. Do not retry an uncertain write without checking GitHub." }
            "add_reply_to_pull_request_comment" => { add("pullNumber",integer()); add("commentId",integer()); add("body",string()); required.extend(["pullNumber","commentId","body"]); "Reply to a PR review comment." }
            "pull_request_review_write" => { add("pullNumber",integer()); add("method",json!({"enum":["create","submit_pending","delete_pending"]})); add("commitID",string()); add("reviewId",integer()); add("event",json!({"enum":["APPROVE","REQUEST_CHANGES","COMMENT"]})); add("body",string()); required.extend(["pullNumber","method"]); "Create a review at commitID (required for create). Omit event to keep it pending; submit_pending requires reviewId and event. The PR head must still match commitID before creation." }
            "add_comment_to_pending_review" => { add("pullNumber",integer()); add("reviewId",integer()); add("body",string()); add("path",string()); add("line",integer()); add("side",json!({"enum":["LEFT","RIGHT"]})); required.extend(["pullNumber","reviewId","body","path","line"]); "Add an inline comment to a pending review." }
            "create_branch" => { add("branch",string()); add("from_branch",string()); required.extend(["branch","from_branch"]); "Create a task branch from an existing branch." }
            "create_or_update_file" | "delete_file" => { for key in ["path","branch","message","sha","content"] { add(key,string()); } required.extend(["path","branch","message"]); if *name == "delete_file" { required.push("sha"); } else { required.push("content"); } "Create/update a file using base64 content, or delete a file. Existing files require their blob sha; branch is explicit." }
            "push_files" => { for key in ["branch","message","expectedHead"] { add(key,string()); } add("files",json!({"type":"array","minItems":1,"maxItems":100,"items":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}})); required.extend(["branch","message","expectedHead","files"]); "Commit UTF-8 file changes atomically to an existing branch using Git trees. Requires the inspected expectedHead; never force-pushes. Uncertain writes must be inspected before retrying." }
            "create_pull_request" => { for key in ["title","body","head","base"] { add(key,string()); } add("draft",json!({"type":"boolean"})); required.extend(["title","head","base"]); "Create a pull request from a task branch." }
            "update_pull_request" => { add("pullNumber",integer()); for key in ["title","body","base"] { add(key,string()); } add("state",json!({"enum":["open","closed"]})); required.push("pullNumber"); "Update a PR title, body, base or open/closed state. Does not merge." }
            _ => unreachable!(),
        };
        json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":READ.contains(name),"destructiveHint":!READ.contains(name),"openWorldHint":*name != "git_context"}})
    }).collect()
}
