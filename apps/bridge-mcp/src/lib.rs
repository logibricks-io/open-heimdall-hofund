//! Stdio MCP helper used by a long-lived coding-agent subprocess.
//!
//! This is the protocol loop behind the hidden `bridge-mcp-stdio` subcommand
//! of `hofund` and `hofundd`. The helper lists a fixed tool set and, on
//! `tools/call`, POSTs JSON to a loopback callback URL with the HTTP proxy
//! disabled so the request cannot be diverted off the machine.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::Duration;

/// Command name the host prepends to the helper's argv.
pub const SUBCOMMAND: &str = "bridge-mcp-stdio";

const MAX_LINE: usize = 16 * 1024 * 1024;
const MAX_CALLBACK_BODY: u64 = 32 * 1024 * 1024;
const LIST_WAIT: Duration = Duration::from_secs(5);
/// `_meta` key a client uses when it already chose the tool-call id.
const TOOL_USE_ID_META: &str = "claudecode/toolUseId";

/// An error from starting or running the helper.
#[derive(Debug)]
pub struct Error {
    message: String,
}

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

/// Runs the helper on this process's standard input and output until stdin
/// closes.
///
/// `args` are the callback URL or path, the tools JSON path, and optionally a
/// path written once the first `tools/list` has been answered.
///
/// # Errors
///
/// Returns an error when the arguments are wrong, the callback or tools file
/// cannot be read, the tools file is not valid JSON, or reading stdin fails.
pub fn run_stdio(args: &[OsString]) -> Result<(), Error> {
    run(args, io::BufReader::new(io::stdin()), io::stdout())
}

/// Runs the helper on the given stdio pipes.
///
/// # Errors
///
/// Same as [`run_stdio`].
pub fn run(
    args: &[OsString],
    stdin: impl BufRead,
    stdout: impl Write + Send + 'static,
) -> Result<(), Error> {
    if args.len() != 2 && args.len() != 3 {
        return Err(Error::new(
            "bridge MCP helper expects a callback URL and a tools file",
        ));
    }
    let callback_url = resolve_callback(&args[0])?;
    let tools = read_tools(Path::new(&args[1]))?;
    let listed_path = args.get(2).cloned().map(PathBuf::from);
    let listed_once = Arc::new(Once::new());
    let state = Arc::new(Mutex::new(ToolState {
        tools,
        listed: None,
    }));
    let stdout = Arc::new(Mutex::new(stdout));
    let mut stdin = stdin;
    let mut line = Vec::new();
    loop {
        line.clear();
        if !read_line(&mut stdin, &mut line, MAX_LINE)? {
            return Ok(());
        }
        let Ok(req) = serde_json::from_slice::<RpcRequest>(&line) else {
            continue;
        };
        if req.id.is_none() {
            continue;
        }
        let state = Arc::clone(&state);
        let stdout = Arc::clone(&stdout);
        let callback_url = callback_url.clone();
        let listed_path = listed_path.clone();
        let listed_once = Arc::clone(&listed_once);
        thread::spawn(move || {
            let write_listed_file = req.method == "tools/list";
            let reply = dispatch(req, &state, &callback_url, &stdout);
            let mut body = serde_json::Map::new();
            body.insert(
                "jsonrpc".to_owned(),
                serde_json::Value::String("2.0".to_owned()),
            );
            body.insert("id".to_owned(), reply.id);
            if let Some(error) = reply.error {
                body.insert("error".to_owned(), error);
            } else {
                body.insert("result".to_owned(), reply.result);
            }
            respond(&stdout, serde_json::Value::Object(body));
            if write_listed_file && let Some(path) = listed_path.as_deref() {
                listed_once.call_once(|| {
                    let _ = write_listed(path);
                });
            }
            drop(reply.release_listed);
        });
    }
}

struct ToolState {
    tools: Vec<Tool>,
    listed: Option<mpsc::Sender<()>>,
}

