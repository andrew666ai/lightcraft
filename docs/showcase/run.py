#!/usr/bin/env python3
"""run.py FILE.jsonl — control requests ({"method":..., "params":...} or {"sleep": s}).

The desktop app must be started with --control. Set LIGHTCRAFT_CONTROL_TOKEN or
LIGHTCRAFT_CONTROL_TOKEN_FILE to the same bearer (see SECURITY.md).
"""
import json, os, socket, sys, time

def token():
    if os.environ.get("LIGHTCRAFT_CONTROL_TOKEN"):
        return os.environ["LIGHTCRAFT_CONTROL_TOKEN"].strip()
    path = os.environ.get("LIGHTCRAFT_CONTROL_TOKEN_FILE")
    if not path:
        sys.exit("set LIGHTCRAFT_CONTROL_TOKEN or LIGHTCRAFT_CONTROL_TOKEN_FILE")
    with open(path, encoding="utf-8") as f:
        return f.read().strip()

def roundtrip(sock, payload):
    sock.sendall((json.dumps(payload) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = sock.recv(1 << 20)
        if not chunk:
            break
        buf += chunk
    return json.loads(buf)

def call(method, params, tok):
    sock = socket.create_connection(("127.0.0.1", 7980), timeout=180)
    auth = roundtrip(sock, {"id": "auth", "method": "auth", "params": {"token": tok}})
    if not auth.get("ok"):
        return auth
    return roundtrip(sock, {"id": 1, "method": method, "params": params})

tok = token()
for line in open(sys.argv[1], encoding="utf-8"):
    line = line.strip()
    if not line or line.startswith("#"):
        continue
    req = json.loads(line)
    if "sleep" in req:
        time.sleep(req["sleep"])
        continue
    out = call(req["method"], req.get("params", {}), tok)
    print(req["method"], json.dumps(out)[:160])
