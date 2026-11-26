//! One owner task per agent handle. Caller cancellation and close requests
//! cannot strand a locked protocol reader or a turn waiting for approval.
use crate::{
    claude, codex,
    session::{Call, Control, Session},
    Approver, Backend, BoxFuture, Error, Failure, LaunchError, Launcher, Options, Recorder,
    SessionIds, Spec,
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};

/// A structured final answer: Codex gets `schema` as its output schema, Claude
/// gets `note` appended to the prompt. If `accept` rejects the answer, one
/// `repair` turn in the same session asks for it again (unless interrupted).
#[derive(Clone)]
pub struct OutputFormat {
    pub schema: Value,
    pub note: String,
    pub repair: String,
    pub accept: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}
/// One unit of work for an agent.
#[derive(Clone)]
pub struct Turn {
    pub prompt: String,
    /// Backend session to resume; empty starts a new one. If the backend no
    /// longer has it, the turn runs once in a fresh session.
    pub resume: String,
    /// Backend session to fork: the new session starts with a copy of that
    /// session's conversation and its own ID, leaving the source untouched.
    /// Empty means none; a non-empty `fork_from` with a non-empty `resume` is
    /// refused. A fork whose source is gone fails with `fork_source_missing`
    /// rather than starting fresh.
    pub fork_from: String,
    /// Passed to the [`Recorder`] with every event of this turn.
    pub context: Value,
    pub approver: Arc<dyn Approver>,
    pub output: Option<OutputFormat>,
}
#[derive(Clone, Debug)]
pub struct Reply {
    /// The accepted answer, or the first answer when none was accepted.
    pub text: String,
    /// Whether `OutputFormat::accept` accepted `text` (true without a format).
    pub accepted: bool,
    /// The backend session the turn ran in.
    pub backend_session_id: String,
}