struct Reply {
    id: serde_json::Value,
    result: serde_json::Value,
    error: Option<serde_json::Value>,
    release_listed: Option<mpsc::Sender<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Tool {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Value,
}

#[derive(Debug, serde::Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct CallParams {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
    #[serde(default, rename = "_meta")]
    meta: serde_json::Value,
}

#[derive(serde::Serialize)]
struct CallbackRequest {
    tool_call_id: String,
    name: String,
    arguments: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct CallbackResponse {
    #[serde(default)]
    content: serde_json::Value,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    tools: Vec<Tool>,
}

fn dispatch(
    req: RpcRequest,
    state: &Arc<Mutex<ToolState>>,
    callback_url: &str,
    stdout: &Arc<Mutex<impl Write>>,
) -> Reply {
    let id = req.id.expect("notifications are filtered");
    let (result, error, release_listed) = match req.method.as_str() {
        "initialize" => (
            serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "hofund", "version": "1"},
            }),
            None,
            None,
        ),
        "tools/list" => {
            let mut state = state.lock().expect("tools state");
            let tools = state.tools.clone();
            let release_listed = state.listed.take();
            (serde_json::json!({"tools": tools}), None, release_listed)
        }
        "tools/call" => match serde_json::from_value::<CallParams>(req.params) {
            Err(error) => (
                serde_json::Value::Null,
                Some(rpc_error(-32602, error.to_string())),
                None,
            ),
            Ok(params) => {
                let (result, error) = call_tool(params, state, callback_url, stdout);
                (result, error, None)
            }
        },
        _ => (
            serde_json::Value::Null,
            Some(rpc_error(-32601, "method not found")),
            None,
        ),
    };
    Reply {
        id,
        result,
        error,
        release_listed,
    }
}

fn call_tool(
    params: CallParams,
    state: &Arc<Mutex<ToolState>>,
    callback_url: &str,
    stdout: &Arc<Mutex<impl Write>>,
) -> (serde_json::Value, Option<serde_json::Value>) {
    let id = match params
        .meta
        .get(TOOL_USE_ID_META)
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
    {
        Some(id) => id.to_owned(),
        None => match new_call_id() {
            Ok(id) => id,
            Err(_) => {
                return (
                    serde_json::Value::Null,
                    Some(rpc_error(-32000, "cannot allocate a call id")),
                );
            }
        },
    };
    let payload = match serde_json::to_vec(&CallbackRequest {
        tool_call_id: id,
        name: params.name,
        arguments: params.arguments,
    }) {
        Ok(payload) => payload,
        Err(error) => {
            return (
                serde_json::Value::Null,
                Some(rpc_error(-32000, error.to_string())),
            );
        }
    };
    let (status, body) = match post_json(callback_url, &payload) {
        Ok(result) => result,
        Err(_) => {
            return (
                serde_json::Value::Null,
                Some(rpc_error(-32000, "callback request failed")),
            );
        }
    };
    if status != 200 {
        let message = String::from_utf8_lossy(&body).into_owned();
        return (serde_json::Value::Null, Some(rpc_error(-32000, message)));
    }
    let cb: CallbackResponse = match serde_json::from_slice(&body) {
        Ok(cb) => cb,
        Err(error) => {
            return (
                serde_json::Value::Null,
                Some(rpc_error(-32000, error.to_string())),
            );
        }
    };
    if !cb.tools.is_empty() {
        retool(state, cb.tools, stdout);
    }
    (
        serde_json::json!({"content": cb.content, "isError": cb.is_error}),
        None,
    )
}

fn retool(state: &Arc<Mutex<ToolState>>, next: Vec<Tool>, stdout: &Arc<Mutex<impl Write>>) {
    let rx = {
        let mut state = state.lock().expect("tools state");
        if state.tools == next {
            return;
        }
        state.tools = next;
        let (tx, rx) = mpsc::channel();
        state.listed = Some(tx);
        rx
    };
    respond(
        stdout,
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}),
    );
    let _ = rx.recv_timeout(LIST_WAIT);
}

