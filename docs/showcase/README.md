# README showcase

The screenshots in `docs/images/` are captured by driving LightCraft over its control channel.

```sh
# public-domain set (downloaded into the gitignored corpus/, see assets/ATTRIBUTION.md for sources)
cargo run --release -p lightcraft -- --memory --control 7980 \
  --control-token-file ~/.config/lightcraft/control.token corpus/images/pd &
LIGHTCRAFT_CONTROL_TOKEN_FILE=~/.config/lightcraft/control.token \
  python3 docs/showcase/run.py docs/showcase/pd-edits.jsonl

# procedural demo library
cargo run --release -p lightcraft -- --memory --control 7980 \
  --control-token-file ~/.config/lightcraft/control.token &
LIGHTCRAFT_CONTROL_TOKEN_FILE=~/.config/lightcraft/control.token \
  python3 docs/showcase/run.py docs/showcase/demo.jsonl

# compress for the repo (macOS)
for f in docs/images/*.png; do sips -s format jpeg -s formatOptions 82 "$f" --out "${f%.png}.jpg" && rm "$f"; done
```

`run.py` authenticates, then sends each JSON line (`{"method":…,"params":…}` or `{"sleep": seconds}`) to `127.0.0.1:7980`.
