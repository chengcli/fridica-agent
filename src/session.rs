//! One live backend process: line-framed reading, recorded writes, control
//! signals, approvals and request/response matching.
use crate::{
    claude, codex, ApprovalDecision, ApprovalRequest, Approver, Backend, Child, Error, Failure,
    Options, Recorder, Spec, LINE_LIMIT,
};
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, watch},
    task::JoinHandle,
};

const STDERR_TAIL: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct WireError {
    pub kind: Failure,
    pub code: String,
    pub lost: bool,
}
impl WireError {
    fn of(kind: Failure, code: &str) -> Self {
        Self {
            kind,
            code: code.into(),
            lost: false,
        }
    }
    pub fn execution(code: &str) -> Self {
        Self::of(Failure::Execution, code)
    }
    pub fn refusal(code: &str) -> Self {
        Self::of(Failure::Refusal, code)
    }
    pub fn interrupted() -> Self {
        Self::of(Failure::Interrupted, "backend_interrupted")
    }
    pub fn closed() -> Self {
        Self::of(Failure::Cancelled, "backend_closed")
    }
    /// The backend no longer has the session being resumed.
    pub fn lost() -> Self {
        Self {
            lost: true,
            ..Self::execution("backend_session_unavailable")
        }
    }
}
impl From<Error> for WireError {
    fn from(f: Error) -> Self {
        Self::of(f.kind, &f.code)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Control {
    Interrupt,
    Close,
}
/// The caller's side of the current turn.
pub(crate) struct Call {
    pub approver: Arc<dyn Approver>,
    pub control: watch::Receiver<Control>,
    pub context: Value,
}
enum Line {
    Bytes(Vec<u8>),
    Error,
    Eof,
}
pub(crate) struct Session {
    pub spec: Spec,
    pub options: Options,
    pub session: String,
    pub resume: String,
    pub fork_from: String,
    pub turn: String,
    pub in_turn: bool,
    pub interrupted: bool,
    pub initialized: bool,
    pub tainted: bool,
    pub call: Call,
    pub process: Box<dyn Child>,
    pub stdin: Option<crate::Stdin>,
    lines: mpsc::Receiver<Line>,
    reader: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    stderr: Arc<Mutex<Vec<u8>>>,
    recorder: Arc<dyn Recorder>,
    sequence: u64,
    pending: VecDeque<Value>,
    approved: HashSet<String>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.reader.abort();
        self.stderr_task.abort();
    }
}
impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        spec: Spec,
        options: Options,
        recorder: Arc<dyn Recorder>,
        mut process: Box<dyn Child>,
        session: String,
        resume: String,
        fork_from: String,
        call: Call,
    ) -> Result<Self, Error> {
        let stdin = process.take_stdin();
        let (Some(stdout), Some(mut stderr)) = (process.take_stdout(), process.take_stderr())
        else {
            return Err(Error::new(
                Failure::Execution,
                "backend_start_failed",
                &session,
            ));
        };
        let (send, lines) = mpsc::channel(16);
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = vec![];
                loop {
                    let chunk = match reader.fill_buf().await {
                        Ok(b) => b,
                        Err(_) => {
                            let _ = send.send(Line::Error).await;
                            return;
                        }
                    };
                    if chunk.is_empty() {
                        if !line.is_empty() {
                            let _ = send.send(Line::Bytes(line)).await;
                        }
                        let _ = send.send(Line::Eof).await;
                        return;
                    }
                    let size = chunk
                        .iter()
                        .position(|b| *b == b'\n')
                        .map_or(chunk.len(), |p| p + 1);
                    let ended = chunk[size - 1] == b'\n';
                    if line.len() + size > LINE_LIMIT {
                        let _ = send.send(Line::Error).await;
                        return;
                    }
                    line.extend_from_slice(&chunk[..size]);
                    reader.consume(size);
                    if ended {
                        break;
                    }
                }
                if send.send(Line::Bytes(line)).await.is_err() {
                    return;
                }
            }
        });
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let tail = buffer.clone();
        let stderr_task = tokio::spawn(async move {
            let mut chunk = [0; 4096];
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                let mut tail = tail.lock().unwrap();
                tail.extend_from_slice(&chunk[..n]);
                let excess = tail.len().saturating_sub(STDERR_TAIL);
                tail.drain(..excess);
            }
        });
        Ok(Self {
            spec,
            options,
            session,
            resume,
            fork_from,
            turn: String::new(),
            in_turn: false,
            interrupted: false,
            initialized: false,
            tainted: false,
            call,
            process,
            stdin,
            lines,
            reader,
            stderr_task,
            stderr: buffer,
            recorder,
            sequence: 0,
            pending: VecDeque::new(),
            approved: HashSet::new(),
        })
    }
    pub fn next_id(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }
    /// The Claude control request ID for `id`.
    pub fn request_id(&self, id: u64) -> String {
        format!("{}-{id}", self.options.client.name)
    }
    pub async fn notice(&self, notice: Value) -> Result<(), WireError> {
        self.recorder
            .record(
                self.call.context.clone(),
                json!({"direction":"notice","notice":notice}),
            )
            .await?;
        Ok(())
    }
    pub async fn send(&mut self, message: Value) -> Result<(), WireError> {
        let mut bytes = serde_json::to_vec(&message)
            .map_err(|_| WireError::refusal("backend_message_invalid"))?;
        bytes.push(b'\n');
        if bytes.len() > LINE_LIMIT {
            return Err(WireError::refusal("backend_message_too_large"));
        }
        self.recorder
            .record(
                self.call.context.clone(),
                json!({"direction":"send_intent","message":message}),
            )
            .await?;
        self.stdin
            .as_mut()
            .ok_or_else(WireError::closed)?
            .write_all(&bytes)
            .await
            .map_err(|_| WireError::execution("backend_write_failed"))?;
        self.recorder
            .record(
                self.call.context.clone(),
                json!({"direction":"write_completed","message":message}),
            )
            .await?;
        Ok(())
    }
    async fn on_control(&mut self) -> Result<(), WireError> {
        let control = *self.call.control.borrow_and_update();
        match control {
            Control::Close => Err(WireError::closed()),
            Control::Interrupt => {
                self.interrupted = true;
                self.send_interrupt().await
            }
        }
    }
    pub async fn send_interrupt(&mut self) -> Result<(), WireError> {
        match self.spec.backend {
            Backend::Codex => {
                if self.turn.is_empty() {
                    return Ok(());
                }
                let id = self.next_id();
                self.send(json!({"id":id,"method":"turn/interrupt","params":{"threadId":self.session,"turnId":self.turn}})).await
            }
            Backend::Claude => {
                if !self.in_turn {
                    return Ok(());
                }
                let id = self.next_id();
                let request_id = self.request_id(id);
                self.send(json!({"type":"control_request","request_id":request_id,"request":{"subtype":"interrupt"}})).await
            }
        }
    }
    /// The next protocol message. With `wire_only`, messages queued while
    /// waiting for a response are skipped.
    pub async fn receive(&mut self, wire_only: bool) -> Result<Value, WireError> {
        loop {
            if !wire_only {
                if let Some(m) = self.pending.pop_front() {
                    return Ok(m);
                }
            }
            let output = tokio::select! {biased;
                changed=self.call.control.changed()=>{changed.map_err(|_|WireError::closed())?;self.on_control().await?;continue;},
                output=self.lines.recv()=>output,
            };
            match output {
                Some(Line::Bytes(line)) => {
                    self.recorder
                        .record(
                            self.call.context.clone(),
                            json!({"direction":"received","bytes":line}),
                        )
                        .await?;
                    if let Ok(message) = serde_json::from_slice::<Value>(&line) {
                        if message.is_object() {
                            return Ok(message);
                        }
                    }
                }
                Some(Line::Error) => {
                    self.recorder.record(self.call.context.clone(),json!({"direction":"read_failed","incomplete":true,"code":"backend_output_invalid_or_too_large"})).await?;
                    return Err(WireError::execution("backend_output_invalid_or_too_large"));
                }
                _ => {
                    let deadline = tokio::time::Instant::now() + self.options.eof_wait;
                    let status = self
                        .process
                        .eof_status(self.options.eof_wait)
                        .await
                        .map_err(|_| WireError::execution("backend_wait_failed"))?;
                    let _ = tokio::time::timeout_at(deadline, &mut self.stderr_task).await;
                    let stderr = self.stderr.lock().unwrap().clone();
                    self.recorder.record(self.call.context.clone(),json!({"direction":"eof","status":status,"stderr_tail":stderr,"incomplete":stderr.len()>=STDERR_TAIL})).await?;
                    if let Some(code) = self.process.refusal(status, &stderr) {
                        return Err(WireError::refusal(&code));
                    }
                    let detail = String::from_utf8_lossy(&stderr);
                    if detail.contains("No conversation found with session ID")
                        || detail.contains("no rollout found for thread id")
                    {
                        return Err(WireError::lost());
                    }
                    return Err(WireError::execution(
                        &status
                            .map(|s| format!("backend_exit_{s}"))
                            .unwrap_or_else(|| "backend_output_closed_before_exit".into()),
                    ));
                }
            }
        }
    }
    /// A Codex JSON-RPC request; server requests that arrive meanwhile are
    /// answered, and turn notifications are queued for the turn loop.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, WireError> {
        let id = self.next_id();
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let m = self.receive(true).await?;
            if m["id"] == id && m.get("method").is_none() {
                if m.get("error").is_some() {
                    if matches!(method, "thread/resume" | "thread/fork")
                        && m["error"]["message"]
                            .as_str()
                            .is_some_and(|s| s.contains("no rollout found for thread id"))
                    {
                        return Err(WireError::lost());
                    }
                    if matches!(m["error"]["code"].as_i64(), Some(-32602..=-32600)) {
                        return Err(WireError::refusal("codex_request_rejected"));
                    }
                    return Err(WireError::execution("codex_request_failed"));
                }
                return Ok(if m["result"].is_object() {
                    m["result"].clone()
                } else {
                    json!({})
                });
            }
            if m.get("id").is_some() {
                codex::dispatch(self, m).await?;
            } else if matches!(
                m["method"].as_str(),
                Some("item/completed" | "turn/completed" | "error")
            ) {
                if self.pending.len() >= 64 {
                    return Err(WireError::execution("backend_pending_output_limit"));
                }
                self.pending.push_back(m);
            }
        }
    }
    pub async fn approve(
        &mut self,
        request: ApprovalRequest,
    ) -> Result<ApprovalDecision, WireError> {
        if self.interrupted
            || self.spec.policy.approvals == "never"
            || (self.spec.backend == Backend::Claude && self.spec.policy.claude_prompts == "none")
        {
            return Ok(ApprovalDecision::Deny);
        }
        if !request.cache_key.is_empty() && self.approved.contains(&request.cache_key) {
            return Ok(ApprovalDecision::Session);
        }
        let approver = self.call.approver.clone();
        let pending = approver.request(request.clone());
        let decision = tokio::select! {biased;
            changed=self.call.control.changed()=>{changed.map_err(|_|WireError::closed())?;self.on_control().await?;ApprovalDecision::Deny},
            _=tokio::time::sleep(self.spec.approval_timeout)=>ApprovalDecision::Deny,
            decision=pending=>decision,
        };
        if decision == ApprovalDecision::Session && !request.cache_key.is_empty() {
            self.approved.insert(request.cache_key);
        }
        Ok(decision)
    }
    pub async fn handshake(&mut self) -> Result<(), WireError> {
        match self.spec.backend {
            Backend::Codex => codex::handshake(self).await,
            Backend::Claude => claude::handshake(self).await,
        }
    }
    /// One turn; returns the final assistant text.
    pub async fn job(&mut self, prompt: &str, schema: Option<&Value>) -> Result<String, WireError> {
        match self.spec.backend {
            Backend::Codex => codex::job(self, prompt, schema).await,
            Backend::Claude => claude::job(self, prompt).await,
        }
    }
}