fn respond(stdout: &Arc<Mutex<impl Write>>, value: serde_json::Value) {
    let mut out = stdout.lock().expect("stdout");
    let _ = serde_json::to_writer(&mut *out, &value);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn rpc_error(code: i64, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"code": code, "message": message.into()})
}

fn new_call_id() -> Result<String, Error> {
    let mut bytes = [0_u8; 12];
    getrandom::getrandom(&mut bytes).map_err(|_| Error::new("cannot allocate a call id"))?;
    Ok(format!("call_{}", hex_lower(&bytes)))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn resolve_callback(arg: &OsString) -> Result<String, Error> {
    let Some(text) = arg.to_str() else {
        return read_callback_file(Path::new(arg));
    };
    if text.starts_with("http://") || text.starts_with("https://") {
        return Ok(text.to_owned());
    }
    read_callback_file(Path::new(arg))
}

fn read_callback_file(path: &Path) -> Result<String, Error> {
    let text =
        fs::read_to_string(path).map_err(|_| Error::new("failed to read the callback file"))?;
    let url = text.trim();
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(url.to_owned())
    } else {
        Err(Error::new("callback file did not contain an http URL"))
    }
}

fn read_tools(path: &Path) -> Result<Vec<Tool>, Error> {
    let bytes = fs::read(path).map_err(|_| Error::new("failed to read the tools file"))?;
    serde_json::from_slice(&bytes).map_err(|_| Error::new("tools file is not valid JSON"))
}

fn write_listed(path: &Path) -> io::Result<()> {
    // Create the file at mode 0o600 so a reader cannot observe the default
    // umask (0o644) between `write` and `set_permissions`.
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        drop(file);
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, b"")?;
    }
    Ok(())
}

fn read_line(reader: &mut impl BufRead, buf: &mut Vec<u8>, max: usize) -> Result<bool, Error> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }
        if let Some(at) = available.iter().position(|&b| b == b'\n') {
            if buf.len() + at + 1 > max {
                return Err(Error::new("MCP line is too long"));
            }
            buf.extend_from_slice(&available[..=at]);
            reader.consume(at + 1);
            if buf.ends_with(b"\n") {
                buf.pop();
            }
            if buf.ends_with(b"\r") {
                buf.pop();
            }
            return Ok(true);
        }
        if buf.len() + available.len() > max {
            return Err(Error::new("MCP line is too long"));
        }
        let n = available.len();
        buf.extend_from_slice(available);
        reader.consume(n);
    }
}

