use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ProgressNotificationParam, ServerCapabilities, ServerConfig},
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
use crate::trust::{Event, Outcome, Visibility};

pub const ASK_SERVER: &str = "sidekick_ask";
pub const ASK_TOOL: &str = "ask_orchestrator";
pub const CLAUDE_TOOL_NAME: &str = "mcp__sidekick_ask__ask_orchestrator";
pub const OPENCODE_TOOL_NAME: &str = "sidekick_ask_ask_orchestrator";
pub const ASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const ENV_ASK_TIMEOUT: &str = "AGENT_SIDEKICK_ASK_TIMEOUT_S";
pub const NO_ANSWER: &str = "Keine Antwort vom Orchestrator: nichts entscheiden, aktuellen Stand abliefern und offene Punkte nennen.";
pub const ASK_PROMPT_LINE: &str = "You have the MCP tool ask_orchestrator(question): use it instead of guessing when a decision is open.";
pub const QUESTIONS_PREFIX: &str = "questions: ";
pub const ENV_ASK_JOB: &str = "AGENT_SIDEKICK_ASK_JOB";
pub const ENV_ASK_SOCKET: &str = "AGENT_SIDEKICK_ASK_SOCKET";
pub const ENV_ASK_EXE: &str = "AGENT_SIDEKICK_ASK_EXE";
const TOOL_TIMEOUT_MARGIN: Duration = Duration::from_secs(5 * 60);
const BRIDGE_PROGRESS: Duration = Duration::from_secs(20);

pub fn ask_timeout() -> Duration {
    std::env::var(ENV_ASK_TIMEOUT).ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|&s| s > 0).map_or(ASK_TIMEOUT, Duration::from_secs)
}

#[derive(Clone, Debug)]
pub struct AskWire {
    pub exe: PathBuf,
    pub job_id: String,
    pub endpoint: String,
}

impl AskWire {
    pub fn bridge_args(&self) -> Vec<String> {
        ["ask-bridge", "--job", &self.job_id, "--socket", &self.endpoint].map(String::from).to_vec()
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
        None => insert_before_separator(&mut launch.args, ["--allowedTools".into(), CLAUDE_TOOL_NAME.into()]),
    }
    insert_before_separator(&mut launch.args, ["--mcp-config".into(), config.to_string().into()]);
    launch.header.push(format!("ask_orchestrator: mcp server {ASK_SERVER} on job {}", wire.job_id));
}

fn opencode_wire(launch: &mut Launch, wire: &AskWire) -> Result<()> {
    let mut command = vec![wire.exe.to_string_lossy().into_owned()];
    command.extend(wire.bridge_args());
    let patch = |launch: &mut Launch, name: &str, f: &dyn Fn(&mut Value)| -> Result<()> {
        let slot = launch.env.iter_mut().find(|(k, _)| k == name).with_context(|| format!("{name} missing in the opencode environment"))?;
        let mut value: Value = serde_json::from_str(&slot.1.to_string_lossy())?;
        f(&mut value);
        slot.1 = value.to_string().into();
        Ok(())
    };
    patch(launch, "OPENCODE_CONFIG_CONTENT", &|v| {
        v["mcp"] = json!({ASK_SERVER: {"type": "local", "command": command, "enabled": true, "timeout": wire.tool_timeout_ms()}});
    })?;
    patch(launch, "OPENCODE_PERMISSION", &|v| v[OPENCODE_TOOL_NAME] = json!("allow"))?;
    launch.header.push(format!("ask_orchestrator: mcp server {ASK_SERVER} on job {}", wire.job_id));
    Ok(())
}

fn toml_value(v: impl serde::Serialize) -> String {
    toml::Value::try_from(v).map(|t| t.to_string()).unwrap_or_default()
}

fn codex_wire(launch: &mut Launch, wire: &AskWire) {
    let key = |k: &str| format!("mcp_servers.{ASK_SERVER}.{k}");
    let flags = [
        (key("command"), toml_value(wire.exe.to_string_lossy())),
        (key("args"), toml_value(wire.bridge_args())),
        (key("tool_timeout_sec"), (wire.tool_timeout_ms() / 1000).to_string()),
    ];
    let at = launch.args.len().saturating_sub(1);
    launch.args.splice(at..at, flags.into_iter().flat_map(|(k, v)| ["-c".into(), format!("{k}={v}").into()]));
    launch.header.push(format!("ask_orchestrator: mcp server {ASK_SERVER} on job {}", wire.job_id));
}

