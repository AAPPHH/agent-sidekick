use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ContentBlock, Implementation, ProgressNotificationParam,
        ServerCapabilities, ServerConfig,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::harness::HarnessKind;
use crate::runner::{Launch, Pause};
use crate::server::SharedPhase;
use crate::trust::{Event, Outcome};

pub const ASK_SERVER: &str = "sidekick_ask";
pub const ASK_TOOL: &str = "ask_orchestrator";
pub const CLAUDE_TOOL_NAME: &str = "mcp__sidekick_ask__ask_orchestrator";
pub const OPENCODE_TOOL_NAME: &str = "sidekick_ask_ask_orchestrator";
pub const ASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const ENV_ASK_TIMEOUT: &str = "AGENT_SIDEKICK_ASK_TIMEOUT_S";
pub const ENV_RESULT_GRACE: &str = "AGENT_SIDEKICK_RESULT_GRACE_S";
pub const NO_ANSWER: &str = "Keine Antwort vom Orchestrator: nichts entscheiden, aktuellen Stand abliefern und offene Punkte nennen.";
pub const ASK_PROMPT_LINE: &str = "You have the MCP tool ask_orchestrator(question): use it instead of guessing when a decision is open.";
pub const MAX_PAUSED: Duration = Duration::from_secs(60 * 60);
pub const ENV_ASK_JOB: &str = "AGENT_SIDEKICK_ASK_JOB";
pub const ENV_ASK_SOCKET: &str = "AGENT_SIDEKICK_ASK_SOCKET";
pub const ENV_ASK_EXE: &str = "AGENT_SIDEKICK_ASK_EXE";
const TOOL_TIMEOUT_MARGIN: Duration = Duration::from_secs(5 * 60);
const BRIDGE_PROGRESS: Duration = Duration::from_secs(20);

fn clamp_timeout(raw: Option<&str>) -> Duration {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map_or(ASK_TIMEOUT, Duration::from_secs)
        .min(MAX_PAUSED)
}

pub fn ask_timeout() -> Duration {
    clamp_timeout(std::env::var(ENV_ASK_TIMEOUT).ok().as_deref())
}

pub fn result_grace() -> Duration {
    std::env::var(ENV_RESULT_GRACE)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(ASK_TIMEOUT, Duration::from_secs)
        .min(MAX_PAUSED)
}

#[derive(Clone, Debug)]
pub struct AskWire {
    pub exe: PathBuf,
    pub job_id: String,
    pub endpoint: String,
}

impl AskWire {
    pub fn bridge_args(&self) -> Vec<String> {
        [
            "ask-bridge",
            "--job",
            &self.job_id,
            "--socket",
            &self.endpoint,
        ]
        .map(String::from)
        .to_vec()
    }

    fn tool_timeout_ms(&self) -> u64 {
        (ask_timeout() + TOOL_TIMEOUT_MARGIN).as_millis() as u64
    }
}

fn insert_before_separator(args: &mut Vec<OsString>, extra: impl IntoIterator<Item = OsString>) {
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    args.splice(at..at, extra);
}

fn claude_wire(launch: &mut Launch, wire: &AskWire) {
    let config = json!({"mcpServers": {ASK_SERVER: {"type": "stdio", "command": wire.exe, "args": wire.bridge_args(), "timeout": wire.tool_timeout_ms()}}});
    match launch.args.iter().position(|a| a == "--allowedTools") {
        Some(i) => launch.args.insert(i + 1, CLAUDE_TOOL_NAME.into()),
        None => insert_before_separator(
            &mut launch.args,
            ["--allowedTools".into(), CLAUDE_TOOL_NAME.into()],
        ),
    }
    insert_before_separator(
        &mut launch.args,
        ["--mcp-config".into(), config.to_string().into()],
    );
    launch.header.push(format!(
        "ask_orchestrator: mcp server {ASK_SERVER} on job {}",
        wire.job_id
    ));
}

fn opencode_wire(launch: &mut Launch, wire: &AskWire) -> Result<()> {
    let mut command = vec![wire.exe.to_string_lossy().into_owned()];
    command.extend(wire.bridge_args());
    let patch = |launch: &mut Launch, name: &str, f: &dyn Fn(&mut Value)| -> Result<()> {
        let slot = launch
            .env
            .iter_mut()
            .find(|(k, _)| k == name)
            .with_context(|| format!("{name} missing in the opencode environment"))?;
        let mut value: Value = serde_json::from_str(&slot.1.to_string_lossy())?;
        f(&mut value);
        slot.1 = value.to_string().into();
        Ok(())
    };
    patch(launch, "OPENCODE_CONFIG_CONTENT", &|v| {
        v["mcp"] = json!({ASK_SERVER: {"type": "local", "command": command, "enabled": true, "timeout": wire.tool_timeout_ms()}});
    })?;
    patch(launch, "OPENCODE_PERMISSION", &|v| {
        v[OPENCODE_TOOL_NAME] = json!("allow")
    })?;
    launch.header.push(format!(
        "ask_orchestrator: mcp server {ASK_SERVER} on job {}",
        wire.job_id
    ));
    Ok(())
}