struct Target {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<Target, Error> {
    let Some(rest) = url.strip_prefix("http://") else {
        return Err(Error::new("callback URL must be http"));
    };
    let (hostport, path) = match rest.split_once('/') {
        Some((host, path)) => (host, format!("/{path}")),
        None => (rest, "/".to_owned()),
    };
    if hostport.is_empty() {
        return Err(Error::new("callback URL is not a valid http URL"));
    }
    let (host, port) = if let Some(host) = hostport.strip_prefix('[') {
        let Some(end) = host.find(']') else {
            return Err(Error::new("callback URL is not a valid http URL"));
        };
        let addr = host[..end].to_owned();
        let port = match &host[end + 1..] {
            "" => 80,
            rest if let Some(port) = rest.strip_prefix(':') => port
                .parse()
                .map_err(|_| Error::new("callback URL is not a valid http URL"))?,
            _ => return Err(Error::new("callback URL is not a valid http URL")),
        };
        (addr, port)
    } else if let Some((host, port)) = hostport.rsplit_once(':') {
        if host.is_empty() {
            return Err(Error::new("callback URL is not a valid http URL"));
        }
        let port = port
            .parse()
            .map_err(|_| Error::new("callback URL is not a valid http URL"))?;
        (host.to_owned(), port)
    } else {
        (hostport.to_owned(), 80)
    };
    Ok(Target { host, port, path })
}

fn post_json(url: &str, body: &[u8]) -> Result<(u16, Vec<u8>), Error> {
    let target = parse_http_url(url)?;
    let mut stream = TcpStream::connect((target.host.as_str(), target.port))
        .map_err(|_| Error::new("callback request failed"))?;
    let host_header = if target.port == 80 {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let header = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        path = target.path,
        host = host_header,
        len = body.len(),
    );
    stream
        .write_all(header.as_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush())
        .map_err(|_| Error::new("callback request failed"))?;
    let mut reader = stream.take(MAX_CALLBACK_BODY + 65_536);
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .map_err(|_| Error::new("callback request failed"))?;
    parse_http_response(&buf)
}

fn parse_http_response(buf: &[u8]) -> Result<(u16, Vec<u8>), Error> {
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| Error::new("callback request failed"))?;
    let header =
        std::str::from_utf8(&buf[..split]).map_err(|_| Error::new("callback request failed"))?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| Error::new("callback request failed"))?;
    let mut body = buf[split + 4..].to_vec();
    if let Some(length) = header.lines().find_map(|line| {
        line.split_once(':').and_then(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
    }) {
        body.truncate(length.min(MAX_CALLBACK_BODY as usize));
    } else if body.len() as u64 > MAX_CALLBACK_BODY {
        body.truncate(MAX_CALLBACK_BODY as usize);
    }
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{SUBCOMMAND, run};

    struct Helper {
        stdin: io::PipeWriter,
        lines: std::sync::mpsc::Receiver<Value>,
        handle: Option<thread::JoinHandle<Result<(), super::Error>>>,
    }

    impl Helper {
        fn start(args: Vec<OsString>) -> Self {
            let (stdin_r, stdin_w) = io::pipe().expect("stdin pipe");
            let (stdout_r, stdout_w) = io::pipe().expect("stdout pipe");
            let handle = thread::spawn(move || run(&args, BufReader::new(stdin_r), stdout_w));
            let (tx, rx) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let mut reader = BufReader::new(stdout_r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Ok(value) = serde_json::from_str::<Value>(line.trim_end())
                                && tx.send(value).is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
            Self {
                stdin: stdin_w,
                lines: rx,
                handle: Some(handle),
            }
        }

        fn send(&mut self, line: &str) {
            writeln!(self.stdin, "{line}").expect("write stdin");
        }

        fn next(&self) -> Value {
            self.lines
                .recv_timeout(Duration::from_secs(3))
                .expect("a message from the helper")
        }

        fn expect_silent(&self, wait: Duration) {
            match self.lines.recv_timeout(wait) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Ok(value) => panic!("unexpected message from the helper: {value}"),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("helper stdout closed while waiting for silence")
                }
            }
        }

        fn close_stdin(&mut self) {
            let writer = std::mem::replace(&mut self.stdin, io::pipe().expect("dummy").1);
            drop(writer);
        }

        fn join(mut self) -> Result<(), super::Error> {
            self.close_stdin();
            self.handle.take().expect("handle").join().expect("thread")
        }
    }

    impl Drop for Helper {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                let _ = self.stdin.write_all(b"");
                drop(std::mem::replace(
                    &mut self.stdin,
                    io::pipe().expect("dummy").1,
                ));
                let _ = handle.join();
            }
        }
    }

    type PostedCalls = Vec<(String, Vec<u8>)>;

    struct Callback {
        url: String,
        posts: Arc<Mutex<PostedCalls>>,
        _server: thread::JoinHandle<()>,
        // Kept so the listener stays open until the test ends.
        _listener: Arc<TcpListener>,
    }

    fn spawn_callback(status: u16, body: Value) -> Callback {
        spawn_callback_with(move |_path, _body| (status, body.clone()))
    }

    fn spawn_callback_with(
        handler: impl Fn(&str, &[u8]) -> (u16, Value) + Send + Sync + 'static,
    ) -> Callback {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind callback");
        listener.set_nonblocking(false).expect("blocking");
        let addr = listener.local_addr().expect("addr");
        let url = format!("http://{addr}/cb/tok");
        let posts = Arc::new(Mutex::new(Vec::new()));
        let listener = Arc::new(listener);
        let serve = Arc::clone(&listener);
        let recorded = Arc::clone(&posts);
        let server = thread::spawn(move || {
            let handler = handler;
            while let Ok((mut stream, _)) = serve.accept() {
                if let Some((path, body)) = read_http_request(&mut stream) {
                    recorded
                        .lock()
                        .expect("posts")
                        .push((path.clone(), body.clone()));
                    let (status, reply) = handler(&path, &body);
                    let payload = serde_json::to_vec(&reply).expect("reply");
                    write_http_response(&mut stream, status, &payload);
                }
            }
        });
        Callback {
            url,
            posts,
            _server: server,
            _listener: listener,
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
        let mut buf = Vec::new();
        let mut tmp = [0_u8; 4096];
        loop {
            let n = stream.read(&mut tmp).ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(at) = find_header_end(&buf) {
                let header = std::str::from_utf8(&buf[..at]).ok()?;
                let path = header.lines().next()?.split_whitespace().nth(1)?.to_owned();
                let length = content_length(header).unwrap_or(0);
                let mut body = buf[at..].to_vec();
                while body.len() < length {
                    let n = stream.read(&mut tmp).ok()?;
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&tmp[..n]);
                }
                body.truncate(length);
                return Some((path, body));
            }
            if buf.len() > 64 * 1024 {
                return None;
            }
        }
        None
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
    }

    fn content_length(header: &str) -> Option<usize> {
        for line in header.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse().ok();
            }
        }
        None
    }

    fn write_http_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        let reason = if status == 200 { "OK" } else { "Error" };
        let header = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
    }

    fn tools_file(dir: &TempDir, json: &str) -> PathBuf {
        let path = dir.path().join("tools.json");
        std::fs::write(&path, json).expect("tools file");
        path
    }

    fn os(path: &std::path::Path) -> OsString {
        path.as_os_str().to_owned()
    }

    #[test]
    fn rejects_the_wrong_number_of_arguments() {
        let error = run(&[], io::empty(), io::sink()).expect_err("no args");
        assert!(
            error.to_string().contains("callback") && error.to_string().contains("tools"),
            "{error}"
        );
        let error = run(
            &[OsString::from("http://127.0.0.1:9/x")],
            io::empty(),
            io::sink(),
        )
        .expect_err("one arg");
        assert!(error.to_string().contains("tools"), "{error}");
    }

    #[test]
    fn initialize_advertises_list_changed_and_a_hofund_server() {
        let dir = TempDir::new().expect("temp");
        let tools = tools_file(&dir, r#"[{"name":"read","inputSchema":{"type":"object"}}]"#);
        let mut helper = Helper::start(vec![OsString::from("http://127.0.0.1:9/x"), os(&tools)]);
        helper.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        let message = helper.next();
        assert_eq!(message["id"], json!(1));
        let result = &message["result"];
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
        assert_eq!(result["serverInfo"]["name"], "hofund");
        assert_eq!(result["serverInfo"]["version"], "1");
        helper.close_stdin();
        helper.join().expect("helper ends");
    }

    #[test]
    fn tools_list_returns_the_file_and_writes_the_listed_file_afterward() {
        let dir = TempDir::new().expect("temp");
        let tools = tools_file(&dir, r#"[{"name":"read","inputSchema":{"type":"object"}}]"#);
        let listed = dir.path().join("listed");
        let mut helper = Helper::start(vec![
            OsString::from("http://127.0.0.1:9/x"),
            os(&tools),
            os(&listed),
        ]);
        helper.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        helper.next();
        assert!(
            !listed.exists(),
            "listed file written before tools were listed"
        );
        helper.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let list = helper.next();
        assert_eq!(list["id"], json!(2));
        assert_eq!(list["result"]["tools"][0]["name"], "read");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !listed.exists() {
            assert!(Instant::now() < deadline, "listed file was not written");
            thread::sleep(Duration::from_millis(10));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&listed)
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "listed file mode {mode:#o}");
        }
        helper.close_stdin();
        helper.join().expect("helper ends");
    }

    #[test]
    fn new_tools_are_listed_before_the_call_answer() {
        let dir = TempDir::new().expect("temp");
        let tools = tools_file(
            &dir,
            r#"[{"name":"ToolSearch","inputSchema":{"type":"object"}}]"#,
        );
        let callback = spawn_callback(
            200,
            json!({
                "content": [{"type":"text","text":"Tool WebFetch is loaded and can be called now."}],
                "tools": [
                    {"name":"ToolSearch","inputSchema":{"type":"object"}},
                    {"name":"WebFetch","inputSchema":{"type":"object"}}
                ]
            }),
        );
        let callback_path = dir.path().join("callback");
        std::fs::write(&callback_path, format!("{}\n", callback.url)).expect("callback file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&callback_path, std::fs::Permissions::from_mode(0o600))
                .expect("mode");
        }
        let mut helper = Helper::start(vec![os(&callback_path), os(&tools)]);
        helper.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        let caps = helper.next();
        assert_eq!(caps["result"]["capabilities"]["tools"]["listChanged"], true);
        helper.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ToolSearch","arguments":{"query":"select:WebFetch"},"_meta":{"claudecode/toolUseId":"toolu_1"}}}"#,
        );
        let first = helper.next();
        assert_eq!(first["method"], "notifications/tools/list_changed");
        assert!(
            first.get("id").is_none(),
            "notification must not have an id: {first}"
        );
        helper.expect_silent(Duration::from_millis(200));
        helper.send(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
        let list = helper.next();
        assert_eq!(
            list["id"],
            json!(3),
            "expected tools/list answer first, got {list}"
        );
        assert_eq!(list["result"]["tools"].as_array().expect("tools").len(), 2);
        let answer = helper.next();
        assert_eq!(answer["id"], json!(2));
        assert_eq!(
            answer["result"]["content"][0]["text"],
            "Tool WebFetch is loaded and can be called now."
        );
        assert_eq!(answer["result"]["isError"], false);
        let posts = callback.posts.lock().expect("posts");
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, "/cb/tok");
        let posted: Value = serde_json::from_slice(&posts[0].1).expect("posted json");
        assert_eq!(posted["tool_call_id"], "toolu_1");
        assert_eq!(posted["name"], "ToolSearch");
        assert_eq!(posted["arguments"]["query"], "select:WebFetch");
        helper.close_stdin();
        helper.join().expect("helper ends");
    }

    #[test]
    fn tools_call_generates_an_id_when_the_client_omits_one() {
        let dir = TempDir::new().expect("temp");
        let tools = tools_file(&dir, r#"[{"name":"read","inputSchema":{"type":"object"}}]"#);
        let callback = spawn_callback(200, json!({"content":[{"type":"text","text":"ok"}]}));
        let mut helper = Helper::start(vec![OsString::from(&callback.url), os(&tools)]);
        helper.send(
            r#"{"jsonrpc":"2.0","id":"call-1","method":"tools/call","params":{"name":"read","arguments":{}}}"#,
        );
        let answer = helper.next();
        assert_eq!(answer["id"], "call-1");
        assert_eq!(answer["result"]["content"][0]["text"], "ok");
        let posts = callback.posts.lock().expect("posts");
        let posted: Value = serde_json::from_slice(&posts[0].1).expect("posted json");
        let id = posted["tool_call_id"].as_str().expect("id");
        assert!(id.starts_with("call_"), "{id}");
        assert_eq!(id.len(), "call_".len() + 24, "{id}");
        helper.close_stdin();
        helper.join().expect("helper ends");
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let dir = TempDir::new().expect("temp");
        let tools = tools_file(&dir, r#"[]"#);
        let mut helper = Helper::start(vec![OsString::from("http://127.0.0.1:9/x"), os(&tools)]);
        helper.send(r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#);
        let message = helper.next();
        assert_eq!(message["id"], 7);
        assert_eq!(message["error"]["code"], -32601);
        assert!(message.get("result").is_none());
        helper.close_stdin();
        helper.join().expect("helper ends");
    }

    #[test]
    fn notifications_and_invalid_json_receive_no_response() {
        let dir = TempDir::new().expect("temp");
        let tools = tools_file(&dir, r#"[]"#);
        let mut helper = Helper::start(vec![OsString::from("http://127.0.0.1:9/x"), os(&tools)]);
        helper.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        helper.send(r#"not-json"#);
        helper.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        let message = helper.next();
        assert_eq!(message["id"], 1);
        helper.close_stdin();
        helper.join().expect("helper ends");
    }

    #[test]
    fn non_ok_callback_becomes_an_rpc_error_without_echoing_the_url() {
        let dir = TempDir::new().expect("temp");
        let tools = tools_file(&dir, r#"[{"name":"read","inputSchema":{"type":"object"}}]"#);
        let callback = spawn_callback(404, json!("unknown or expired bridge run"));
        let mut helper = Helper::start(vec![OsString::from(&callback.url), os(&tools)]);
        helper.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read","arguments":{}}}"#,
        );
        let message = helper.next();
        assert_eq!(message["error"]["code"], -32000);
        let text = message["error"]["message"].as_str().expect("message");
        assert!(text.contains("unknown or expired bridge run"), "{text}");
        assert!(
            !text.contains("tok") && !text.contains(&callback.url),
            "error echoed the callback: {text}"
        );
        helper.close_stdin();
        helper.join().expect("helper ends");
    }

    #[test]
    fn callback_transport_error_does_not_include_the_url() {
        let dir = TempDir::new().expect("temp");
        let tools = tools_file(&dir, r#"[{"name":"read","inputSchema":{"type":"object"}}]"#);
        let mut helper = Helper::start(vec![
            OsString::from("http://127.0.0.1:9/secret-token"),
            os(&tools),
        ]);
        helper.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read","arguments":{}}}"#,
        );
        let message = helper.next();
        assert_eq!(message["error"]["code"], -32000);
        let text = message["error"]["message"].as_str().expect("message");
        assert!(
            !text.contains("secret-token") && !text.contains("127.0.0.1:9"),
            "error leaked the callback: {text}"
        );
        helper.close_stdin();
        helper.join().expect("helper ends");
    }

    #[test]
    fn tools_call_invalid_params_are_invalid_params() {
        let dir = TempDir::new().expect("temp");
        let tools = tools_file(&dir, r#"[]"#);
        let mut helper = Helper::start(vec![OsString::from("http://127.0.0.1:9/x"), os(&tools)]);
        helper.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":"nope"}"#);
        let message = helper.next();
        assert_eq!(message["error"]["code"], -32602);
        helper.close_stdin();
        helper.join().expect("helper ends");
    }

    #[test]
    fn post_json_roundtrips_against_the_test_server() {
        let callback = spawn_callback(200, json!({"ok": true}));
        match super::post_json(&callback.url, br#"{"n":1}"#) {
            Ok((status, body)) => {
                assert_eq!(status, 200, "body={}", String::from_utf8_lossy(&body));
                assert!(
                    String::from_utf8_lossy(&body).contains("ok"),
                    "{}",
                    String::from_utf8_lossy(&body)
                );
            }
            Err(error) => panic!("post_json failed: {error}"),
        }
        assert_eq!(callback.posts.lock().expect("posts").len(), 1);
    }

    #[test]
    fn subcommand_name_is_the_hidden_argv_prefix() {
        assert_eq!(SUBCOMMAND, "bridge-mcp-stdio");
    }
}