fn command_wire(launch: &mut Launch, wire: &AskWire) {
    for (k, v) in [(ENV_ASK_JOB, wire.job_id.clone()), (ENV_ASK_SOCKET, wire.endpoint.clone()), (ENV_ASK_EXE, wire.exe.to_string_lossy().into_owned())] {
        launch.env.retain(|(n, _)| n != k);
        launch.env.push((k.into(), v.into()));
    }
    launch.header.push(format!("ask_orchestrator: bridge in {ENV_ASK_JOB}, {ENV_ASK_SOCKET}, {ENV_ASK_EXE}"));
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

    pub fn bind(dir: &Path, job_id: &str) -> Result<(Listener, String, String)> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let path = dir.join(format!("{job_id}.sock"));
        if path.as_os_str().len() > 100 {
            bail!("ask socket path too long: {}", path.display());
        }
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok((Listener(listener, path.clone()), path.to_string_lossy().into_owned(), String::new()))
    }

    impl Listener {
        pub async fn accept(&self) -> std::io::Result<Stream> {
            self.0.accept().await.map(|(s, _)| s)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.1);
        }
    }

    pub async fn connect(endpoint: &str) -> std::io::Result<(Stream, String)> {
        Ok((tokio::net::UnixStream::connect(endpoint).await?, String::new()))
    }
}

#[cfg(not(unix))]
mod transport {
    use super::*;
    pub type Stream = tokio::net::TcpStream;
    pub struct Listener(tokio::net::TcpListener);

    pub fn bind(_dir: &Path, _job_id: &str) -> Result<(Listener, String, String)> {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        std_listener.set_nonblocking(true)?;
        let port = std_listener.local_addr()?.port();
        let token = uuid::Uuid::new_v4().simple().to_string();
        Ok((Listener(tokio::net::TcpListener::from_std(std_listener)?), format!("127.0.0.1:{port}#{token}"), token))
    }

    impl Listener {
        pub async fn accept(&self) -> std::io::Result<Stream> {
            self.0.accept().await.map(|(s, _)| s)
        }
    }

    pub async fn connect(endpoint: &str) -> std::io::Result<(Stream, String)> {
        let (addr, token) = endpoint.split_once('#').unwrap_or((endpoint, ""));
        Ok((tokio::net::TcpStream::connect(addr).await?, token.to_string()))
    }
}

#[derive(Debug)]
pub enum JobEvent {
    Question { job_id: String, step: Option<String>, text: String },
    Done { text: String, is_error: bool },
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
    pending: HashMap<String, Vec<(u64, oneshot::Sender<String>)>>,
    next: u64,
}

#[derive(Debug, Default)]
pub struct Broker {
    state: Mutex<BrokerState>,
}

pub enum Answered {
    Delivered(Arc<Session>),
    NoOpenQuestion(Arc<Session>),
    Unknown,
}

