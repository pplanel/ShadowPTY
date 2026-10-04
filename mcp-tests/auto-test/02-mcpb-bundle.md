# 02 · MCPB bundle for Claude Desktop

**Feature name:** `mcpb-bundle`
**Commit:** `092303d`

## What was implemented

- `mcpb/manifest.json` (manifest 0.4, `server.type: binary`, platforms `darwin` and `linux`, the 14 tools, a `workdir` folder setting defaulting to `${HOME}`).
- `mcpb/server/shadowpty`: a `sh` launcher that picks `bin/shadowpty-<target>` from `uname` (Apple Silicon macOS, Linux x86_64, Linux aarch64), changes into the working directory, and falls back to `$HOME` when started in `/` or when `workdir` arrives unsubstituted (`${user_config.workdir}`).
- `scripts/pack-mcpb.sh <binaries-dir> <version>`: stages the bundle, validates the manifest and writes `dist/shadowpty.mcpb`. Targets missing from the directory are left out.
- `.github/workflows/release.yml`: an `mcpb` job attaches `shadowpty.mcpb` to each GitHub release.
- A unit test keeps the manifest's tool list equal to the server's.

## Session

Open Claude Code as in the [README](README.md#opening-a-test-session) with `<feature>` = `mcpb-bundle`. All checks below run as `!` commands in that session. `<dir>` is `<repo>/mcp-tests/auto-test`.

## Steps

### A. Pack

1. `!mkdir -p <dir>/mcpb-bundle-bins && cp <repo>/target/release/shadowpty <dir>/mcpb-bundle-bins/shadowpty-aarch64-apple-darwin` (on Linux x86_64 name it `shadowpty-x86_64-unknown-linux-gnu`).
2. `!cd <repo> && scripts/pack-mcpb.sh <dir>/mcpb-bundle-bins <version from Cargo.toml>`
3. `tui_expect` for `Output:` (`timeout_ms: 120000`; the first run downloads `@anthropic-ai/mcpb`). Expect `pack-mcpb: no binary for …` lines for the two missing targets, `name: shadowpty`, the version, and `Output: <repo>/dist/shadowpty.mcpb`.

### B. Unpack it the way a host does

1. `!rm -rf <dir>/mcpb-bundle-unpacked && npx -y @anthropic-ai/mcpb@2.1.2 unpack <repo>/dist/shadowpty.mcpb <dir>/mcpb-bundle-unpacked && ls -lR <dir>/mcpb-bundle-unpacked`
2. Read the listing: `manifest.json`, `icon.png`, `LICENSE-MIT`, `LICENSE-APACHE`, `server/shadowpty` and `server/bin/shadowpty-<target>`, both executable (`-rwxr-xr-x`).

### C. Start it through the launcher, from `/`, with an unsubstituted `workdir`

Run as one `!` line:

```sh
!cd / && { printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' '{"jsonrpc":"2.0","method":"notifications/initialized"}' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tui_start","arguments":{"command":"pwd","live":false}}}'; sleep 1; echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"tui_wait_exit","arguments":{}}}'; sleep 1; } | SHADOWPTY_WORKDIR='${user_config.workdir}' /bin/sh <dir>/mcpb-bundle-unpacked/server/shadowpty 2>/dev/null > <dir>/mcpb-bundle-rpc.jsonl; python3 -c "import json; r={m.get('id'): m for m in map(json.loads, open('<dir>/mcpb-bundle-rpc.jsonl'))}; print(r[1]['result']['serverInfo'], len(r[2]['result']['tools']), 'tools'); print(r[4]['result']['content'][0]['text'])"
```

### D. Working directory setting

Repeat step C with `SHADOWPTY_WORKDIR=<dir>` instead. The `pwd` output must be `<dir>`.

### E. Manifest and tool list stay in sync

`!cd <repo> && cargo test --lib mcpb_manifest_lists_every_tool` passes.

### F. Release job (read-only review)

`!grep -n "mcpb" <repo>/.github/workflows/release.yml` shows a `mcpb` job that runs `scripts/pack-mcpb.sh binaries "${GITHUB_REF_NAME#v}"`, uploads `dist/shadowpty.mcpb`, and a `release` job with `needs: [build, mcpb]`.

### G. Manual, outside ShadowPTY (Claude Desktop)

Not drivable from a terminal; do it by hand once per release:

1. Double-click `dist/shadowpty.mcpb` (or drag it onto Claude Desktop) and install it.
2. In Claude Desktop's settings, the extension shows as **ShadowPTY** with the logo and the **Working directory** setting.
3. Ask Claude Desktop to run `pwd` with ShadowPTY and read the screen: the output is the chosen working directory.

## Assertions

1. Step A produces `dist/shadowpty.mcpb`, and validation passes.
2. Step B shows all six files, with the launcher and binary executable.
3. Step C prints `{'name': 'shadowpty', 'version': '<version>'} 14 tools`, and the `pwd` output is your `$HOME`, not `/`.
4. Step D prints `<dir>`.
5. Step E passes; step F shows the job wiring.
6. Step G (when run) installs and runs in Claude Desktop.

## Close

As in the [README](README.md#closing-a-test-session). Then delete `<dir>/mcpb-bundle-bins` and `<dir>/mcpb-bundle-unpacked`.