fn toml_value(v: impl serde::Serialize) -> String {
    toml::Value::try_from(v)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

fn codex_wire(launch: &mut Launch, wire: &AskWire) {
    let key = |k: &str| format!("mcp_servers.{ASK_SERVER}.{k}");
    let flags = [
        (key("command"), toml_value(wire.exe.to_string_lossy())),
        (key("args"), toml_value(wire.bridge_args())),
        (
            key("tool_timeout_sec"),
            (wire.tool_timeout_ms() / 1000).to_string(),
        ),
    ];
    let at = launch.args.len().saturating_sub(1);
    launch.args.splice(
        at..at,
        flags
            .into_iter()
            .flat_map(|(k, v)| ["-c".into(), format!("{k}={v}").into()]),
    );
    launch.header.push(format!(
        "ask_orchestrator: mcp server {ASK_SERVER} on job {}",
        wire.job_id
    ));
}

fn command_wire(launch: &mut Launch, wire: &AskWire) {
    for (k, v) in [
        (ENV_ASK_JOB, wire.job_id.clone()),
        (ENV_ASK_SOCKET, wire.endpoint.clone()),
        (ENV_ASK_EXE, wire.exe.to_string_lossy().into_owned()),
    ] {
        launch.env.retain(|(n, _)| n != k);
        launch.env.push((k.into(), v.into()));
    }
    launch.header.push(format!(
        "ask_orchestrator: bridge in {ENV_ASK_JOB}, {ENV_ASK_SOCKET}, {ENV_ASK_EXE}"
    ));
}

pub fn wire_launch(kind: HarnessKind, launch: &mut Launch, wire: &AskWire) -> Result<()> {
    match kind {
        HarnessKind::Claude => claude_wire(launch, wire),
        HarnessKind::OpenCode => opencode_wire(launch, wire)?,
        HarnessKind::Codex => codex_wire(launch, wire),
        HarnessKind::Command => command_wire(launch, wire),
    }
    Ok(())
}

#[cfg(unix)]
mod transport {
    use super::*;
    pub type Stream = tokio::net::UnixStream;
    pub struct Listener(tokio::net::UnixListener, PathBuf);

    const SOCKET_PATH_MAX: usize = 100;

    pub fn bind(dir: &Path) -> Result<(Listener, String, String)> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let mut job_dir = dir.join(&id[..16]);
        if job_dir.join("s").as_os_str().len() > SOCKET_PATH_MAX {
            job_dir = std::env::temp_dir().join(format!("sk-{}", &id[..16]));
        }
        let path = job_dir.join("s");
        if path.as_os_str().len() > SOCKET_PATH_MAX {
            bail!("ask socket path too long: {}", path.display());
        }
        std::fs::DirBuilder::new().mode(0o700).create(&job_dir)?;
        let listener = match tokio::net::UnixListener::bind(&path) {
            Ok(l) => Listener(l, job_dir.clone()),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&job_dir);
                return Err(e.into());
            }
        };
        let token = uuid::Uuid::new_v4().simple().to_string();
        write_token(&job_dir, &token)?;
        Ok((listener, path.to_string_lossy().into_owned(), token))
    }

    fn token_dir(endpoint: &str) -> PathBuf {
        Path::new(endpoint)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    impl Listener {
        pub async fn accept(&self) -> std::io::Result<Stream> {
            self.0.accept().await.map(|(s, _)| s)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    pub async fn connect(endpoint: &str) -> std::io::Result<Stream> {
        tokio::net::UnixStream::connect(endpoint).await
    }

    pub fn take_token(endpoint: &str) -> Result<String> {
        take_token_file(&token_dir(endpoint))
    }
}

#[cfg(not(unix))]
mod transport {
    use super::*;
    pub type Stream = tokio::net::TcpStream;
    pub struct Listener(tokio::net::TcpListener, PathBuf);

    pub fn bind(dir: &Path) -> Result<(Listener, String, String)> {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        std_listener.set_nonblocking(true)?;
        let port = std_listener.local_addr()?.port();
        let job_dir = dir.join(&uuid::Uuid::new_v4().simple().to_string()[..16]);
        std::fs::create_dir_all(&job_dir)?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        write_token(&job_dir, &token)?;
        Ok((
            Listener(
                tokio::net::TcpListener::from_std(std_listener)?,
                job_dir.clone(),
            ),
            format!("127.0.0.1:{port}|{}", job_dir.display()),
            token,
        ))
    }

    impl Listener {
        pub async fn accept(&self) -> std::io::Result<Stream> {
            self.0.accept().await.map(|(s, _)| s)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    pub async fn connect(endpoint: &str) -> std::io::Result<Stream> {
        tokio::net::TcpStream::connect(endpoint.split_once('|').map_or(endpoint, |e| e.0)).await
    }

    pub fn take_token(endpoint: &str) -> Result<String> {
        take_token_file(Path::new(endpoint.split_once('|').map_or("", |e| e.1)))
    }
}

const TOKEN_FILE: &str = "t";

fn write_token(dir: &Path, token: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(dir.join(TOKEN_FILE))?
        .write_all(token.as_bytes())?;
    Ok(())
}

fn take_token_file(dir: &Path) -> Result<String> {
    let path = dir.join(TOKEN_FILE);
    let token = std::fs::read_to_string(&path)
        .context("the ask token was already consumed or is missing")?;
    std::fs::remove_file(&path)?;
    Ok(token.trim().to_string())
}

#[derive(Debug)]
pub enum JobEvent {
    Question {
        job_id: String,
        step: Option<String>,
        question_id: u64,
        text: String,
    },
    Done {
        text: String,
        is_error: bool,
    },
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub cancel: CancellationToken,
    pub timeout: Duration,
    pub(crate) phase: SharedPhase,
    tx: mpsc::UnboundedSender<JobEvent>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<JobEvent>>,
}

impl Session {
    pub fn send(&self, event: JobEvent) {
        let _ = self.tx.send(event);
    }

    pub async fn next(&self) -> Option<JobEvent> {
        self.rx.lock().await.recv().await
    }
}

#[derive(Debug, Default)]
struct BrokerState {
    sessions: HashMap<String, Arc<Session>>,
    job_session: HashMap<String, String>,
    pending: HashMap<String, (u64, String, oneshot::Sender<String>)>,
    next: u64,
}

#[derive(Debug, Default)]
pub struct Broker {
    state: Mutex<BrokerState>,
}

pub enum Answered {
    Delivered(Arc<Session>),
    NoOpenQuestion(Arc<Session>),
    OtherQuestionOpen { open_id: u64, text: String },
    Unknown,
}

impl Broker {
    fn st(&self) -> std::sync::MutexGuard<'_, BrokerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn taken(&self, id: &str) -> bool {
        let st = self.st();
        st.sessions.contains_key(id) || st.job_session.contains_key(id)
    }

    pub(crate) fn open(
        &self,
        id: &str,
        job_ids: &[String],
        shutdown: &CancellationToken,
        phase: SharedPhase,
        timeout: Duration,
    ) -> Result<Arc<Session>, String> {
        let mut st = self.st();
        if let Some(busy) = std::iter::once(id)
            .chain(job_ids.iter().map(String::as_str))
            .find(|i| st.sessions.contains_key(*i) || st.job_session.contains_key(*i))
        {
            return Err(format!(
                "`{busy}` is already running; job_id and workflow_id must be unique"
            ));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let session = Arc::new(Session {
            id: id.to_string(),
            timeout,
            cancel: shutdown.child_token(),
            phase,
            tx,
            rx: tokio::sync::Mutex::new(rx),
        });
        st.sessions.insert(id.to_string(), session.clone());
        for j in job_ids {
            st.job_session.insert(j.clone(), id.to_string());
        }
        Ok(session)
    }

    pub fn close(&self, session: &Arc<Session>) {
        let session_id = session.id.as_str();
        let mut st = self.st();
        if !st
            .sessions
            .get(session_id)
            .is_some_and(|s| Arc::ptr_eq(s, session))
        {
            return;
        }
        st.sessions.remove(session_id);
        let jobs: Vec<String> = st
            .job_session
            .iter()
            .filter(|(_, s)| *s == session_id)
            .map(|(j, _)| j.clone())
            .collect();
        for j in jobs {
            st.job_session.remove(&j);
            st.pending.remove(&j);
        }
    }

    pub fn session_of(&self, job_id: &str) -> Option<Arc<Session>> {
        let st = self.st();
        st.job_session
            .get(job_id)
            .and_then(|s| st.sessions.get(s))
            .cloned()
    }

    pub fn answer(&self, job_id: &str, question_id: u64, answer: String) -> Answered {
        let mut st = self.st();
        let Some(session) = st
            .job_session
            .get(job_id)
            .and_then(|s| st.sessions.get(s))
            .cloned()
        else {
            return Answered::Unknown;
        };
        let Some((open_id, text, _)) = st.pending.get(job_id) else {
            return Answered::NoOpenQuestion(session);
        };
        if question_id != *open_id {
            return Answered::OtherQuestionOpen {
                open_id: *open_id,
                text: text.clone(),
            };
        }
        let (_, _, tx) = st.pending.remove(job_id).expect("checked");
        if tx.send(answer).is_ok() {
            Answered::Delivered(session)
        } else {
            Answered::NoOpenQuestion(session)
        }
    }

    pub fn is_open(&self, job_id: &str, question_id: u64) -> bool {
        self.st()
            .pending
            .get(job_id)
            .is_some_and(|(id, ..)| *id == question_id)
    }

    fn add_pending(&self, job_id: &str, question: &str) -> (u64, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        let mut st = self.st();
        st.next += 1;
        let id = st.next;
        st.pending
            .insert(job_id.to_string(), (id, question.to_string(), tx));
        (id, rx)
    }

    fn drop_pending(&self, job_id: &str, id: u64) {
        let mut st = self.st();
        if st.pending.get(job_id).is_some_and(|(i, ..)| *i == id) {
            st.pending.remove(job_id);
        }
    }
}

pub struct AskJob {
    pub job_id: String,
    pub step: Option<String>,
    pub session: Arc<Session>,
    pub broker: Arc<Broker>,
    pub pause: Arc<Pause>,
    pub timeout: Duration,
    pub budget: Duration,
    pub questions: Mutex<Vec<String>>,
    pub unanswered: Arc<AtomicBool>,
    used: Mutex<Duration>,
    turn: tokio::sync::Mutex<()>,
}

struct OpenQuestion<'a> {
    job: &'a AskJob,
    id: u64,
    since: Instant,
    answered: bool,
}

impl Drop for OpenQuestion<'_> {
    fn drop(&mut self) {
        self.job.broker.drop_pending(&self.job.job_id, self.id);
        *self
            .job
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += self.since.elapsed();
        if !self.answered {
            self.job.unanswered.store(true, Ordering::SeqCst);
        }
    }
}

impl AskJob {
    pub fn new(
        job_id: &str,
        step: Option<String>,
        session: Arc<Session>,
        broker: Arc<Broker>,
        pause: Arc<Pause>,
    ) -> Arc<Self> {
        Self::with_limits(
            job_id,
            step,
            session,
            broker,
            pause,
            ask_timeout(),
            MAX_PAUSED,
        )
    }

    pub fn with_limits(
        job_id: &str,
        step: Option<String>,
        session: Arc<Session>,
        broker: Arc<Broker>,
        pause: Arc<Pause>,
        timeout: Duration,
        budget: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            job_id: job_id.into(),
            step,
            session,
            broker,
            pause,
            timeout,
            budget,
            questions: Mutex::default(),
            unanswered: Arc::default(),
            used: Mutex::default(),
            turn: tokio::sync::Mutex::new(()),
        })
    }

    pub fn asked(&self) -> Vec<String> {
        self.questions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn give_up(&self) -> String {
        self.unanswered.store(true, Ordering::SeqCst);
        NO_ANSWER.to_string()
    }

    pub async fn ask(&self, question: String) -> String {
        let _turn = tokio::select! {
            t = self.turn.lock() => t,
            _ = self.session.cancel.cancelled() => return self.give_up(),
        };
        let left = self.budget.saturating_sub(
            *self
                .used
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let limit = self.timeout.min(left);
        if limit.is_zero() || self.session.cancel.is_cancelled() {
            return self.give_up();
        }
        self.questions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(question.clone());
        let (id, mut rx) = self.broker.add_pending(&self.job_id, &question);
        let mut open = OpenQuestion {
            job: self,
            id,
            since: Instant::now(),
            answered: false,
        };
        let _hold = self.pause.hold();
        self.session.send(JobEvent::Question {
            job_id: self.job_id.clone(),
            step: self.step.clone(),
            question_id: id,
            text: question,
        });
        let mut answer = tokio::select! {
            a = tokio::time::timeout(limit, &mut rx) => a.ok().and_then(Result::ok),
            _ = self.session.cancel.cancelled() => None,
        };
        if answer.is_none() {
            rx.close();
            answer = rx.try_recv().ok();
        }
        open.answered = answer.is_some();
        answer.unwrap_or_else(|| NO_ANSWER.to_string())
    }
}

pub struct AskServer {
    pub endpoint: String,
    handle: JoinHandle<()>,
}

impl Drop for AskServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    token: String,
    question: String,
}

async fn handle(stream: transport::Stream, job: Arc<AskJob>, token: String) {
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }
    let Ok(req) = serde_json::from_str::<Request>(&line) else {
        return;
    };
    if req.token != token {
        return;
    }
    let mut eof = String::new();
    let answer = tokio::select! {
        a = job.ask(req.question) => a,
        _ = reader.read_line(&mut eof) => return,
    };
    let _ = write
        .write_all(format!("{}\n", json!({"answer": answer})).as_bytes())
        .await;
    let _ = write.shutdown().await;
}

