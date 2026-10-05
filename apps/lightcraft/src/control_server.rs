//! Loopback JSON-lines control server: one request per line, one reply per line.
//!
//! The first line on every connection must be `{"method":"auth","params":{"token":…}}`.
//! Nothing is dispatched before that succeeds. This is the transport `lightcraft-cli mcp --connect` wraps.

use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use lightcraft_mcp::{ConnectionLimiter, MAX_CONNECTIONS, configure_stream, serve_authenticated, write_reply};
use lightcraft_ui_egui::ControlRequest;
use serde_json::{Value, json};

pub fn start(port: u16, token: String, ctx: egui::Context) -> Receiver<ControlRequest> {
    let (tx, rx) = channel::<ControlRequest>();
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lightcraft: control server failed to bind 127.0.0.1:{port}: {e}");
            return rx;
        }
    };
    match listener.local_addr() {
        Ok(addr) if addr.ip().is_loopback() => {}
        Ok(addr) => {
            eprintln!("lightcraft: control server refused non-loopback address {addr}");
            return rx;
        }
        Err(e) => {
            eprintln!("lightcraft: control server has no local address: {e}");
            return rx;
        }
    }
    eprintln!("lightcraft: control server listening on 127.0.0.1:{port}");
    let token = Arc::new(token);
    if let Err(e) = std::thread::Builder::new().name("lightcraft-control-listen".into()).spawn(move || {
        let limiter = ConnectionLimiter::new(MAX_CONNECTIONS);
        for mut stream in listener.incoming().flatten() {
            let Some(permit) = limiter.try_acquire() else {
                let _ = configure_stream(&stream);
                let _ = write_reply(&mut stream, &json!({"id": null, "ok": false, "error": "connection limit reached"}));
                let _ = stream.flush();
                continue;
            };
            let tx = tx.clone();
            let ctx = ctx.clone();
            let token = Arc::clone(&token);
            if let Err(e) = std::thread::Builder::new().name("lightcraft-control".into()).spawn(move || {
                let _permit = permit;
                serve_authenticated(stream, &token, |method, params| dispatch(&tx, &ctx, method, params));
            }) {
                eprintln!("lightcraft: control connection thread failed to start: {e}");
            }
        }
    }) {
        eprintln!("lightcraft: control server failed to start: {e}");
    }
    rx
}

fn dispatch(tx: &std::sync::mpsc::Sender<ControlRequest>, ctx: &egui::Context, method: String, params: Value) -> Value {
    let (req, rrx) = ControlRequest::new(method, params);
    if tx.send(req).is_err() {
        return json!({"ok": false, "error": "control channel closed"});
    }
    ctx.request_repaint();
    rrx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| json!({"ok": false, "error": "timeout"}))
}
