//! Where MCP tool calls end up: a control-channel method call.

use std::io::{BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::{Value, json};

use crate::control_auth::{self, AUTH_METHOD, LineRead, MAX_RESPONSE_BYTES};

/// Something that answers control-channel methods (`engine.execute`, `engine.commands`,
/// `ui.render`, `ui.screenshot`, `ui.key`, …). See `docs/control-protocol.md`.
pub trait Backend {
    /// Call one method. `Ok` carries the `result`, `Err` the error message.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
    /// True when a real UI is attached (`ui.inspect`, `ui.clickWidget`, `ui.pointer` … work).
    fn has_ui(&self) -> bool;
    /// Short human description ("headless", "remote 127.0.0.1:7980").
    fn describe(&self) -> String;
}

/// A running LightCraft app, reached through its loopback control port.
///
/// `token` is the 64-hex bearer from the desktop app. It is sent again on each new TCP
/// connection and never appears in errors or in [`Self::describe`].
pub struct Remote {
    addr: String,
    token: String,
    conn: Option<(BufReader<TcpStream>, TcpStream)>,
    authed: bool,
    next_id: u64,
}

impl Remote {
    /// Connect to a loopback `addr` (e.g. `127.0.0.1:7980`) and authenticate before returning.
    pub fn connect(addr: &str, token: &str) -> std::io::Result<Self> {
        let mut r = Self::lazy(addr, token)?;
        r.reconnect()?;
        Ok(r)
    }

    /// A remote that connects on first use (the app may start after the MCP server).
    ///
    /// The address must resolve only to loopback, and `token` must be 64 hexadecimal characters.
    pub fn lazy(addr: &str, token: &str) -> std::io::Result<Self> {
        control_auth::ensure_loopback(addr)?;
        let token = control_auth::validate_token(token)
            .map(|()| token.to_ascii_lowercase())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        Ok(Self { addr: addr.to_string(), token, conn: None, authed: false, next_id: 1 })
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    fn reconnect(&mut self) -> std::io::Result<()> {
        self.conn = None;
        self.authed = false;
        control_auth::ensure_loopback(&self.addr)?;
        let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, format!("cannot resolve {}", self.addr));
        for sa in self.addr.to_socket_addrs()? {
            if !sa.ip().is_loopback() {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "control bridge refuses non-loopback hosts"));
            }
            match TcpStream::connect_timeout(&sa, Duration::from_millis(800)) {
                Ok(s) => {
                    s.set_nodelay(true).ok();
                    // The app answers within 60 s (its own timeout); leave headroom.
                    s.set_read_timeout(Some(Duration::from_secs(90))).ok();
                    let read = s.try_clone()?;
                    self.conn = Some((BufReader::new(read), s));
                    return self.authenticate();
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn authenticate(&mut self) -> std::io::Result<()> {
        let line = json!({"id": "auth", "method": AUTH_METHOD, "params": {"token": self.token}}).to_string();
        let reply = self.write_and_read(&line)?;
        let v: Value =
            serde_json::from_str(reply.trim()).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad auth reply: {e}")))?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            self.authed = true;
            Ok(())
        } else {
            self.conn = None;
            self.authed = false;
            let msg = v.get("error").and_then(Value::as_str).unwrap_or("authentication required");
            // A hostile reply must not echo the bearer back into a tool error.
            let msg = if msg.contains(&self.token) { "authentication required" } else { msg };
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg.to_string()))
        }
    }

    fn write_and_read(&mut self, line: &str) -> std::io::Result<String> {
        let Some((reader, writer)) = self.conn.as_mut() else {
            return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "not connected"));
        };
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        let mut reply = String::new();
        let read = control_auth::read_bounded_line(reader, &mut reply, MAX_RESPONSE_BYTES)?;
        match read {
            LineRead::Eof => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "control channel closed")),
            LineRead::TooLong => {
                self.conn = None;
                self.authed = false;
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("response exceeds {MAX_RESPONSE_BYTES} bytes")))
            }
            LineRead::Line => Ok(reply),
        }
    }

    fn roundtrip(&mut self, line: &str) -> std::io::Result<String> {
        if self.conn.is_none() || !self.authed {
            self.reconnect()?;
        }
        self.write_and_read(line)
    }
}

impl Backend for Remote {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        // One retry with a fresh connection when the app has restarted. Auth and protocol
        // failures are not retried: the request may already have been refused, or it may
        // have run and only the reply was rejected.
        let reply = match self.roundtrip(&line) {
            Ok(r) => r,
            Err(e)
                if matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData) =>
            {
                self.conn = None;
                self.authed = false;
                return Err(e.to_string());
            }
            Err(_) => {
                self.conn = None;
                self.authed = false;
                self.roundtrip(&line).map_err(|e| {
                    self.conn = None;
                    self.authed = false;
                    format!(
                        "LightCraft app at {} is not reachable: {e} (start it with `lightcraft --control PORT` and the same control token)",
                        self.addr
                    )
                })?
            }
        };
        let v: Value = serde_json::from_str(reply.trim()).map_err(|e| format!("bad reply from app: {e}"))?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(v.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(v.get("error").and_then(Value::as_str).unwrap_or("unknown error").to_string())
        }
    }

    fn has_ui(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        format!("remote {}", self.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    use crate::control_auth::serve_authenticated;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn listen() -> (String, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        (addr, listener)
    }

    #[test]
    fn bridge_rejects_a_non_loopback_host_and_a_bad_token() {
        let err = Remote::connect("192.0.2.1:9", TOKEN).err().expect("non-loopback address");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(Remote::lazy("192.0.2.1:9", TOKEN).is_err());

        let (addr, listener) = listen();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut dispatched = false;
            serve_authenticated(stream, TOKEN, |_, _| {
                dispatched = true;
                json!({"ok": true, "result": true})
            });
            dispatched
        });
        let err = Remote::connect(&addr, &"ab".repeat(32)).err().expect("bad token");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("authentication required"));
        assert!(!err.to_string().contains(TOKEN));
        assert!(!server.join().unwrap(), "bad token reached method dispatch");
    }

    #[test]
    fn bridge_authenticates_then_calls() {
        let (addr, listener) = listen();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_authenticated(stream, TOKEN, |method, _| json!({"ok": true, "result": method}));
        });
        let mut remote = Remote::connect(&addr, TOKEN).unwrap();
        let result = remote.call("ui.inspect", json!({})).unwrap();
        assert_eq!(result, "ui.inspect");
        assert!(!remote.describe().contains(TOKEN));
        drop(remote);
        server.join().unwrap();
    }
}