fn failure(kind: Failure, code: &str, session: &str) -> Error {
    Error::new(kind, code, session)
}
struct Run {
    turn: Turn,
    control: watch::Receiver<Control>,
    reply: oneshot::Sender<Result<Reply, Error>>,
}
enum Command {
    Run(Box<Run>),
    Close(oneshot::Sender<Result<(), Error>>),
}
/// A handle to one backend session, started lazily by the first turn and kept
/// for later turns until it fails, idles out or is closed.
pub struct Agent {
    commands: mpsc::Sender<Command>,
    control: watch::Sender<Control>,
    alive: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
}
impl Agent {
    /// Construct the handle only; the backend starts with the first turn.
    pub fn new(
        spec: Spec,
        options: Options,
        launcher: Arc<dyn Launcher>,
        ids: Arc<dyn SessionIds>,
        recorder: Arc<dyn Recorder>,
    ) -> Result<Self, Error> {
        if [
            spec.job_timeout,
            spec.idle_timeout,
            spec.approval_timeout,
            options.eof_wait,
            options.close_grace,
        ]
        .iter()
        .any(Duration::is_zero)
            || (spec.forbid_mcp && !spec.mcp_servers.is_empty())
            || spec.mcp_servers.iter().any(|s| !s.valid())
            || spec
                .mcp_servers
                .iter()
                .map(|s| &s.name)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != spec.mcp_servers.len()
            || options.client.name.is_empty()
            || options
                .disabled_mcp_servers
                .iter()
                .any(|s| !crate::McpServer::valid_name(s))
        {
            return Err(failure(
                Failure::Refusal,
                "invalid_backend_configuration",
                "",
            ));
        }
        let (commands, receiver) = mpsc::channel(2);
        let (control, _) = watch::channel(Control::Interrupt);
        let alive = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(false));
        tokio::spawn(owner(
            Owner {
                spec,
                options,
                launcher,
                ids,
                recorder,
                alive: alive.clone(),
                busy: busy.clone(),
            },
            receiver,
        ));
        Ok(Self {
            commands,
            control,
            alive,
            busy,
        })
    }
    /// Whether a backend process is running.
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    /// Whether a turn is in progress.
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    /// Run one turn. Only one turn runs at a time; a second is refused with
    /// `backend_busy`. Dropping the future cancels the turn.
    pub fn run(&self, turn: Turn) -> BoxFuture<'_, Result<Reply, Error>> {
        Box::pin(async move {
            if self
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Err(failure(Failure::Refusal, "backend_busy", ""));
            }
            let (reply, receive) = oneshot::channel();
            let command = Command::Run(Box::new(Run {
                turn,
                control: self.control.subscribe(),
                reply,
            }));
            if self.commands.try_send(command).is_err() {
                self.busy.store(false, Ordering::SeqCst);
                return Err(failure(Failure::Execution, "backend_owner_unavailable", ""));
            }
            receive
                .await
                .unwrap_or_else(|_| Err(failure(Failure::Execution, "backend_owner_stopped", "")))
        })
    }
    /// Interrupt the current turn, if any; the session stays usable.
    pub fn interrupt(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            if self.busy() {
                self.control.send_replace(Control::Interrupt);
            }
            Ok(())
        })
    }
    /// Cancel any turn, then stop and reap the backend process.
    pub fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            self.control.send_replace(Control::Close);
            let (reply, receive) = oneshot::channel();
            self.commands
                .send(Command::Close(reply))
                .await
                .map_err(|_| failure(Failure::Execution, "backend_owner_unavailable", ""))?;
            receive
                .await
                .unwrap_or_else(|_| Err(failure(Failure::Execution, "backend_owner_stopped", "")))
        })
    }
}
struct Owner {
    spec: Spec,
    options: Options,
    launcher: Arc<dyn Launcher>,
    ids: Arc<dyn SessionIds>,
    recorder: Arc<dyn Recorder>,
    alive: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
}
async fn cleanup(session: &mut Option<Session>, owner: &Owner) -> Result<(), Error> {
    if let Some(s) = session {
        s.stdin.take();
        s.tainted = true;
        s.process
            .terminate(owner.options.close_grace)
            .await
            .map_err(|_| failure(Failure::Execution, "backend_close_unconfirmed", &s.session))?;
    }
    session.take();
    owner.alive.store(false, Ordering::SeqCst);
    Ok(())
}
async fn owner(owner: Owner, mut commands: mpsc::Receiver<Command>) {
    let mut session = None;
    loop {
        let command = tokio::select! {biased;
            command=commands.recv()=>command,
            _=tokio::time::sleep(owner.spec.idle_timeout),if session.is_some()=>{let _=cleanup(&mut session,&owner).await;continue;},
        };
        match command {
            Some(Command::Run(run)) => {
                let Run {
                    turn,
                    control,
                    mut reply,
                } = *run;
                let mut closing = control.clone();
                let result = {
                    let operation = run_turn(&owner, &mut session, turn, control);
                    tokio::select! {biased;
                        _=reply.closed()=>Err(failure(Failure::Cancelled,"backend_caller_cancelled","")),
                        _=async { loop {
                            if closing.changed().await.is_err() || matches!(*closing.borrow_and_update(),Control::Close) { break; }
                        }}=>Err(failure(Failure::Cancelled,"backend_closed","")),
                        result=tokio::time::timeout(owner.spec.job_timeout,operation)=>result.unwrap_or_else(|_|Err(failure(Failure::Execution,"backend_job_timeout",""))),
                    }
                };
                let result = result.map_err(|mut error| {
                    if error.backend_session_id.is_empty() {
                        if let Some(s) = &session {
                            error.backend_session_id = s.session.clone();
                        }
                    }
                    error
                });
                let result = if result.is_err() {
                    match cleanup(&mut session, &owner).await {
                        Ok(()) => result,
                        Err(error) => Err(error),
                    }
                } else {
                    result
                };
                owner.busy.store(false, Ordering::SeqCst);
                let _ = reply.send(result);
            }
            Some(Command::Close(reply)) => {
                let _ = reply.send(cleanup(&mut session, &owner).await);
            }
            None => {
                let _ = cleanup(&mut session, &owner).await;
                break;
            }
        }
    }
}
async fn run_turn(
    owner: &Owner,
    slot: &mut Option<Session>,
    turn: Turn,
    mut control: watch::Receiver<Control>,
) -> Result<Reply, Error> {
    if control.has_changed().unwrap_or(true) {
        return Err(failure(
            Failure::Interrupted,
            "backend_interrupted_before_start",
            "",
        ));
    }
    control.borrow_and_update();
    if !turn.fork_from.is_empty() && !turn.resume.is_empty() {
        return Err(failure(Failure::Refusal, "fork_with_resume", &turn.resume));
    }
    // A fork always needs its own process: the live one continues the source.
    let replace = if let Some(s) = slot {
        !turn.fork_from.is_empty()
            || s.tainted
            || s.process
                .try_exit()
                .map_err(|_| failure(Failure::Execution, "backend_wait_failed", &s.session))?
                .is_some()
            || (!turn.resume.is_empty() && s.session != turn.resume)
    } else {
        false
    };
    if replace {
        cleanup(slot, owner).await?;
    }
    let prompt = match (&turn.output, owner.spec.backend) {
        (Some(output), Backend::Claude) => format!("{}\n\n{}", turn.prompt, output.note),
        _ => turn.prompt.clone(),
    };
    let schema = turn.output.as_ref().map(|o| &o.schema);
    let call = |control: &watch::Receiver<Control>| Call {
        approver: turn.approver.clone(),
        control: control.clone(),
        context: turn.context.clone(),
    };
    let mut resume = turn.resume.clone();
    let fork_from = turn.fork_from.clone();
    for fresh_attempt in 0..2 {
        if slot.is_none() {
            // A forked Claude session reports its new ID itself; until then
            // the source names it in diagnostics.
            let id = if !fork_from.is_empty() {
                fork_from.clone()
            } else if owner.spec.backend == Backend::Claude && resume.is_empty() {
                owner.ids.new_session_id()
            } else {
                resume.clone()
            };
            let spec = &owner.spec;
            let command = match spec.backend {
                Backend::Codex => codex::command(spec, &owner.options.disabled_mcp_servers),
                Backend::Claude => claude::command(spec, &resume, &id, &fork_from),
            };
            owner
                .recorder
                .record(
                    turn.context.clone(),
                    json!({"direction":"start","command":command,"resume":resume,"fork_from":fork_from}),
                )
                .await?;
            let process = owner.launcher.launch(command).map_err(|e| match e {
                LaunchError::Refused(error) => error,
                LaunchError::StartFailed => {
                    failure(Failure::Execution, "backend_start_failed", &id)
                }
            })?;
            *slot = Some(Session::start(
                spec.clone(),
                owner.options.clone(),
                owner.recorder.clone(),
                process,
                id,
                resume.clone(),
                fork_from.clone(),
                call(&control),
            )?);
            owner.alive.store(true, Ordering::SeqCst);
        }
        let s = slot.as_mut().unwrap();
        s.call = call(&control);
        s.interrupted = false;
        s.turn.clear();
        let operation: Result<(String, bool), crate::session::WireError> = async {
            if !s.initialized {
                s.handshake().await?;
                s.initialized = true;
            }
            let text = s.job(&prompt, schema).await?;
            let Some(output) = &turn.output else {
                return Ok((text, true));
            };
            if (output.accept)(&text) {
                return Ok((text, true));
            }
            if !s.interrupted {
                if let Ok(repaired) = s.job(&output.repair, schema).await {
                    if (output.accept)(&repaired) {
                        return Ok((repaired, true));
                    }
                }
            }
            Ok((text, false))
        }
        .await;
        let id = s.session.clone();
        match operation {
            Ok((text, accepted)) => {
                return Ok(Reply {
                    text,
                    accepted,
                    backend_session_id: id,
                })
            }
            Err(e) if e.lost && !fork_from.is_empty() => {
                cleanup(slot, owner).await?;
                return Err(failure(Failure::Execution, "fork_source_missing", &id));
            }
            Err(e) if e.lost && fresh_attempt == 0 && !resume.is_empty() => {
                control = s.call.control.clone();
                cleanup(slot, owner).await?;
                resume.clear();
            }
            Err(e) => return Err(failure(e.kind, &e.code, &id)),
        }
    }
    unreachable!()
}
