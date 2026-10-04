# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.7.0] - 2026-10-04

### Added
- Claude Desktop bundle: every release now includes `shadowpty.mcpb`, a one-file install with
  no Node.js needed (macOS on Apple Silicon and Linux). It has a working-directory setting
  and falls back to your home folder when started in `/`.
- Claude Code plugin: `claude plugin marketplace add pplanel/ShadowPTY`, then
  `claude plugin install shadowpty@shadowpty`, installs the server together with a skill that
  teaches Claude how to drive terminal apps well.
- The same skill installs on its own for other agents with `npx skills add pplanel/ShadowPTY`.
- Every tool now has a readable title and behavior hints (read-only, destructive, idempotent,
  open-world), so clients can show them and decide what to auto-approve.
- The server sends short usage instructions to clients when they connect.
- The server logs one line to stderr saying what the connected client supports
  (protocol, elicitation, the MCP Tasks extension).

### Changed
- **`tui_start` never waits for the person anymore.** A session started without
  `record_path`, `report_path` or `live` starts at once, recording with a report to default
  files (`<command>.cast`, `<command>.report.jsonl`) until the person decides.
  - On Claude Code (protocol 2026-07-28), `tui_end` asks the person whether to keep the files
    and where, then moves or deletes them.
  - On older protocols, the person is asked in the background as the session starts and can
    also turn on the live view; without an answer within 30 seconds, the files are kept.
- **Default capture keeps the files.** When the person declines or closes the form, doesn't
  answer, or the client can't show forms, the recording and report are kept. Clients without
  form support now get these files written for such sessions; before, nothing was written.

### Fixed
- Optional tool parameters no longer declare a `null` type, which some clients (such as
  those using Gemini's OpenAPI subset) rejected and the MCP Inspector warned about.
  Sending `null` still works.
- The server now identifies itself as `shadowpty` with its own version, instead of `rmcp 3.4.0`.
- Asking how to capture sessions no longer blocks other `tui_start` calls, and a client that
  comes back without answering the form isn't shown it again and again.

[0.7.0]: https://github.com/pplanel/ShadowPTY/compare/v0.6.0...v0.7.0
