//! Bearer token and budgets for the loopback JSON-lines control channel.
//!
//! Stdio MCP does not use this handshake. TCP control and any bridge that dials it do:
//! the first line must authenticate, and only then are methods dispatched.

use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

/// The first request on every TCP connection must use this method.
pub const AUTH_METHOD: &str = "auth";
/// Maximum encoded JSON request line, including its newline.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;
/// Maximum encoded JSON reply, including its newline.
pub const MAX_RESPONSE_BYTES: usize = 8 << 20;
/// Maximum simultaneously serviced TCP connections per listener.
pub const MAX_CONNECTIONS: usize = 16;
/// Idle read and write timeout on a control connection.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

const TOKEN_BYTES: usize = 32;
const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// Result of reading one bounded line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineRead {
    Eof,
    Line,
    TooLong,
}

/// Read one line, never buffering more than `max_bytes` plus one byte (so a too-long line is visible).
pub fn read_bounded_line(reader: &mut impl BufRead, line: &mut String, max_bytes: usize) -> io::Result<LineRead> {
    line.clear();
    let mut limited = std::io::Read::take(reader, (max_bytes.saturating_add(1)) as u64);
    let n = limited.read_line(line)?;
    if n == 0 {
        Ok(LineRead::Eof)
    } else if n > max_bytes {
        Ok(LineRead::TooLong)
    } else {
        Ok(LineRead::Line)
    }
}

/// Drop bytes until the next newline. Used after [`LineRead::TooLong`] when the stream stays open.
pub fn discard_rest_of_line(reader: &mut impl BufRead) -> io::Result<()> {
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(());
        }
        if let Some(i) = buf.iter().position(|b| *b == b'\n') {
            reader.consume(i.saturating_add(1));
            return Ok(());
        }
        let n = buf.len();
        reader.consume(n);
    }
}

/// Generate a 256-bit bearer token with the operating-system CSPRNG.
pub fn generate_token() -> Result<String, String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    fill_random(&mut bytes)?;
    let mut token = String::with_capacity(TOKEN_HEX_LEN);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        let hi = usize::from(byte >> 4);
        let lo = usize::from(byte & 0x0f);
        let Some(h) = HEX.get(hi).copied() else {
            return Err("cannot encode control token".into());
        };
        let Some(l) = HEX.get(lo).copied() else {
            return Err("cannot encode control token".into());
        };
        token.push(h as char);
        token.push(l as char);
    }
    Ok(token)
}

#[cfg(not(target_arch = "wasm32"))]
fn fill_random(dest: &mut [u8]) -> Result<(), String> {
    getrandom::fill(dest).map_err(|e| format!("cannot generate control token: {e}"))
}

#[cfg(target_arch = "wasm32")]
fn fill_random(_dest: &mut [u8]) -> Result<(), String> {
    Err("control tokens are not generated in the web build".into())
}

/// Accept only the fixed-width hexadecimal form emitted by [`generate_token`].
pub fn validate_token(token: &str) -> Result<(), String> {
    if token.len() != TOKEN_HEX_LEN || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("control token must contain exactly 64 hexadecimal characters".into());
    }
    Ok(())
}

/// Compare fixed-width tokens without leaving early on a mismatching byte.
pub fn token_matches(expected: &str, supplied: &str) -> bool {
    if expected.len() != TOKEN_HEX_LEN || supplied.len() != TOKEN_HEX_LEN {
        return false;
    }
    let mut different = 0u8;
    for (a, b) in expected.bytes().zip(supplied.bytes()) {
        different |= a.to_ascii_lowercase() ^ b.to_ascii_lowercase();
    }
    different == 0
}

/// Validate the first TCP frame. The method is not dispatched either way.
///
/// Failures share one error string so a caller cannot tell a missing handshake from a wrong token.
pub fn authentication_reply(line: &str, expected_token: &str) -> (Value, bool) {
    let req: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => return (json!({"id": null, "ok": false, "error": "authentication required"}), false),
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let supplied = req.get("params").and_then(|p| p.get("token")).and_then(Value::as_str).unwrap_or("");
    let ok = req.get("method").and_then(Value::as_str) == Some(AUTH_METHOD) && token_matches(expected_token, supplied);
    if ok {
        (json!({"id": id, "ok": true, "result": {"authenticated": true}}), true)
    } else {
        (json!({"id": id, "ok": false, "error": "authentication required"}), false)
    }
}

