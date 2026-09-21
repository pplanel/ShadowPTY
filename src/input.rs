//! Key input translation for ShadowPTY.
//!
//! Converts human-readable key tokens (e.g. `<ENTER>`, `<UP>`, `<CTRL+C>`)
//! into the corresponding ANSI escape byte sequences for writing to a pseudo-terminal.

/// Translates a key string with special tokens into raw terminal bytes.
///
/// Supported tokens:
/// - `<ENTER>`, `<RETURN>` -> `\r`
/// - `<ESC>`, `<ESCAPE>` -> `\x1b`
/// - `<TAB>` -> `\t`
/// - `<BACKSPACE>` -> `\x7f`
/// - `<DELETE>` -> `\x1b[3~`
/// - `<UP>` -> `\x1b[A`, `<DOWN>` -> `\x1b[B`, `<RIGHT>` -> `\x1b[C`, `<LEFT>` -> `\x1b[D`
/// - `<HOME>` -> `\x1b[H`, `<END>` -> `\x1b[F`
/// - `<PAGEUP>` -> `\x1b[5~`, `<PAGEDOWN>` -> `\x1b[6~`
/// - `<F1>` to `<F12>` -> standard ANSI escape codes
/// - `<CTRL+X>` or `<C-X>` (where X is a letter or character) -> ASCII control code (1-26)
/// - `<ALT+X>` -> `\x1b` + character
#[must_use]
pub fn parse_input_keys(input: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.chars();

    while let Some(ch) = chars.next() {
        if ch == '<' {
            let mut token = String::new();
            let mut closed = false;

            for next_ch in chars.by_ref() {
                if next_ch == '>' {
                    closed = true;
                    break;
                }
                token.push(next_ch);
            }

            if closed && parse_token(&token, &mut bytes) {
                continue;
            }

            // Not a recognized token or unclosed '<', emit '<' and token literally
            bytes.push(b'<');
            bytes.extend_from_slice(token.as_bytes());
            if closed {
                bytes.push(b'>');
            }
        } else {
            let mut buf = [0u8; 4];
            let encoded = ch.encode_utf8(&mut buf);
            bytes.extend_from_slice(encoded.as_bytes());
        }
    }

    bytes
}

const STATIC_TOKENS: &[(&str, &[u8])] = &[
    ("ENTER", b"\r"),
    ("RETURN", b"\r"),
    ("ESC", b"\x1b"),
    ("ESCAPE", b"\x1b"),
    ("TAB", b"\t"),
    ("BACKSPACE", b"\x7f"),
    ("DELETE", b"\x1b[3~"),
    ("UP", b"\x1b[A"),
    ("DOWN", b"\x1b[B"),
    ("RIGHT", b"\x1b[C"),
    ("LEFT", b"\x1b[D"),
    ("HOME", b"\x1b[H"),
    ("END", b"\x1b[F"),
    ("PAGEUP", b"\x1b[5~"),
    ("PAGE_UP", b"\x1b[5~"),
    ("PAGEDOWN", b"\x1b[6~"),
    ("PAGE_DOWN", b"\x1b[6~"),
    ("SPACE", b" "),
];

const FUNCTION_KEYS: &[(&str, &[u8])] = &[
    ("F1", b"\x1bOP"),
    ("F2", b"\x1bOQ"),
    ("F3", b"\x1bOR"),
    ("F4", b"\x1bOS"),
    ("F5", b"\x1b[15~"),
    ("F6", b"\x1b[17~"),
    ("F7", b"\x1b[18~"),
    ("F8", b"\x1b[19~"),
    ("F9", b"\x1b[20~"),
    ("F10", b"\x1b[21~"),
    ("F11", b"\x1b[23~"),
    ("F12", b"\x1b[24~"),
];

fn parse_token(token: &str, output: &mut Vec<u8>) -> bool {
    let upper = token.to_ascii_uppercase();

    for &(name, bytes) in STATIC_TOKENS {
        if upper == name {
            output.extend_from_slice(bytes);
            return true;
        }
    }

    for &(name, bytes) in FUNCTION_KEYS {
        if upper == name {
            output.extend_from_slice(bytes);
            return true;
        }
    }

    parse_modifier_token(&upper, output)
}

fn parse_modifier_token(token: &str, output: &mut Vec<u8>) -> bool {
    // CTRL+X or C-X
    if let Some(rest) = token.strip_prefix("CTRL+").or_else(|| token.strip_prefix("C-")) {
        if rest.len() == 1 {
            let ch = rest.chars().next().unwrap_or('\0');
            if ch.is_ascii_alphabetic() {
                let code = (ch.to_ascii_uppercase() as u8) - b'@';
                output.push(code);
                return true;
            }
        }
    }

    // ALT+X or A-X or M-X (Meta)
    if let Some(rest) = token
        .strip_prefix("ALT+")
        .or_else(|| token.strip_prefix("A-"))
        .or_else(|| token.strip_prefix("M-"))
    {
        if rest.len() == 1 {
            let ch = rest.chars().next().unwrap_or('\0');
            output.push(0x1b);
            output.push(ch as u8);
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text() {
        let res = parse_input_keys("hello world");
        assert_eq!(res, b"hello world");
    }

    #[test]
    fn test_enter_and_esc() {
        let res = parse_input_keys("<enter><esc>");
        assert_eq!(res, b"\r\x1b");
    }

    #[test]
    fn test_arrow_keys() {
        let res = parse_input_keys("<up><down><left><right>");
        assert_eq!(res, b"\x1b[A\x1b[B\x1b[D\x1b[C");
    }

    #[test]
    fn test_ctrl_characters() {
        assert_eq!(parse_input_keys("<ctrl+c>"), vec![3]);
        assert_eq!(parse_input_keys("<ctrl+a>"), vec![1]);
        assert_eq!(parse_input_keys("<ctrl+z>"), vec![26]);
        assert_eq!(parse_input_keys("<c-d>"), vec![4]);
    }

    #[test]
    fn test_alt_characters() {
        assert_eq!(parse_input_keys("<alt+x>"), vec![0x1b, b'X']);
        assert_eq!(parse_input_keys("<m-a>"), vec![0x1b, b'A']);
    }

    #[test]
    fn test_mixed_sequence() {
        let res = parse_input_keys("ls -l<ENTER>");
        assert_eq!(res, b"ls -l\r");
    }

    #[test]
    fn test_unknown_or_unclosed_token() {
        assert_eq!(parse_input_keys("<unknown>"), b"<unknown>");
        assert_eq!(parse_input_keys("a < b"), b"a < b");
    }
}
