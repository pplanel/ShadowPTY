# Domain Context: ShadowPTY

This glossary defines the core domain concepts for the ShadowPTY headless terminal environment.

## Domain Glossary

### Screen
An immutable snapshot of the visible terminal viewport at a specific point in time. It encapsulates the full rectangular grid of cells, cursor state, and dimensions. The Screen is captured under the emulator lock in sub-millisecond time and provides lock-free text rendering adapters.

### Cell
A single character coordinate on the Screen grid. A Cell maintains its text content (including combining Unicode marks), wide-character spacer indication, original semantic styling (ANSI named colors, 256-color palette indices, RGB truecolor), and fully resolved RGB colors after dynamic terminal color overrides and style pipelines.

### Tagged Text
The semantic string representation of a Screen produced for LLM agents (`tui_read`). It uses lightweight XML-style tags (e.g., `<fg:red><bold>Text</bold></fg>`) to convey visual formatting without confusing LLMs with raw ANSI escape sequences. Trailing empty rows and trailing whitespace are trimmed.

### Plain Text
The unformatted string representation of a Screen where all color and style tags are omitted. Used for pattern matching in screen-mode expectations (`tui_expect`). Trailing row whitespace and empty rows are trimmed.

### Session
An active headless pseudo-terminal (PTY) instance running a child process command. Each Session owns an alacritty terminal emulator, a bounded raw output buffer, a dedicated continuous reader thread, and optional asciicast recording.

### Expectation
A pattern-matching assertion on either unread stream output or the rendered Plain Text of the Screen, evaluated with a configurable timeout.
