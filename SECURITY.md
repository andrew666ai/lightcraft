# Security

LightCraft is a local photo library for personal use. It does not listen on the network unless you turn the control channel on, and that channel stays on loopback.

## Reporting a vulnerability

Give the maintainers a chance to look before you publish exploit details or a weaponized file. This repository has no private security inbox. Contact the repository owner, keep the public note short and non-exploitable, and agree a private way to share samples. Do not open a public issue with a working exploit, a secret, or a malicious attachment.

## Desktop use

Starting the app without `--control` does not open a port. Links the desktop app opens must use `https://`; `http`, `file` and other schemes are refused.

## Control channel

`--control PORT` (or `LIGHTCRAFT_CONTROL_PORT`) binds **127.0.0.1** only. The first line on every connection must be a bearer-token handshake. No method runs before it succeeds:

```json
{"id": "auth", "method": "auth", "params": {"token": "<64 hexadecimal characters>"}}
```

A wrong token and a missing handshake both reply `authentication required` and close the connection. The token is 256 bits, from the operating system CSPRNG.

Supply it with `--control-token-file PATH` or `LIGHTCRAFT_CONTROL_TOKEN_FILE` (a missing file is created, mode `0600` on Unix). `--control-token` and `LIGHTCRAFT_CONTROL_TOKEN` also work; prefer a file so the token is not in the process list or the shell history. If you pass neither, LightCraft prints a one-shot token to stderr for that launch. Do not commit tokens or put them in logs.

Budgets, per listener: 16 active connections, 1 MiB request lines, 8 MiB replies. Over the cap the reply is `connection limit reached`, `request exceeds … bytes`, or `response exceeds … bytes`. There is no per-tool capability split: a client that has the token can call the whole control surface.

Do not tunnel or proxy this unencrypted protocol off the machine.

## MCP

Prefer stdio: `lightcraft-cli mcp` (headless) speaks JSON-RPC on standard input and does not open a port. Request lines are still capped at 1 MiB.

`lightcraft-cli mcp --connect` and `lightcraft-cli run --connect` bridge to the desktop control port. They use the same token, refuse any non-loopback address, and send `auth` before any tool call. Pass the token with the same flags or environment variables. Headless MCP does not need a token.

## What this does not do

No capability model, session expiry, audit log, or workspace jail. An authenticated control client is trusted with the library the app has open. Loopback TCP is not encrypted. The web build does not expose this control port.
