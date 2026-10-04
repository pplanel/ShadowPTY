# ShadowPTY quick reference

## Key tokens (`tui_input`)

| Kind | Tokens |
| :--- | :--- |
| Keys | `<ENTER>` `<RETURN>` `<ESC>` `<ESCAPE>` `<TAB>` `<SPACE>` `<BACKSPACE>` `<DELETE>` |
| Navigation | `<UP>` `<DOWN>` `<LEFT>` `<RIGHT>` `<HOME>` `<END>` `<PAGEUP>` `<PAGEDOWN>` |
| Function keys | `<F1>` … `<F12>` |
| Modifiers | `<CTRL+C>` / `<C-C>`, `<ALT+X>` / `<M-X>` |

Anything outside a token is typed as-is: `"echo hi<ENTER>"`.

## Pattern syntax (`tui_expect`, `tui_wait_gone`, `tui_run_script`)

| `syntax` | Meaning | Example |
| :--- | :--- | :--- |
| `"literal"` (default) | The exact text | `"Permission denied"` |
| `"regex"` | Rust regular expression | `"Build (succeeded\|failed)"` |
| `"glob"` | Shell-style wildcard, matched anywhere in the text | `"Error:*"`, `"*[Ee]rror*"` |

Globs: `*` (any characters) and `?` (one character) stay **within a line**; `[abc]`, `[a-z]`, `[!abc]` match a set; `\` escapes. `is_regex: true` is the older form of `syntax: "regex"`.

With `patterns`, the match that starts earliest wins; ties go to the one listed first. Only output up to that match is consumed.

## Screen tags (`tui_read`)

One line per row; trailing blanks and empty rows are trimmed. Default colors carry no tag.

| Tag | Meaning |
| :--- | :--- |
| `<fg:NAME>`, `<bg:NAME>` | ANSI colors: `black` `red` `green` `yellow` `blue` `magenta` `cyan` `white`, and `bright-*` |
| `<fg:idx:N>`, `<bg:idx:N>` | 256-color palette index 16–255 |
| `<fg:#rrggbb>`, `<bg:#rrggbb>` | Truecolor |
| `<bold>` `<dim>` `<italic>` `<strikethrough>` `<hidden>` `<inverse>` | Attributes |
| `<underline>`, `<underline:double\|curly\|dotted\|dashed>` | Underlines |

A selected menu item usually shows as `<inverse>` or a `<bg:...>` highlight; check for it to verify selection.

## Defaults and limits

| | Default |
| :--- | :--- |
| Terminal size | 24 rows × 80 cols |
| `tui_expect` / `tui_wait_gone` / `tui_wait_exit` timeout | 10 s |
| `tui_wait_stable` | quiet 100 ms, max 3 s |
| `tui_run_script` per command | 30 s, prompt `"$"` |
| Any wait | capped at 120 s |
| Screenshot | PNG, cursor shown, scale 1 (or 2) |

## Signals (`tui_signal`)

`INT`, `TERM`, `HUP`, `QUIT`, `KILL`, `TSTP`, `STOP`, `CONT`, `USR1`, `USR2`, `WINCH`, `ALRM`, `PIPE`, `TTIN`, `TTOU`, and crash signals `ABRT`, `SEGV`, `BUS`, `FPE`, `TRAP`. Case-insensitive, `SIG` prefix optional. A process outside a job-control shell ignores `TSTP` unless it handles it; send `STOP` to pause it regardless.