impl AskServer {
    pub fn start(dir: &Path, job: Arc<AskJob>) -> Result<Self> {
        let (listener, endpoint, token) = transport::bind(dir)?;
        Ok(Self {
            endpoint,
            handle: tokio::spawn(accept_loop(listener, job, token)),
        })
    }
}

async fn accept_loop(listener: transport::Listener, job: Arc<AskJob>, token: String) {
    while let Ok(stream) = listener.accept().await {
        tokio::spawn(handle(stream, job.clone(), token.clone()));
    }
}

pub fn count_questions(events: &[Event], model_id: &str) -> u32 {
    let own = crate::trust::dedup_sorted(events.iter().filter(|e| e.model_id == model_id).cloned());
    let from = own
        .iter()
        .rposition(|e| e.outcome == Outcome::Reset)
        .map_or(0, |i| i + 1);
    own[from..]
        .iter()
        .filter_map(|e| e.questions)
        .map(|q| q.max(0) as u32)
        .sum()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AskParams {
    pub question: String,
}

#[derive(Clone)]
struct Bridge {
    endpoint: String,
    token: String,
}

impl Bridge {
    async fn forward(&self, question: &str) -> Result<String> {
        let stream = transport::connect(&self.endpoint)
            .await
            .context("the orchestrator is not reachable")?;
        let (read, mut write) = tokio::io::split(stream);
        write
            .write_all(
                format!("{}\n", json!({"token": self.token, "question": question})).as_bytes(),
            )
            .await?;
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await?;
        if line.is_empty() {
            bail!("the orchestrator connection closed without an answer");
        }
        Ok(serde_json::from_str::<Value>(&line)?["answer"]
            .as_str()
            .unwrap_or(NO_ANSWER)
            .to_string())
    }
}

#[tool_router]
impl Bridge {
    /// Ask the orchestrator (the agent that gave you this task) a question and wait for its answer. Use it instead of guessing whenever a decision is open or the task is ambiguous. The call can block for minutes.
    #[tool]
    async fn ask_orchestrator(
        &self,
        Parameters(p): Parameters<AskParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let ticker = ctx.meta.get_progress_token().map(|token| {
            let peer = ctx.peer.clone();
            tokio::spawn(async move {
                let mut n = 0.0;
                loop {
                    tokio::time::sleep(BRIDGE_PROGRESS).await;
                    n += 1.0;
                    if peer
                        .notify_progress(
                            ProgressNotificationParam::new(token.clone(), n)
                                .with_message("waiting for the orchestrator"),
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });
        let result = self.forward(&p.question).await;
        if let Some(t) = ticker {
            t.abort();
        }
        Ok(match result {
            Ok(answer) => CallToolResult::success(vec![ContentBlock::text(answer)]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
        })
    }
}

#[tool_handler]
impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(ASK_SERVER, env!("CARGO_PKG_VERSION")))
    }
}

pub async fn run_bridge(_job: &str, endpoint: &str) -> Result<()> {
    let token = transport::take_token(endpoint)?;
    let service = Bridge {
        endpoint: endpoint.to_string(),
        token,
    }
    .serve(rmcp::transport::stdio())
    .await
    .map_err(|e| anyhow::anyhow!("MCP initialization failed: {e}"))?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::RunPhase;

    fn wire() -> AskWire {
        AskWire {
            exe: PathBuf::from("/bin/agent-sidekick"),
            job_id: "j-1".into(),
            endpoint: "/tmp/x.sock".into(),
        }
    }

    fn launch(args: &[&str], env: &[(&str, &str)]) -> Launch {
        Launch {
            program: "p".into(),
            args: args.iter().map(OsString::from).collect(),
            env: env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
            stdin: None,
            secrets: crate::redact::Secrets::none(),
            header: Vec::new(),
        }
    }

    fn strs(l: &Launch) -> Vec<String> {
        l.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn claude_gets_mcp_config_and_the_tool_stays_allowed_with_the_prompt_last() {
        let mut l = launch(
            &[
                "-p",
                "--allowedTools",
                "Read",
                "Grep",
                "--disallowedTools",
                "Edit",
                "--",
                "do it",
            ],
            &[],
        );
        wire_launch(HarnessKind::Claude, &mut l, &wire()).unwrap();
        let a = strs(&l);
        assert_eq!(&a[a.len() - 2..], ["--", "do it"]);
        let at = a.iter().position(|x| x == "--allowedTools").unwrap();
        assert_eq!(a[at + 1], CLAUDE_TOOL_NAME);
        let cfg: Value =
            serde_json::from_str(&a[a.iter().position(|x| x == "--mcp-config").unwrap() + 1])
                .unwrap();
        let server = &cfg["mcpServers"][ASK_SERVER];
        assert_eq!(server["command"], "/bin/agent-sidekick");
        assert_eq!(
            server["args"],
            json!(["ask-bridge", "--job", "j-1", "--socket", "/tmp/x.sock"])
        );
        assert!(server["timeout"].as_u64().unwrap() > ASK_TIMEOUT.as_millis() as u64);
        let mut bare = launch(&["-p"], &[]);
        wire_launch(HarnessKind::Claude, &mut bare, &wire()).unwrap();
        assert!(strs(&bare)
            .windows(2)
            .any(|w| w[0] == "--allowedTools" && w[1] == CLAUDE_TOOL_NAME));
    }

    #[test]
    fn opencode_gets_an_mcp_entry_a_long_timeout_and_the_permission() {
        let mut l = launch(
            &["run"],
            &[
                ("OPENCODE_CONFIG_CONTENT", r#"{"agent":{}}"#),
                ("OPENCODE_PERMISSION", r#"{"edit":"deny"}"#),
            ],
        );
        wire_launch(HarnessKind::OpenCode, &mut l, &wire()).unwrap();
        let get = |n: &str| {
            serde_json::from_str::<Value>(
                &l.env
                    .iter()
                    .find(|(k, _)| k == n)
                    .unwrap()
                    .1
                    .to_string_lossy(),
            )
            .unwrap()
        };
        let entry = &get("OPENCODE_CONFIG_CONTENT")["mcp"][ASK_SERVER];
        assert_eq!(entry["type"], "local");
        assert_eq!(entry["command"][1], "ask-bridge");
        assert!(entry["timeout"].as_u64().unwrap() > ASK_TIMEOUT.as_millis() as u64);
        let perm = get("OPENCODE_PERMISSION");
        assert_eq!(
            (perm[OPENCODE_TOOL_NAME].as_str(), perm["edit"].as_str()),
            (Some("allow"), Some("deny"))
        );
        assert!(wire_launch(HarnessKind::OpenCode, &mut launch(&["run"], &[]), &wire()).is_err());
    }

    #[test]
    fn codex_gets_mcp_servers_flags_before_the_stdin_dash_and_commands_get_env() {
        let mut l = launch(&["exec", "--json", "-"], &[]);
        wire_launch(HarnessKind::Codex, &mut l, &wire()).unwrap();
        let a = strs(&l);
        assert_eq!(a.last().unwrap(), "-");
        assert!(
            a.contains(&format!(
                "mcp_servers.{ASK_SERVER}.command=\"/bin/agent-sidekick\""
            )),
            "{a:?}"
        );
        assert!(a.contains(&format!("mcp_servers.{ASK_SERVER}.args=[\"ask-bridge\", \"--job\", \"j-1\", \"--socket\", \"/tmp/x.sock\"]")), "{a:?}");
        assert!(a
            .iter()
            .any(|x| x.starts_with(&format!("mcp_servers.{ASK_SERVER}.tool_timeout_sec="))));
        let mut c = launch(&["x"], &[(ENV_ASK_JOB, "stale")]);
        wire_launch(HarnessKind::Command, &mut c, &wire()).unwrap();
        assert_eq!(c.env.iter().filter(|(k, _)| k == ENV_ASK_JOB).count(), 1);
        assert!(c
            .env
            .iter()
            .any(|(k, v)| k == ENV_ASK_SOCKET && v == "/tmp/x.sock"));
    }

    #[test]
    fn question_counts_ride_on_the_job_event_and_stop_at_a_reset() {
        let ev = |outcome, ts, q| {
            let mut e = Event::new(
                "m1",
                crate::trust::Dimension::Search,
                crate::trust::Scope::Global,
                outcome,
                ts,
            );
            e.questions = q;
            e
        };
        let events = vec![
            ev(Outcome::Note, 1, Some(2)),
            ev(Outcome::Note, 2, Some(1)),
            ev(Outcome::Note, 3, None),
        ];
        assert_eq!(count_questions(&events, "m1"), 3);
        assert_eq!(count_questions(&events, "other"), 0);
        let mut after = events.clone();
        after.extend([ev(Outcome::Reset, 10, None), ev(Outcome::Note, 11, Some(4))]);
        assert_eq!(count_questions(&after, "m1"), 4);
    }

    fn job(timeout: Duration) -> (Arc<Broker>, Arc<Session>, Arc<AskJob>) {
        job_with(timeout, MAX_PAUSED)
    }

    fn job_with(timeout: Duration, budget: Duration) -> (Arc<Broker>, Arc<Session>, Arc<AskJob>) {
        let broker = Arc::new(Broker::default());
        let phase: SharedPhase = Arc::new(Mutex::new(RunPhase::primary()));
        let session = broker
            .open(
                "s1",
                &["j1".to_string()],
                &CancellationToken::new(),
                phase,
                Duration::from_secs(1),
            )
            .unwrap();
        let job = AskJob::with_limits(
            "j1",
            None,
            session.clone(),
            broker.clone(),
            Arc::default(),
            timeout,
            budget,
        );
        (broker, session, job)
    }

    async fn next_question(session: &Session) -> (u64, String) {
        let Some(JobEvent::Question {
            question_id, text, ..
        }) = session.next().await
        else {
            panic!("no question")
        };
        (question_id, text)
    }

    #[tokio::test]
    async fn answer_reaches_the_blocked_worker_and_pauses_the_job_timeout() {
        let (broker, session, job) = job(Duration::from_secs(30));
        let asking = {
            let job = job.clone();
            tokio::spawn(async move { job.ask("which?".into()).await })
        };
        let (id, text) = next_question(&session).await;
        assert_eq!(text, "which?");
        assert!(job.pause.paused_total() < Duration::from_secs(5));
        assert!(matches!(
            broker.answer("j1", id, "this".into()),
            Answered::Delivered(_)
        ));
        assert_eq!(asking.await.unwrap(), "this");
        assert!(matches!(
            broker.answer("j1", id, "again".into()),
            Answered::NoOpenQuestion(_)
        ));
        assert!(matches!(
            broker.answer("nope", id, "x".into()),
            Answered::Unknown
        ));
        assert_eq!(job.asked(), ["which?"]);
        assert!(!job.unanswered.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_late_answer_for_an_expired_question_never_reaches_the_next_one() {
        let (broker, session, job) = job(Duration::from_millis(100));
        assert_eq!(job.ask("q1".into()).await, NO_ANSWER);
        let (q1, _) = next_question(&session).await;
        assert!(job.unanswered.load(Ordering::SeqCst));
        let long = AskJob::with_limits(
            "j1",
            None,
            session.clone(),
            broker.clone(),
            Arc::default(),
            Duration::from_secs(30),
            MAX_PAUSED,
        );
        let asking = {
            let long = long.clone();
            tokio::spawn(async move { long.ask("q2".into()).await })
        };
        let (q2, _) = next_question(&session).await;
        assert_ne!(q1, q2);
        assert!(
            matches!(broker.answer("j1", q1, "for q1".into()), Answered::OtherQuestionOpen { open_id, ref text } if open_id == q2 && text == "q2")
        );
        assert!(!asking.is_finished());
        assert!(matches!(
            broker.answer("j1", q2, "for q2".into()),
            Answered::Delivered(_)
        ));
        assert_eq!(asking.await.unwrap(), "for q2");
    }

    #[tokio::test]
    async fn concurrent_asks_are_serialized_to_one_open_question() {
        let (broker, session, job) = job(Duration::from_secs(30));
        let spawn = |q: &'static str| {
            let job = job.clone();
            tokio::spawn(async move { job.ask(q.into()).await })
        };
        let (a, b) = (spawn("first"), spawn("second"));
        let (id1, t1) = next_question(&session).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), session.next())
                .await
                .is_err()
        );
        assert!(matches!(
            broker.answer("j1", id1, format!("re {t1}")),
            Answered::Delivered(_)
        ));
        let (id2, t2) = next_question(&session).await;
        assert_ne!(t1, t2);
        assert!(matches!(
            broker.answer("j1", id2, "ok".into()),
            Answered::Delivered(_)
        ));
        let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
        assert!(ra.starts_with("re ") != rb.starts_with("re "));
    }

    #[tokio::test]
    async fn total_paused_time_per_job_is_capped() {
        let (_broker, session, job) =
            job_with(Duration::from_millis(150), Duration::from_millis(200));
        assert_eq!(job.ask("a".into()).await, NO_ANSWER);
        assert_eq!(job.ask("b".into()).await, NO_ANSWER);
        assert_eq!(job.ask("c".into()).await, NO_ANSWER);
        let mut seen = 0;
        while tokio::time::timeout(Duration::from_millis(50), session.next())
            .await
            .is_ok()
        {
            seen += 1;
        }
        assert_eq!(seen, 2);
        assert_eq!(job.asked().len(), 2);
    }

    #[test]
    fn the_ask_timeout_override_is_clamped() {
        assert_eq!(clamp_timeout(Some("999999")), MAX_PAUSED);
        assert_eq!(clamp_timeout(Some("5")), Duration::from_secs(5));
        assert_eq!(
            (clamp_timeout(Some("0")), clamp_timeout(None)),
            (ASK_TIMEOUT, ASK_TIMEOUT)
        );
    }

    #[test]
    fn a_running_id_cannot_be_opened_again_and_a_stale_close_is_ignored() {
        let broker = Broker::default();
        let open = |id: &str, jobs: &[&str]| {
            broker.open(
                id,
                &jobs.iter().map(|j| j.to_string()).collect::<Vec<_>>(),
                &CancellationToken::new(),
                Arc::new(Mutex::new(RunPhase::primary())),
                Duration::from_secs(1),
            )
        };
        let first = open("wf", &["wf-a", "wf-b"]).unwrap();
        assert!(open("wf", &[]).is_err());
        assert!(open("wf-a", &["wf-a"]).is_err());
        assert!(open("other", &["wf-b"]).is_err());
        assert!(broker.taken("wf-a") && broker.taken("wf"));
        broker.close(&first);
        assert!(!broker.taken("wf") && !broker.taken("wf-a"));
        let second = open("wf", &["wf-a"]).unwrap();
        broker.close(&first);
        assert!(broker.taken("wf"));
        broker.close(&second);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_token_is_never_in_the_endpoint_but_in_a_one_shot_0600_file_next_to_the_private_socket(
    ) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (_broker, session, job) = job(Duration::from_millis(300));
        let server = AskServer::start(&dir.path().join("ask"), job).unwrap();
        let path = server.endpoint.clone();
        assert!(!path.contains('#') && !path.contains("j1"));
        let sock_dir = Path::new(&path).parent().unwrap().to_path_buf();
        assert_eq!(
            std::fs::metadata(&sock_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let token_file = sock_dir.join("t");
        assert_eq!(
            std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut bad = transport::connect(&server.endpoint).await.unwrap();
        bad.write_all(format!("{}\n", json!({"token": "nope", "question": "x"})).as_bytes())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), session.next())
                .await
                .is_err()
        );
        let token = transport::take_token(&server.endpoint).unwrap();
        assert!(!token_file.exists() && transport::take_token(&server.endpoint).is_err());
        let mut good = transport::connect(&server.endpoint).await.unwrap();
        good.write_all(format!("{}\n", json!({"token": token, "question": "y"})).as_bytes())
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), session.next()).await,
            Ok(Some(JobEvent::Question { .. }))
        ));
        drop(server);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!sock_dir.exists());
    }

    #[tokio::test]
    async fn an_answer_arriving_after_the_timeout_is_not_reported_as_delivered_and_a_stale_event_is_not_open(
    ) {
        let (broker, session, job) = job(Duration::from_millis(100));
        assert_eq!(job.ask("q1".into()).await, NO_ANSWER);
        let (q1, _) = next_question(&session).await;
        assert!(!broker.is_open("j1", q1));
        assert!(matches!(
            broker.answer("j1", q1, "late".into()),
            Answered::NoOpenQuestion(_)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_long_base_dir_falls_back_to_a_short_temp_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join("a".repeat(60)).join("b".repeat(60));
        let (_broker, _session, job) = job(Duration::from_millis(300));
        let server = AskServer::start(&long, job).unwrap();
        let path = server.endpoint.clone();
        assert!(path.len() <= 100 && Path::new(&path).exists(), "{path}");
        drop(server);
    }

    #[tokio::test]
    async fn unanswered_question_times_out_with_the_fixed_text() {
        let (_broker, session, job) = job(Duration::from_millis(100));
        let answer = job.ask("which?".into()).await;
        assert_eq!(answer, NO_ANSWER);
        assert!(matches!(
            session.next().await,
            Some(JobEvent::Question { .. })
        ));
    }

    #[tokio::test]
    async fn waiting_for_an_answer_does_not_count_against_the_run_timeout() {
        let pause = Arc::new(Pause::default());
        let guard = pause.hold();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            drop(guard);
        });
        let launch = launch(&["-c", "sleep 2"], &[]);
        let launch = Launch {
            program: "sh".into(),
            ..launch
        };
        let logs = tempfile::tempdir().unwrap();
        let req = crate::runner::RunRequest {
            prompt: String::new(),
            cwd: logs.path().into(),
            max_turns: 1,
            model: None,
            allowed_tools: None,
            timeout: Duration::from_secs(1),
        };
        let out = crate::runner::run_launch_with(
            launch,
            req.clone(),
            logs.path(),
            CancellationToken::new(),
            Some(pause),
        )
        .await
        .unwrap();
        release.await.unwrap();
        assert!(!out.timed_out && out.exit_code == Some(0), "{out:?}");
        let launch = Launch {
            program: "sh".into(),
            ..self::tests::launch(&["-c", "sleep 3"], &[])
        };
        let out = crate::runner::run_launch_with(
            launch,
            req,
            logs.path(),
            CancellationToken::new(),
            Some(Arc::default()),
        )
        .await
        .unwrap();
        assert!(out.timed_out);
    }
}
