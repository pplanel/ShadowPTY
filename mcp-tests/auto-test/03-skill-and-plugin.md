# 03 · ShadowPTY skill and Claude Code plugin

**Feature name:** `skill-plugin`
**Commit:** `e0a2360`

## What was implemented

- `skills/shadowpty/SKILL.md` (+ `references/patterns.md`): teaches an agent the start → act → wait → look → end loop, which wait tool to pick, stream vs `screen_mode` expect, signals vs `<CTRL+C>`, `tui_run_script`, handing recordings/reports/links to the person, and pitfalls.
- The repository is a Claude Code plugin marketplace: `.claude-plugin/marketplace.json` lists one plugin, `shadowpty`, rooted at the repository; `.claude-plugin/plugin.json` declares the MCP server (`npx -y @azimovlabs/mcp-shadow-pty`) and the skill is picked up from `skills/`.
- The same `skills/shadowpty/` directory installs with `npx skills add pplanel/ShadowPTY` for other agents.
- README and `docs/reference.md` document both install paths.

## Session

Open Claude Code inside ShadowPTY with the plugin loaded from the working tree (instead of the README's default arguments):

```json
{
  "command": "claude",
  "args": ["--strict-mcp-config", "--plugin-dir", "<repo>"],
  "rows": 43,
  "cols": 155,
  "session_id": "skill-plugin",
  "record_path": "<repo>/mcp-tests/auto-test/skill-plugin-test.cast",
  "report_path": "<repo>/mcp-tests/auto-test/skill-plugin-test.report.jsonl",
  "live": false
}
```

The plugin's MCP server is the published npm package, so step C needs network access and Node.js.

## Steps

### A. The plugin validates

`!cd <repo> && claude plugin validate .` → `tui_expect` for `Validation passed`. The only allowed warning is `No version specified` for `plugins[0]` (left out on purpose so updates follow commits).

### B. Component inventory

`!cd <repo> && claude --plugin-dir . plugin details shadowpty` → expect:

```
ShadowPTY (shadowpty)
…
Component inventory
  Skills (1)  shadowpty
  …
  MCP servers (1)  shadowpty
```

### C. The plugin's server and skill inside Claude Code

1. Type `/mcp`, `<ENTER>`. Assert a server from the `shadowpty` plugin is listed and connected, with `14 tools`. `<ESC>`.
2. Type `/shadowpty` (don't press Enter). `tui_wait_stable`, `tui_read`: the suggestion menu lists the plugin's `shadowpty` skill (shown with the plugin namespace, e.g. `shadowpty:shadowpty`, depending on the Claude Code version). Clear the line with `<CTRL+U>` or `<ESC>`.
3. Behavior check (optional; costs a model call): prompt `Which wait tool should you use to wait for a "Loading" spinner to disappear in a TUI under test? Answer with the tool name only.` Expect `tui_wait_gone`, and the transcript showing the skill was loaded (e.g. a `Skill(shadowpty…)` line), if the version shows skill use.

### D. The same skill for the `skills` CLI

`!cd <repo> && npx -y skills add . --list` → lists exactly one skill, `shadowpty`.

### E. Skill matches the server

1. `!grep -c "tui_" <repo>/skills/shadowpty/SKILL.md` is greater than 0, and every `tui_*` name it mentions exists: `!cd <repo> && grep -o "tui_[a-z_]*" skills/shadowpty/SKILL.md | sort -u` is a subset of the 14 tool names.
2. `!grep -n "tui_end" <repo>/skills/shadowpty/SKILL.md` shows the rule to end every session and to relay where the files went (capture redesign, plan 06).

### F. Docs

`!grep -n "plugin marketplace add\|skills add" <repo>/README.md` shows `claude plugin marketplace add pplanel/ShadowPTY`, `claude plugin install shadowpty@shadowpty` and `npx skills add pplanel/ShadowPTY`.

## Assertions

1. A passes with at most the version warning.
2. B shows 1 skill (`shadowpty`) and 1 MCP server (`shadowpty`).
3. C.1 shows the plugin's server connected with 14 tools; C.2 offers the skill; C.3 (if run) answers `tui_wait_gone`.
4. D lists one skill, `shadowpty`; there's only one `SKILL.md` in the repository (`!cd <repo> && find . -name SKILL.md -not -path "./.claude/*" -not -path "./target/*"`).
5. E: no tool name in the skill that the server doesn't have.
6. F: the README has all three install commands.

## Close

As in the [README](README.md#closing-a-test-session).