fn read_token_file(path: &Path) -> Result<String, String> {
    let token = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let token = token.trim().to_owned();
    validate_token(&token)?;
    Ok(token.to_ascii_lowercase())
}

fn create_token_file(path: &Path, token: &str) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(file, "{token}").map_err(|e| format!("{}: {e}", path.display()))
}

/// Resolve the desktop listener's token.
///
/// A missing token file is created (mode `0600` on Unix). An existing file is read.
/// With neither a token nor a file, a fresh token is returned for this launch only.
pub fn server_token(supplied: Option<&str>, token_file: Option<&Path>) -> Result<String, String> {
    if supplied.is_some() && token_file.is_some() {
        return Err("use either a control token or a control token file, not both".into());
    }
    if let Some(token) = supplied {
        validate_token(token)?;
        return Ok(token.to_ascii_lowercase());
    }
    let Some(path) = token_file else {
        return generate_token();
    };
    match read_token_file(path) {
        Ok(existing) => Ok(existing),
        Err(_) if !path.exists() => {
            let token = generate_token()?;
            match create_token_file(path, &token) {
                Ok(()) => Ok(token),
                Err(_) if path.exists() => read_token_file(path),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// Resolve the token a bridge sends. Bridges never invent a credential.
pub fn client_token(supplied: Option<&str>, token_file: Option<&Path>) -> Result<String, String> {
    if supplied.is_some() && token_file.is_some() {
        return Err("use either a control token or a control token file, not both".into());
    }
    if let Some(token) = supplied {
        validate_token(token)?;
        return Ok(token.to_ascii_lowercase());
    }
    let Some(path) = token_file else {
        return Err("bridge mode needs --control-token, --control-token-file, LIGHTCRAFT_CONTROL_TOKEN, or LIGHTCRAFT_CONTROL_TOKEN_FILE".into());
    };
    read_token_file(path)
}

/// Command-line values, then `LIGHTCRAFT_CONTROL_TOKEN` / `LIGHTCRAFT_CONTROL_TOKEN_FILE`.
pub fn token_inputs(supplied: Option<String>, token_file: Option<PathBuf>) -> (Option<String>, Option<PathBuf>) {
    let supplied = supplied.or_else(|| std::env::var("LIGHTCRAFT_CONTROL_TOKEN").ok());
    let token_file = token_file.or_else(|| std::env::var_os("LIGHTCRAFT_CONTROL_TOKEN_FILE").map(PathBuf::from));
    (supplied, token_file)
}

/// Refuse any resolved address that is not loopback. Call this before connecting.
pub fn ensure_loopback(addr: &str) -> io::Result<()> {
    let mut any = false;
    for sa in addr.to_socket_addrs()? {
        any = true;
        if !sa.ip().is_loopback() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "control bridge refuses non-loopback hosts"));
        }
    }
    if !any {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("cannot resolve {addr}")));
    }
    Ok(())
}

/// Counts active connections and hands out a permit only while below the maximum.
pub struct ConnectionLimiter {
    active: AtomicUsize,
    max: usize,
}

impl ConnectionLimiter {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Self { active: AtomicUsize::new(0), max })
    }

    pub fn try_acquire(self: &Arc<Self>) -> Option<ConnectionPermit> {
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= self.max {
                return None;
            }
            match self.active.compare_exchange_weak(current, current.saturating_add(1), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(ConnectionPermit { limiter: Arc::clone(self) }),
                Err(actual) => current = actual,
            }
        }
    }
}

pub struct ConnectionPermit {
    limiter: Arc<ConnectionLimiter>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.limiter.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Apply idle and write timeouts before a connection is served.
pub fn configure_stream(stream: &TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))
}

struct LimitedWriter {
    bytes: Vec<u8>,
    maximum: usize,
}

impl Write for LimitedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.maximum.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("response exceeds {} bytes", self.maximum)));
        }
        self.bytes.try_reserve(buf.len()).map_err(|error| io::Error::other(format!("response allocation failed: {error}")))?;
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_with_limit(value: &Value, maximum: usize) -> Result<Vec<u8>, ()> {
    let mut writer = LimitedWriter { bytes: Vec::new(), maximum };
    serde_json::to_writer(&mut writer, value).map_err(|_| ())?;
    Ok(writer.bytes)
}