impl Broker {
    fn st(&self) -> std::sync::MutexGuard<'_, BrokerState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn open(&self, id: &str, shutdown: &CancellationToken, phase: SharedPhase, timeout: Duration) -> Arc<Session> {
        let (tx, rx) = mpsc::unbounded_channel();
        let session = Arc::new(Session { id: id.to_string(), timeout, cancel: shutdown.child_token(), phase, tx, rx: tokio::sync::Mutex::new(rx) });
        self.st().sessions.insert(id.to_string(), session.clone());
        session
    }

    pub fn attach(&self, job_id: &str, session_id: &str) {
        self.st().job_session.insert(job_id.to_string(), session_id.to_string());
    }

    pub fn close(&self, session_id: &str) {
        let mut st = self.st();
        st.sessions.remove(session_id);
        let jobs: Vec<String> = st.job_session.iter().filter(|(_, s)| *s == session_id).map(|(j, _)| j.clone()).collect();
        for j in jobs {
            st.job_session.remove(&j);
            st.pending.remove(&j);
        }
    }

    pub fn session_of(&self, job_id: &str) -> Option<Arc<Session>> {
        let st = self.st();
        st.job_session.get(job_id).and_then(|s| st.sessions.get(s)).cloned()
    }

    pub fn answer(&self, job_id: &str, text: String) -> Answered {
        let Some(session) = self.session_of(job_id) else { return Answered::Unknown };
        let mut st = self.st();
        let queue = st.pending.entry(job_id.to_string()).or_default();
        while !queue.is_empty() {
            let (_, tx) = queue.remove(0);
            if tx.send(text.clone()).is_ok() {
                return Answered::Delivered(session);
            }
        }
        Answered::NoOpenQuestion(session)
    }

    fn add_pending(&self, job_id: &str) -> (u64, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        let mut st = self.st();
        st.next += 1;
        let id = st.next;
        st.pending.entry(job_id.to_string()).or_default().push((id, tx));
        (id, rx)
    }

    fn drop_pending(&self, job_id: &str, id: u64) {
        if let Some(q) = self.st().pending.get_mut(job_id) {
            q.retain(|(i, _)| *i != id);
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
    pub questions: Mutex<Vec<String>>,
}

impl AskJob {
    pub fn new(job_id: &str, step: Option<String>, session: Arc<Session>, broker: Arc<Broker>, pause: Arc<Pause>) -> Arc<Self> {
        Arc::new(Self { job_id: job_id.into(), step, session, broker, pause, timeout: ask_timeout(), questions: Mutex::default() })
    }

    pub fn asked(&self) -> Vec<String> {
        self.questions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    pub async fn ask(&self, question: String) -> String {
        self.questions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(question.clone());
        let (id, rx) = self.broker.add_pending(&self.job_id);
        let _hold = self.pause.hold();
        self.session.send(JobEvent::Question { job_id: self.job_id.clone(), step: self.step.clone(), text: question });
        let answer = tokio::select! {
            a = tokio::time::timeout(self.timeout, rx) => a.ok().and_then(Result::ok),
            _ = self.session.cancel.cancelled() => None,
        };
        self.broker.drop_pending(&self.job_id, id);
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
    let Ok(req) = serde_json::from_str::<Request>(&line) else { return };
    if req.token != token {
        return;
    }
    let mut eof = String::new();
    let answer = tokio::select! {
        a = job.ask(req.question) => a,
        _ = reader.read_line(&mut eof) => return,
    };
    let _ = write.write_all(format!("{}\n", json!({"answer": answer})).as_bytes()).await;
    let _ = write.shutdown().await;
}

impl AskServer {
    pub fn start(dir: &Path, job: Arc<AskJob>) -> Result<Self> {
        let (listener, endpoint, token) = transport::bind(dir, &job.job_id)?;
        Ok(Self { endpoint, handle: tokio::spawn(accept_loop(listener, job, token)) })
    }
}

async fn accept_loop(listener: transport::Listener, job: Arc<AskJob>, token: String) {
    while let Ok(stream) = listener.accept().await {
        tokio::spawn(handle(stream, job.clone(), token.clone()));
    }
}

pub fn question_events(base: &Event, questions: &[String]) -> Vec<Event> {
    if questions.is_empty() {
        return Vec::new();
    }
    let note = |summary: String, visibility: Visibility| {
        let mut e = Event::new(&base.model_id, base.dimension, base.scope.clone(), Outcome::Note, base.ts_ms);
        e.machine_id = base.machine_id.clone();
        e.user_id = base.user_id.clone();
        e.job_id = base.job_id.clone();
        e.summary = Some(summary);
        e.visibility = visibility;
        e
    };
    let texts: Vec<String> = questions.iter().enumerate().map(|(i, q)| format!("{}. {q}", i + 1)).collect();
    vec![note(format!("{QUESTIONS_PREFIX}{}", questions.len()), Visibility::Shared), note(format!("questions:\n{}", texts.join("\n")), Visibility::Private)]
}

pub fn count_questions(events: &[Event], model_id: &str) -> u32 {
    events
        .iter()
        .filter(|e| e.model_id == model_id && e.outcome == Outcome::Note)
        .filter_map(|e| e.summary.as_deref()?.strip_prefix(QUESTIONS_PREFIX)?.trim().parse::<u32>().ok())
        .sum()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AskParams {
    pub question: String,
}

#[derive(Clone)]
struct Bridge {
    endpoint: String,
}

impl Bridge {
    async fn forward(&self, question: &str) -> Result<String> {
        let (stream, token) = transport::connect(&self.endpoint).await.context("the orchestrator is not reachable")?;
        let (read, mut write) = tokio::io::split(stream);
        write.write_all(format!("{}\n", json!({"token": token, "question": question})).as_bytes()).await?;
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await?;
        if line.is_empty() {
            bail!("the orchestrator connection closed without an answer");
        }
        Ok(serde_json::from_str::<Value>(&line)?["answer"].as_str().unwrap_or(NO_ANSWER).to_string())
    }
}

#[tool_router]
impl Bridge {
    /// Ask the orchestrator (the agent that gave you this task) a question and wait for its answer. Use it instead of guessing whenever a decision is open or the task is ambiguous. The call can block for minutes.
    #[tool]
    async fn ask_orchestrator(&self, Parameters(p): Parameters<AskParams>, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let ticker = ctx.meta.get_progress_token().map(|token| {
            let peer = ctx.peer.clone();
            tokio::spawn(async move {
                let mut n = 0.0;
                loop {
                    tokio::time::sleep(BRIDGE_PROGRESS).await;
                    n += 1.0;
                    if peer.notify_progress(ProgressNotificationParam::new(token.clone(), n).with_message("waiting for the orchestrator")).await.is_err() {
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
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(Implementation::new(ASK_SERVER, env!("CARGO_PKG_VERSION")))
    }
}

pub async fn run_bridge(_job: &str, endpoint: &str) -> Result<()> {
    let service = Bridge { endpoint: endpoint.to_string() }.serve(rmcp::transport::stdio()).await.map_err(|e| anyhow::anyhow!("MCP initialization failed: {e}"))?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::RunPhase;

    fn wire() -> AskWire {
        AskWire { exe: PathBuf::from("/bin/agent-sidekick"), job_id: "j-1".into(), endpoint: "/tmp/x.sock".into() }
    }

    fn launch(args: &[&str], env: &[(&str, &str)]) -> Launch {
        Launch {
            program: "p".into(),
            args: args.iter().map(OsString::from).collect(),
            env: env.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))).collect(),
            stdin: None,
            secrets: crate::redact::Secrets::none(),
            header: Vec::new(),
        }
    }

    fn strs(l: &Launch) -> Vec<String> {
        l.args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn claude_gets_mcp_config_and_the_tool_stays_allowed_with_the_prompt_last() {
        let mut l = launch(&["-p", "--allowedTools", "Read", "Grep", "--disallowedTools", "Edit", "--", "do it"], &[]);
        wire_launch(HarnessKind::Claude, &mut l, &wire()).unwrap();
        let a = strs(&l);
        assert_eq!(&a[a.len() - 2..], ["--", "do it"]);
        let at = a.iter().position(|x| x == "--allowedTools").unwrap();
        assert_eq!(a[at + 1], CLAUDE_TOOL_NAME);
        let cfg: Value = serde_json::from_str(&a[a.iter().position(|x| x == "--mcp-config").unwrap() + 1]).unwrap();
        let server = &cfg["mcpServers"][ASK_SERVER];
        assert_eq!(server["command"], "/bin/agent-sidekick");
        assert_eq!(server["args"], json!(["ask-bridge", "--job", "j-1", "--socket", "/tmp/x.sock"]));
        assert!(server["timeout"].as_u64().unwrap() > ASK_TIMEOUT.as_millis() as u64);
        let mut bare = launch(&["-p"], &[]);
        wire_launch(HarnessKind::Claude, &mut bare, &wire()).unwrap();
        assert!(strs(&bare).windows(2).any(|w| w[0] == "--allowedTools" && w[1] == CLAUDE_TOOL_NAME));
    }

    #[test]
    fn opencode_gets_an_mcp_entry_a_long_timeout_and_the_permission() {
        let mut l = launch(&["run"], &[("OPENCODE_CONFIG_CONTENT", r#"{"agent":{}}"#), ("OPENCODE_PERMISSION", r#"{"edit":"deny"}"#)]);
        wire_launch(HarnessKind::OpenCode, &mut l, &wire()).unwrap();
        let get = |n: &str| serde_json::from_str::<Value>(&l.env.iter().find(|(k, _)| k == n).unwrap().1.to_string_lossy()).unwrap();
        let entry = &get("OPENCODE_CONFIG_CONTENT")["mcp"][ASK_SERVER];
        assert_eq!(entry["type"], "local");
        assert_eq!(entry["command"][1], "ask-bridge");
        assert!(entry["timeout"].as_u64().unwrap() > ASK_TIMEOUT.as_millis() as u64);
        let perm = get("OPENCODE_PERMISSION");
        assert_eq!((perm[OPENCODE_TOOL_NAME].as_str(), perm["edit"].as_str()), (Some("allow"), Some("deny")));
        assert!(wire_launch(HarnessKind::OpenCode, &mut launch(&["run"], &[]), &wire()).is_err());
    }

    #[test]
    fn codex_gets_mcp_servers_flags_before_the_stdin_dash_and_commands_get_env() {
        let mut l = launch(&["exec", "--json", "-"], &[]);
        wire_launch(HarnessKind::Codex, &mut l, &wire()).unwrap();
        let a = strs(&l);
        assert_eq!(a.last().unwrap(), "-");
        assert!(a.contains(&format!("mcp_servers.{ASK_SERVER}.command=\"/bin/agent-sidekick\"")), "{a:?}");
        assert!(a.contains(&format!("mcp_servers.{ASK_SERVER}.args=[\"ask-bridge\", \"--job\", \"j-1\", \"--socket\", \"/tmp/x.sock\"]")), "{a:?}");
        assert!(a.iter().any(|x| x.starts_with(&format!("mcp_servers.{ASK_SERVER}.tool_timeout_sec="))));
        let mut c = launch(&["x"], &[(ENV_ASK_JOB, "stale")]);
        wire_launch(HarnessKind::Command, &mut c, &wire()).unwrap();
        assert_eq!(c.env.iter().filter(|(k, _)| k == ENV_ASK_JOB).count(), 1);
        assert!(c.env.iter().any(|(k, v)| k == ENV_ASK_SOCKET && v == "/tmp/x.sock"));
    }

    #[test]
    fn questions_become_a_shared_count_and_a_private_text_note() {
        let mut base = Event::new("m1", crate::trust::Dimension::Search, crate::trust::Scope::Global, Outcome::Note, 5);
        base.job_id = Some("j".into());
        assert!(question_events(&base, &[]).is_empty());
        let events = question_events(&base, &["a?".into(), "b?".into()]);
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].summary.as_deref(), events[0].visibility), (Some("questions: 2"), Visibility::Shared));
        assert_eq!(events[1].visibility, Visibility::Private);
        assert!(events[1].summary.as_deref().unwrap().contains("2. b?"));
        let shared: Vec<Event> = events.iter().map(crate::data::strip_private).collect();
        assert_eq!(count_questions(&shared, "m1"), 2);
        assert_eq!(count_questions(&shared, "other"), 0);
        assert!(events.iter().all(|e| e.outcome == Outcome::Note && e.conclusion.is_none()));
    }

    fn job(timeout: Duration) -> (Arc<Broker>, Arc<Session>, Arc<AskJob>) {
        let broker = Arc::new(Broker::default());
        let phase: SharedPhase = Arc::new(Mutex::new(RunPhase::primary()));
        let session = broker.open("s1", &CancellationToken::new(), phase, Duration::from_secs(1));
        broker.attach("j1", "s1");
        let job = Arc::new(AskJob { job_id: "j1".into(), step: None, session: session.clone(), broker: broker.clone(), pause: Arc::default(), timeout, questions: Mutex::default() });
        (broker, session, job)
    }

    #[tokio::test]
    async fn answer_reaches_the_blocked_worker_and_pauses_the_job_timeout() {
        let (broker, session, job) = job(Duration::from_secs(30));
        let asking = { let job = job.clone(); tokio::spawn(async move { job.ask("which?".into()).await }) };
        let Some(JobEvent::Question { job_id, text, .. }) = session.next().await else { panic!("no question") };
        assert_eq!((job_id.as_str(), text.as_str()), ("j1", "which?"));
        assert!(job.pause.paused_total() < Duration::from_secs(5));
        assert!(matches!(broker.answer("j1", "this".into()), Answered::Delivered(_)));
        assert_eq!(asking.await.unwrap(), "this");
        assert!(matches!(broker.answer("j1", "again".into()), Answered::NoOpenQuestion(_)));
        assert!(matches!(broker.answer("nope", "x".into()), Answered::Unknown));
        assert_eq!(job.asked(), ["which?"]);
    }

    #[tokio::test]
    async fn unanswered_question_times_out_with_the_fixed_text() {
        let (_broker, session, job) = job(Duration::from_millis(100));
        let answer = job.ask("which?".into()).await;
        assert_eq!(answer, NO_ANSWER);
        assert!(matches!(session.next().await, Some(JobEvent::Question { .. })));
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
        let launch = Launch { program: "sh".into(), ..launch };
        let logs = tempfile::tempdir().unwrap();
        let req = crate::runner::RunRequest { prompt: String::new(), cwd: logs.path().into(), max_turns: 1, model: None, allowed_tools: None, timeout: Duration::from_secs(1) };
        let out = crate::runner::run_launch_with(launch, req.clone(), logs.path(), CancellationToken::new(), Some(pause)).await.unwrap();
        release.await.unwrap();
        assert!(!out.timed_out && out.exit_code == Some(0), "{out:?}");
        let launch = Launch { program: "sh".into(), ..self::tests::launch(&["-c", "sleep 3"], &[]) };
        let out = crate::runner::run_launch_with(launch, req, logs.path(), CancellationToken::new(), Some(Arc::default())).await.unwrap();
        assert!(out.timed_out);
    }
}
