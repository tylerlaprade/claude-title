//! Rex's control API, spoken over the server's socket so title updates never
//! share the terminal's output stream. The title becomes the label of the Rex
//! window, which Rex shows as its tab, while the session's terminal is the only
//! block in that window.

use crate::title::{Spinner, TerminalDetached};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{Message, WebSocket};

const PROTOCOL: &str = "rex.control.v1";
const API_REVISION: u64 = 11;
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);
const LABEL_LIMIT: usize = 128;

#[derive(Debug)]
struct Refusal {
    code: String,
    message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Rex refused the request ({}): {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for Refusal {}

fn refused(error: &anyhow::Error, code: &str) -> bool {
    error
        .downcast_ref::<Refusal>()
        .is_some_and(|refusal| refusal.code == code)
}

fn waiting(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

struct Client {
    socket: WebSocket<UnixStream>,
    requests: u64,
}

impl Client {
    fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("failed to connect to Rex at {}", path.display()))?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
        stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
        let mut request = "ws://localhost/api/control".into_client_request()?;
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(PROTOCOL));
        let (socket, _) = tungstenite::client(request, stream)
            .map_err(|error| anyhow!("Rex refused the control connection: {error}"))?;
        let mut client = Self {
            socket,
            requests: 0,
        };
        client.send(&json!({
            "type": "hello",
            "api_revision": API_REVISION,
            "api_min_revision": API_REVISION,
            "client": {
                "kind": "other",
                "name": "claude-title",
                "version": env!("CARGO_PKG_VERSION"),
            },
        }))?;
        let welcome = client
            .receive()?
            .ok_or_else(|| anyhow!("Rex sent no welcome within {REPLY_TIMEOUT:?}"))?;
        ensure!(
            welcome["type"] == "welcome" && welcome["protocol"] == PROTOCOL,
            "Rex answered the hello with {welcome}"
        );
        Ok(client)
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        self.socket
            .send(Message::text(message.to_string()))
            .context("failed to write to Rex")
    }

    fn receive(&mut self) -> Result<Option<Value>> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    return serde_json::from_str(&text)
                        .map(Some)
                        .context("Rex sent a message that isn't JSON");
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => {}
                Ok(Message::Close(frame)) => {
                    bail!("Rex closed the control connection: {frame:?}")
                }
                Ok(Message::Binary(_) | Message::Frame(_)) => {
                    bail!("Rex sent binary data on the control connection")
                }
                Err(tungstenite::Error::Io(error)) if waiting(&error) => return Ok(None),
                Err(error) => return Err(error).context("lost the Rex control connection"),
            }
        }
    }

    fn request(&mut self, method: &str, payload: &Value, events: &mut Vec<Value>) -> Result<Value> {
        self.requests += 1;
        let id = self.requests.to_string();
        self.send(&json!({"type": "request", "id": id, "method": method, "payload": payload}))?;
        loop {
            let mut message = self
                .receive()?
                .ok_or_else(|| anyhow!("Rex did not answer {method} within {REPLY_TIMEOUT:?}"))?;
            match message["type"].as_str() {
                Some("response") if message["id"] == id.as_str() => {
                    let error = message["error"].take();
                    if error.is_null() {
                        return Ok(message["payload"].take());
                    }
                    return Err(Refusal {
                        code: error["code"].as_str().unwrap_or_default().to_owned(),
                        message: error["message"].as_str().unwrap_or_default().to_owned(),
                    }
                    .into());
                }
                Some("event") => events.push(message),
                // Rex adds message types over time and asks clients to skip
                // the ones they don't know.
                _ => {}
            }
        }
    }

    fn pending_events(&mut self) -> Result<Vec<Value>> {
        self.socket.get_ref().set_nonblocking(true)?;
        let mut events = Vec::new();
        let drained = self.drain(&mut events);
        self.socket.get_ref().set_nonblocking(false)?;
        drained?;
        self.socket.flush().context("failed to answer Rex")?;
        Ok(events)
    }

    fn drain(&mut self, events: &mut Vec<Value>) -> Result<()> {
        while let Some(message) = self.receive()? {
            if message["type"] == "event" {
                events.push(message);
            }
        }
        Ok(())
    }
}