/// Encode a reply before writing it. An oversized reply becomes one small error line with the same id.
/// The operation may already have completed.
pub fn write_reply(out: &mut impl Write, reply: &Value) -> io::Result<()> {
    let encoded = match encode_with_limit(reply, MAX_RESPONSE_BYTES.saturating_sub(1)) {
        Ok(bytes) => bytes,
        Err(()) => {
            let error = json!({
                "id": reply.get("id").cloned().unwrap_or(Value::Null),
                "ok": false,
                "error": format!("response exceeds {MAX_RESPONSE_BYTES} bytes; operation may have completed"),
            });
            match encode_with_limit(&error, MAX_RESPONSE_BYTES.saturating_sub(1)) {
                Ok(bytes) => bytes,
                Err(()) => b"{\"id\":null,\"ok\":false,\"error\":\"response budget exceeded\"}".to_vec(),
            }
        }
    };
    out.write_all(&encoded)?;
    out.write_all(b"\n")
}

/// Read a control connection. `dispatch` runs only after the bearer token is accepted.
pub fn serve_authenticated<F>(stream: TcpStream, token: &str, mut dispatch: F)
where
    F: FnMut(String, Value) -> Value,
{
    if configure_stream(&stream).is_err() {
        return;
    }
    let Ok(read) = stream.try_clone() else { return };
    let mut reader = std::io::BufReader::new(read);
    let mut out = stream;
    let mut line = String::new();
    let mut authenticated = false;
    loop {
        match read_bounded_line(&mut reader, &mut line, MAX_REQUEST_BYTES) {
            Ok(LineRead::Eof) | Err(_) => break,
            Ok(LineRead::TooLong) => {
                let reply = json!({
                    "id": null,
                    "ok": false,
                    "error": format!("request exceeds {MAX_REQUEST_BYTES} bytes"),
                });
                let _ = write_reply(&mut out, &reply);
                let _ = out.flush();
                break;
            }
            Ok(LineRead::Line) if line.trim().is_empty() => continue,
            Ok(LineRead::Line) => {}
        }
        if !authenticated {
            let (reply, ok) = authentication_reply(&line, token);
            authenticated = ok;
            if write_reply(&mut out, &reply).is_err() || out.flush().is_err() || !authenticated {
                break;
            }
            continue;
        }
        let reply = match serde_json::from_str::<Value>(line.trim()) {
            Ok(msg) => {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                let mut r = dispatch(method, params);
                if let Some(o) = r.as_object_mut() {
                    o.insert("id".into(), id);
                }
                r
            }
            Err(e) => json!({"ok": false, "error": format!("bad JSON: {e}")}),
        };
        if write_reply(&mut out, &reply).is_err() || out.flush().is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn generated_tokens_are_valid_and_distinct() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        validate_token(&a).unwrap();
        assert_ne!(a, b);
        assert!(token_matches(&a, &a));
        assert!(token_matches(&a, &a.to_ascii_uppercase()));
        assert!(!token_matches(&a, &b));
        assert!(!token_matches(&a, "short"));
        assert!(validate_token("zz").is_err());
    }

    #[test]
    fn bounded_reader_rejects_an_oversized_line() {
        let input = format!("{}\n", "x".repeat(MAX_REQUEST_BYTES + 1));
        let mut reader = std::io::Cursor::new(input);
        let mut line = String::new();
        assert_eq!(read_bounded_line(&mut reader, &mut line, MAX_REQUEST_BYTES).unwrap(), LineRead::TooLong);
    }

    #[test]
    fn unauthenticated_and_wrong_token_are_rejected_without_a_result() {
        let (reply, authenticated) = authentication_reply(r#"{"id":1,"method":"ui.inspect","params":{}}"#, TOKEN);
        assert!(!authenticated);
        assert_eq!(reply["error"], "authentication required");
        assert!(reply.get("result").is_none());

        let wrong = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        let line = json!({"id": 2, "method": AUTH_METHOD, "params": {"token": wrong}}).to_string();
        let (reply, authenticated) = authentication_reply(&line, TOKEN);
        assert!(!authenticated);
        assert_eq!(reply["id"], 2);
        assert_eq!(reply["error"], "authentication required");
        assert!(reply.get("result").is_none());

        let line = json!({"id": "auth", "method": AUTH_METHOD, "params": {"token": TOKEN}}).to_string();
        let (reply, authenticated) = authentication_reply(&line, TOKEN);
        assert!(authenticated);
        assert_eq!(reply["result"]["authenticated"], true);
        assert!(!reply.to_string().contains(TOKEN));
    }

    #[test]
    fn tcp_dispatch_waits_for_a_matching_token() {
        let (replies, methods) = exchange(&[json!({"id": 1, "method": "engine.execute", "params": {"command": "app.quit"}}).to_string()]);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["error"], "authentication required");
        assert!(methods.is_empty(), "unauthenticated method was dispatched: {methods:?}");

        let (replies, methods) = exchange(&[json!({"id": 2, "method": "auth", "params": {"token": "00".repeat(32)}}).to_string()]);
        assert_eq!(replies[0]["error"], "authentication required");
        assert!(methods.is_empty(), "bad token dispatched: {methods:?}");

        let (replies, methods) = exchange(&[
            json!({"id": "auth", "method": "auth", "params": {"token": TOKEN}}).to_string(),
            json!({"id": 3, "method": "ui.inspect", "params": {}}).to_string(),
        ]);
        assert_eq!(replies.len(), 2, "{replies:?}");
        assert_eq!(replies[0]["result"]["authenticated"], true);
        assert_eq!(replies[1]["id"], 3);
        assert_eq!(replies[1]["result"], "ui.inspect");
        assert_eq!(methods, vec!["ui.inspect".to_string()]);
    }

    #[test]
    fn oversized_request_is_rejected_before_dispatch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut called = false;
            serve_authenticated(stream, TOKEN, |_, _| {
                called = true;
                json!({"ok": true})
            });
            called
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.write_all(&vec![b'x'; MAX_REQUEST_BYTES + 8]).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("request exceeds"));
        assert!(!server.join().unwrap(), "oversized line was dispatched");
    }

    #[test]
    fn connection_limiter_releases_capacity() {
        let limiter = ConnectionLimiter::new(1);
        let permit = limiter.try_acquire().unwrap();
        assert!(limiter.try_acquire().is_none());
        drop(permit);
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn token_file_round_trips_between_server_and_client() {
        let path = std::env::temp_dir().join(format!("lightcraft-control-token-{}-{}.txt", std::process::id(), generate_token().unwrap()));
        let server = server_token(None, Some(&path)).unwrap();
        let client = client_token(None, Some(&path)).unwrap();
        assert_eq!(server, client);
        assert!(token_matches(&server, &client));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn loopback_gate_rejects_routable_addresses() {
        assert!(ensure_loopback("127.0.0.1:9").is_ok());
        assert!(ensure_loopback("[::1]:9").is_ok());
        let err = ensure_loopback("192.0.2.1:9").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn oversized_reply_is_one_error_line_with_the_same_id() {
        let reply = json!({"id": 7, "ok": true, "result": "x".repeat(MAX_RESPONSE_BYTES)});
        let mut out = Vec::new();
        write_reply(&mut out, &reply).unwrap();
        assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 1);
        let error: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(error["id"], 7);
        assert_eq!(error["ok"], false);
        assert!(error["error"].as_str().unwrap().contains("operation may have completed"));
    }

    /// Write `lines` to a throwaway listener and collect replies plus dispatched method names.
    fn exchange(lines: &[String]) -> (Vec<Value>, Vec<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_authenticated(stream, TOKEN, |method, _| {
                tx.send(method.clone()).unwrap();
                json!({"ok": true, "result": method})
            });
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        for line in lines {
            writeln!(stream, "{line}").unwrap();
        }
        stream.flush().unwrap();
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut replies = Vec::new();
        let mut buf = String::new();
        while reader.read_line(&mut buf).unwrap_or(0) > 0 {
            if let Ok(v) = serde_json::from_str::<Value>(buf.trim()) {
                replies.push(v);
            }
            buf.clear();
        }
        server.join().unwrap();
        (replies, rx.try_iter().collect())
    }
}