fn socket_path(server: &str) -> Option<PathBuf> {
    let mut rest = server.strip_prefix("unix://")?.as_bytes();
    let mut bytes = Vec::with_capacity(rest.len());
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let digits = tail.get(..2)?;
            if !digits.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            bytes.push(u8::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

// Rex trims a label, rejects control characters, and caps it at 128
// characters, so the label sent must already be in that form to be recognized
// when Rex reports it back.
fn label(title: &str) -> String {
    let visible: String = title
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    visible
        .trim()
        .chars()
        .take(LABEL_LIMIT)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

#[derive(Serialize, Deserialize)]
struct Original {
    window_id: String,
    label: String,
}

struct Window {
    id: String,
    label: String,
}

fn sole_window(view: &Value, window_id: &str, block_id: &str) -> Option<Window> {
    let window = view["windows"]
        .as_array()?
        .iter()
        .find(|window| window["window_id"] == window_id)?;
    let mut blocks = window["layers"]
        .as_array()?
        .iter()
        .filter_map(|layer| layer["blocks"].as_array())
        .flatten();
    let only = blocks.next()?;
    (only["block_id"] == block_id && blocks.next().is_none()).then(|| Window {
        id: window_id.to_owned(),
        label: window["label"].as_str().unwrap_or_default().to_owned(),
    })
}

pub(crate) struct Connection {
    client: Client,
    block_id: String,
    owner: u32,
    session_id: String,
    window: Option<Window>,
    revision: u64,
    unpaired_labels: u64,
    stale: bool,
    wanted: Option<String>,
    echoes: VecDeque<String>,
    original: Option<Original>,
    original_path: PathBuf,
    renamed: Option<String>,
    reclaim: bool,
}

impl Connection {
    pub(crate) fn open(owner: u32, original_path: &Path) -> Result<Self> {
        let (Ok(block_id), Ok(server)) = (env::var("REX_BLOCK"), env::var("REX_SERVER")) else {
            return Err(TerminalDetached.into());
        };
        let path = socket_path(&server)
            .ok_or_else(|| anyhow!("Rex server {server} isn't a local socket"))?;
        Self::connect(&path, block_id, owner, original_path)
    }

    fn connect(path: &Path, block_id: String, owner: u32, original_path: &Path) -> Result<Self> {
        let mut client = Client::connect(path)?;
        let mut events = Vec::new();
        client.request(
            "session.attach_all",
            &json!({"event_scope": "session"}),
            &mut events,
        )?;
        let original = fs::read(original_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let mut connection = Self {
            client,
            block_id,
            owner,
            session_id: String::new(),
            window: None,
            revision: 0,
            unpaired_labels: 0,
            stale: true,
            wanted: None,
            echoes: VecDeque::new(),
            original,
            original_path: original_path.to_owned(),
            renamed: None,
            reclaim: false,
        };
        connection.locate(&mut events)?;
        connection.confirm_owner(&mut events)?;
        connection.observe(events);
        Ok(connection)
    }

    pub(crate) fn write(&mut self, title: &str) -> Result<()> {
        self.wanted = Some(label(title));
        self.poll()?;
        self.show()
    }

    pub(crate) fn poll(&mut self) -> Result<()> {
        let events = self.client.pending_events()?;
        self.observe(events);
        if std::mem::take(&mut self.reclaim)
            && let (Some(name), Some(window)) = (self.renamed.clone(), &self.window)
        {
            let window_id = window.id.clone();
            self.set_label(&window_id, &name)?;
        }
        if self.stale {
            let mut events = Vec::new();
            self.locate(&mut events)?;
            self.observe(events);
        }
        Ok(())
    }

    // Claude Code reports its own state to Rex (OSC 7501) unless
    // CLAUDE_CODE_DISABLE_TERMINAL_TITLE is set, and Rex then animates the tab.
    pub(crate) fn spinner(&mut self) -> Result<Spinner> {
        let mut events = Vec::new();
        let status = self.client.request(
            "com.superlogical.terminal.program_status",
            &json!({"session_id": self.session_id, "block_id": self.block_id, "args": {}}),
            &mut events,
        )?;
        self.observe(events);
        let reported = status["records"].as_array().is_some_and(|records| {
            records
                .iter()
                .any(|record| record["owner"]["pid"] == self.owner)
        });
        Ok(if reported {
            Spinner::ByTerminal
        } else {
            Spinner::EverySecond
        })
    }

    fn confirm_owner(&mut self, events: &mut Vec<Value>) -> Result<()> {
        let process = self.client.request(
            "com.superlogical.terminal.process",
            &json!({"session_id": self.session_id, "block_id": self.block_id, "args": {}}),
            events,
        )?;
        let child = process["child"]["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or(TerminalDetached)?;
        if !crate::probe::descends_from(self.owner, child)? {
            return Err(TerminalDetached.into());
        }
        Ok(())
    }

    fn locate(&mut self, events: &mut Vec<Value>) -> Result<()> {
        let mut resolved = self.client.request(
            "target.resolve",
            &json!({"kinds": ["block"], "queries": [self.block_id]}),
            events,
        )?;
        let result = resolved["results"][0].take();
        if result["status"] != "resolved" {
            return Err(TerminalDetached.into());
        }
        let session_id = result["target"]["session_id"]
            .as_str()
            .ok_or_else(|| anyhow!("Rex resolved the block without its session"))?
            .to_owned();
        let view =
            self.client
                .request("session.view", &json!({"session_id": session_id}), events)?;
        if session_id != self.session_id {
            self.session_id = session_id;
            self.unpaired_labels = 0;
        }
        self.revision = view["revision"].as_u64().unwrap_or_default();
        self.stale = false;
        let found = result["target"]["window_id"]
            .as_str()
            .and_then(|window_id| sole_window(&view, window_id, &self.block_id));
        self.place(found)
    }

    fn place(&mut self, found: Option<Window>) -> Result<()> {
        let current = self.window.as_ref().map(|window| window.id.as_str());
        if current.is_some() && current != found.as_ref().map(|window| window.id.as_str()) {
            self.restore()?;
            self.renamed = None;
            self.echoes.clear();
        }
        self.window = found;
        if self.original.as_ref().is_some_and(|original| {
            self.window
                .as_ref()
                .is_none_or(|window| window.id != original.window_id)
        }) {
            self.forget_original()?;
        }
        self.show()
    }

    fn show(&mut self) -> Result<()> {
        let Some(wanted) = self.wanted.clone() else {
            return Ok(());
        };
        if wanted.is_empty() {
            return self.restore();
        }
        if self.renamed.is_some() {
            return Ok(());
        }
        let Some(window) = &self.window else {
            return Ok(());
        };
        if window.label == wanted {
            return Ok(());
        }
        let window_id = window.id.clone();
        if self.original.is_none() {
            let original = Original {
                window_id: window_id.clone(),
                label: window.label.clone(),
            };
            fs::write(&self.original_path, serde_json::to_vec(&original)?)
                .with_context(|| format!("failed to write {}", self.original_path.display()))?;
            self.original = Some(original);
        }
        self.set_label(&window_id, &wanted)?;
        if let Some(window) = &mut self.window {
            window.label = wanted;
        }
        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        let Some(original) = self.original.take() else {
            return Ok(());
        };
        self.forget_original()?;
        let still_ours = self.renamed.is_none()
            && self.window.as_ref().is_some_and(|window| {
                window.id == original.window_id && window.label != original.label
            });
        if still_ours {
            self.set_label(&original.window_id, &original.label)?;
            if let Some(window) = &mut self.window {
                window.label = original.label;
            }
        }
        Ok(())
    }

    fn forget_original(&mut self) -> Result<()> {
        self.original = None;
        match fs::remove_file(&self.original_path) {
            Err(error) if error.kind() != ErrorKind::NotFound => Err(error)
                .with_context(|| format!("failed to remove {}", self.original_path.display())),
            Ok(()) | Err(_) => Ok(()),
        }
    }

    fn set_label(&mut self, window_id: &str, label: &str) -> Result<()> {
        let mut events = Vec::new();
        let result = self.client.request(
            "session.set_window_label",
            &json!({"session_id": self.session_id, "window_id": window_id, "label": label}),
            &mut events,
        );
        match result {
            Ok(_) => self.echoes.push_back(label.to_owned()),
            Err(error) if refused(&error, "not_found") => self.stale = true,
            Err(error) => return Err(error),
        }
        self.observe(events);
        Ok(())
    }

    fn observe(&mut self, events: Vec<Value>) {
        for mut event in events {
            let mut session_event = event["payload"].take();
            if session_event["type"] != "session_event"
                || session_event["payload"]["session_id"] != self.session_id.as_str()
            {
                continue;
            }
            let change = session_event["payload"]["event"].take();
            match change["type"].as_str() {
                Some("window_label_changed") => self.observe_label(&change["payload"]),
                Some("session_view_changed") => self.observe_view(&change["payload"]),
                // Every other change a title depends on also bumps the view,
                // and Rex asks clients to skip the events they don't know.
                _ => {}
            }
        }
    }

    // Rex sends every relabel as window_label_changed and then
    // session_view_changed. Pairing them keeps the spinner frames of other
    // sessions' tabs from triggering a fresh read of the session's layout.
    fn observe_label(&mut self, change: &Value) {
        self.unpaired_labels += 1;
        let Some(window) = self
            .window
            .as_mut()
            .filter(|window| change["window_id"] == window.id.as_str())
        else {
            return;
        };
        let label = change["label"].as_str().unwrap_or_default();
        if let Some(position) = self.echoes.iter().position(|echo| echo == label) {
            self.echoes.drain(..=position);
            if self.echoes.is_empty() {
                label.clone_into(&mut window.label);
            }
            if self.renamed.as_deref().is_some_and(|name| name != label) {
                self.reclaim = true;
            }
        } else {
            label.clone_into(&mut window.label);
            if self.original.is_some() {
                self.renamed = Some(label.to_owned());
                self.reclaim = false;
            }
        }
    }

    fn observe_view(&mut self, change: &Value) {
        if self.unpaired_labels > 0 {
            self.unpaired_labels -= 1;
        } else if change["revision"]
            .as_u64()
            .is_some_and(|revision| revision > self.revision)
        {
            self.stale = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::process::Command;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::thread;
    use tungstenite::handshake::server::{Callback, ErrorResponse, Request, Response};

    const SESSION: &str = "session:work";
    const CLAUDE: &str = "block:claude";
    const OURS: &str = "window:claude";
    const OTHER: &str = "window:other";

    struct FakeWindow {
        id: &'static str,
        label: String,
        blocks: Vec<String>,
    }

    struct Rex {
        socket: Option<WebSocket<UnixStream>>,
        windows: Vec<FakeWindow>,
        revision: u64,
        seq: u64,
        views: usize,
        relabels: usize,
        child: u32,
        status_owner: Option<u32>,
        before_next_request: Vec<(&'static str, String)>,
    }

    type Failure = (&'static str, String);

    impl Rex {
        fn window(&mut self, id: &str) -> &mut FakeWindow {
            self.windows
                .iter_mut()
                .find(|window| window.id == id)
                .unwrap()
        }

        fn label(&mut self, id: &str) -> String {
            self.window(id).label.clone()
        }

        fn emit(&mut self, event: &Value) {
            self.seq += 1;
            let message = json!({
                "type": "event",
                "seq": self.seq,
                "payload": {
                    "type": "session_event",
                    "payload": {"session_id": SESSION, "event": event},
                },
            });
            let socket = self.socket.as_mut().unwrap();
            socket.send(Message::text(message.to_string())).unwrap();
        }

        fn view_changed(&mut self) {
            self.revision += 1;
            let revision = self.revision;
            self.emit(&json!({"type": "session_view_changed", "payload": {"revision": revision}}));
        }

        fn relabel(&mut self, id: &'static str, label: &str) {
            if self.window(id).label == label {
                return;
            }
            label.clone_into(&mut self.window(id).label);
            self.emit(&json!({
                "type": "window_label_changed",
                "payload": {"window_id": id, "label": label},
            }));
            self.view_changed();
        }

        fn split(&mut self, id: &str) {
            self.window(id).blocks.push("block:shell".to_owned());
            self.view_changed();
        }

        fn unsplit(&mut self, id: &str) {
            self.window(id).blocks.truncate(1);
            self.view_changed();
        }

        fn view(&mut self) -> Value {
            self.views += 1;
            let windows: Vec<Value> = self
                .windows
                .iter()
                .map(|window| {
                    let blocks: Vec<Value> = window
                        .blocks
                        .iter()
                        .map(|block| json!({"block_id": block}))
                        .collect();
                    json!({
                        "window_id": window.id,
                        "label": window.label,
                        "layers": [{"kind": "tiled", "blocks": blocks}],
                    })
                })
                .collect();
            json!({"revision": self.revision, "session_id": SESSION, "windows": windows})
        }

        fn answer(&mut self, method: &str, payload: &Value) -> Result<Value, Failure> {
            for (id, label) in std::mem::take(&mut self.before_next_request) {
                self.relabel(id, &label);
            }
            match method {
                "session.attach_all" => Ok(json!({"sessions": [{"session_id": SESSION}]})),
                "target.resolve" => {
                    let block = payload["queries"][0].as_str().unwrap_or_default();
                    let window = self
                        .windows
                        .iter()
                        .find(|window| window.blocks.iter().any(|held| held == block));
                    Ok(window.map_or_else(
                        || json!({"results": [{"query": block, "status": "not_found"}]}),
                        |window| {
                            json!({"results": [{"query": block, "status": "resolved", "target": {
                                "kind": "block",
                                "session_id": SESSION,
                                "window_id": window.id,
                                "block_id": block,
                                "label": "shell",
                            }}]})
                        },
                    ))
                }
                "session.view" => Ok(self.view()),
                "com.superlogical.terminal.process" => Ok(json!({"child": {"pid": self.child}})),
                "com.superlogical.terminal.program_status" => {
                    let records: Vec<Value> = self
                        .status_owner
                        .iter()
                        .map(|pid| json!({"state": "working", "owner": {"pid": pid, "name": "claude"}}))
                        .collect();
                    Ok(json!({"records": records}))
                }
                "session.set_window_label" => {
                    self.relabels += 1;
                    let label = payload["label"].as_str().unwrap_or_default().trim();
                    if label.chars().count() > LABEL_LIMIT || label.chars().any(char::is_control) {
                        return Err(("bad_request", "invalid label".to_owned()));
                    }
                    let id = payload["window_id"].as_str().unwrap_or_default();
                    let Some(window) = self.windows.iter().find(|window| window.id == id) else {
                        return Err(("not_found", "window not found".to_owned()));
                    };
                    let id = window.id;
                    self.relabel(id, label);
                    Ok(json!({"revision": self.revision}))
                }
                other => Err(("method_not_found", other.to_owned())),
            }
        }
    }

    struct Fake {
        rex: Arc<Mutex<Rex>>,
        path: PathBuf,
        original: PathBuf,
        _directory: tempfile::TempDir,
    }

    impl Fake {
        fn start() -> Self {
            Self::with_child(std::process::id())
        }

        fn with_child(child: u32) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("server.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let rex = Arc::new(Mutex::new(Rex {
                socket: None,
                windows: vec![
                    FakeWindow {
                        id: OURS,
                        label: "work".to_owned(),
                        blocks: vec![CLAUDE.to_owned()],
                    },
                    FakeWindow {
                        id: OTHER,
                        label: "other".to_owned(),
                        blocks: vec!["block:other".to_owned()],
                    },
                ],
                revision: 1,
                seq: 0,
                views: 0,
                relabels: 0,
                child,
                status_owner: None,
                before_next_request: Vec::new(),
            }));
            let served = Arc::clone(&rex);
            thread::spawn(move || serve(&listener, &served));
            Self {
                rex,
                path,
                original: directory.path().join("ttys001.rex.json"),
                _directory: directory,
            }
        }

        fn connect(&self) -> Result<Connection> {
            Connection::connect(
                &self.path,
                CLAUDE.to_owned(),
                std::process::id(),
                &self.original,
            )
        }

        fn rex(&self) -> MutexGuard<'_, Rex> {
            self.rex.lock().unwrap()
        }
    }

    fn serve(listener: &UnixListener, rex: &Arc<Mutex<Rex>>) {
        for stream in listener.incoming() {
            serve_connection(stream.unwrap(), rex);
        }
    }

    struct AcceptRexProtocol;

    impl Callback for AcceptRexProtocol {
        fn on_request(
            self,
            _request: &Request,
            mut response: Response,
        ) -> Result<Response, ErrorResponse> {
            response
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(PROTOCOL));
            Ok(response)
        }
    }

    fn serve_connection(stream: UnixStream, rex: &Arc<Mutex<Rex>>) {
        let Ok(mut socket) = tungstenite::accept_hdr(stream, AcceptRexProtocol) else {
            panic!("the WebSocket handshake failed");
        };
        let hello: Value = serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(hello["type"], "hello");
        let welcome =
            json!({"type": "welcome", "protocol": PROTOCOL, "api_revision": API_REVISION});
        socket.send(Message::text(welcome.to_string())).unwrap();
        socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        rex.lock().unwrap().socket = Some(socket);
        loop {
            let mut rex = rex.lock().unwrap();
            let read = rex.socket.as_mut().unwrap().read();
            let request: Value = match read {
                Ok(Message::Text(text)) => serde_json::from_str(&text).unwrap(),
                Err(tungstenite::Error::Io(error)) if waiting(&error) => {
                    drop(rex);
                    thread::yield_now();
                    continue;
                }
                Ok(_) | Err(_) => return,
            };
            let method = request["method"].as_str().unwrap_or_default();
            let answer = match rex.answer(method, &request["payload"]) {
                Ok(payload) => {
                    json!({"type": "response", "id": request["id"], "seq": 0, "payload": payload})
                }
                Err((code, message)) => json!({
                    "type": "response",
                    "id": request["id"],
                    "seq": 0,
                    "error": {"code": code, "message": message, "status": 400},
                }),
            };
            let socket = rex.socket.as_mut().unwrap();
            socket.send(Message::text(answer.to_string())).unwrap();
        }
    }

    #[test]
    fn rex_socket_addresses_are_decoded() {
        assert_eq!(
            socket_path("unix:///Users/a/Library/Application%20Support/rex/server.sock"),
            Some(PathBuf::from(
                "/Users/a/Library/Application Support/rex/server.sock"
            ))
        );
        assert_eq!(socket_path("tcp://127.0.0.1:7000"), None);
        assert_eq!(socket_path("unix:///tmp/bad%2"), None);
        assert_eq!(socket_path("unix:///tmp/bad%+1"), None);
    }

    #[test]
    fn labels_take_the_form_rex_reports_back() {
        assert_eq!(
            label("  ⠋ Busy | Working | app  "),
            "⠋ Busy | Working | app"
        );
        assert_eq!(label("one\u{85}two\u{1b}"), "onetwo");
        let long = format!("{} tail", "⠋".repeat(LABEL_LIMIT - 1));
        assert_eq!(label(&long), "⠋".repeat(LABEL_LIMIT - 1));
    }

    #[test]
    fn a_session_reporting_its_own_status_leaves_the_spinner_to_rex() {
        let fake = Fake::start();
        let mut connection = fake.connect().unwrap();
        assert_eq!(connection.spinner().unwrap(), Spinner::EverySecond);
        fake.rex().status_owner = Some(std::process::id() + 1);
        assert_eq!(connection.spinner().unwrap(), Spinner::EverySecond);
        fake.rex().status_owner = Some(std::process::id());
        assert_eq!(connection.spinner().unwrap(), Spinner::ByTerminal);
    }

    #[test]
    fn an_unchanged_title_sends_nothing_to_rex() {
        let fake = Fake::start();
        let mut connection = fake.connect().unwrap();
        for _ in 0..5 {
            connection.write("⠋ Working | app").unwrap();
        }
        assert_eq!(fake.rex().relabels, 1);
    }

    #[test]
    fn a_title_labels_the_window_and_the_end_restores_its_name() {
        let fake = Fake::start();
        let mut connection = fake.connect().unwrap();
        connection.write("⠋ Busy | Working | app").unwrap();
        assert_eq!(fake.rex().label(OURS), "⠋ Busy | Working | app");
        connection.write("⠙ Busy | Working | app").unwrap();
        assert_eq!(fake.rex().label(OURS), "⠙ Busy | Working | app");
        connection.write("").unwrap();
        assert_eq!(fake.rex().label(OURS), "work");
        assert!(!fake.original.exists());
    }

    #[test]
    fn a_restarted_daemon_restores_the_name_from_before_the_first_title() {
        let fake = Fake::start();
        let mut first = fake.connect().unwrap();
        first.write("✳ Ready | app").unwrap();
        drop(first);
        let mut second = fake.connect().unwrap();
        second.write("⠋ Working | app").unwrap();
        second.write("").unwrap();
        assert_eq!(fake.rex().label(OURS), "work");
    }

    #[test]
    fn spinner_frames_in_other_tabs_do_not_reread_the_layout() {
        let fake = Fake::start();
        let mut connection = fake.connect().unwrap();
        connection.write("✳ Ready | app").unwrap();
        let views = fake.rex().views;
        for frame in ["⠋", "⠙", "⠹", "⠸", "⠼"] {
            fake.rex()
                .relabel(OTHER, &format!("{frame} Working | other"));
            connection.poll().unwrap();
        }
        assert_eq!(fake.rex().views, views);
        fake.rex().split(OURS);
        connection.poll().unwrap();
        assert_eq!(fake.rex().views, views + 1);
    }

    #[test]
    fn a_split_tab_keeps_its_own_name_until_the_split_closes() {
        let fake = Fake::start();
        let mut connection = fake.connect().unwrap();
        connection.write("⠋ Working | app").unwrap();
        fake.rex().split(OURS);
        connection.poll().unwrap();
        assert_eq!(fake.rex().label(OURS), "work");
        connection.write("⠙ Working | app").unwrap();
        assert_eq!(fake.rex().label(OURS), "work");
        fake.rex().unsplit(OURS);
        connection.poll().unwrap();
        assert_eq!(fake.rex().label(OURS), "⠙ Working | app");
    }

    #[test]
    fn a_name_given_while_claude_runs_wins() {
        let fake = Fake::start();
        let mut connection = fake.connect().unwrap();
        connection.write("⠋ Working | app").unwrap();
        fake.rex().relabel(OURS, "mine");
        connection.write("⠙ Working | app").unwrap();
        assert_eq!(fake.rex().label(OURS), "mine");
        connection.write("").unwrap();
        assert_eq!(fake.rex().label(OURS), "mine");
    }

    #[test]
    fn a_name_overwritten_by_a_late_frame_is_put_back() {
        let fake = Fake::start();
        let mut connection = fake.connect().unwrap();
        connection.write("⠋ Working | app").unwrap();
        fake.rex()
            .before_next_request
            .push((OURS, "mine".to_owned()));
        connection.write("⠙ Working | app").unwrap();
        assert_eq!(fake.rex().label(OURS), "⠙ Working | app");
        connection.poll().unwrap();
        assert_eq!(fake.rex().label(OURS), "mine");
        connection.write("⠹ Working | app").unwrap();
        assert_eq!(fake.rex().label(OURS), "mine");
    }

    #[test]
    fn a_session_outside_the_block_opens_no_connection() {
        let mut stranger = Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let fake = Fake::with_child(stranger.id());
        let opened = fake.connect();
        stranger.kill().unwrap();
        stranger.wait().unwrap();
        assert!(
            opened
                .err()
                .is_some_and(|error| error.is::<TerminalDetached>())
        );
    }
}
