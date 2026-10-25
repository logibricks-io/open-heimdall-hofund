//! Free-form input of a custom tool that was offered as a function.
//!
//! The model writes `{"input":"..."}`. The client wants that string, unescaped,
//! not the JSON wrapper. Streaming reads the string as the arguments arrive.

/// The free-form input inside `{"input":"..."}`, a lone JSON string, or the
/// original text when it is neither.
#[must_use]
pub(crate) fn custom_input_text(arguments: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) {
        if let Some(input) = value.get("input").and_then(serde_json::Value::as_str) {
            return input.to_owned();
        }
        if let serde_json::Value::String(text) = value {
            return text;
        }
    }
    arguments.to_owned()
}

/// Reads `{"input":"..."}` (or a lone JSON string) as argument bytes arrive.
#[derive(Debug, Default)]
pub(crate) struct CustomStream {
    pend: Vec<u8>,
    depth: i32,
    in_str: bool,
    esc: bool,
    key: Vec<u8>,
    in_key: bool,
    want: bool,
    last: String,
    value: bool,
    inside: bool,
    over: bool,
}

impl CustomStream {
    /// Returns the newly completed unescaped input, if any.
    pub(crate) fn feed(&mut self, piece: &str) -> String {
        if self.over {
            return String::new();
        }
        if self.inside {
            self.pend.extend(piece.as_bytes());
            return self.take();
        }
        let bytes = piece.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if self.in_str {
                if self.in_key {
                    self.key.push(byte);
                }
                if self.esc {
                    self.esc = false;
                } else if byte == b'\\' {
                    self.esc = true;
                } else if byte == b'"' {
                    self.in_str = false;
                    if self.in_key {
                        self.in_key = false;
                        self.last.clear();
                        let mut quoted = vec![b'"'];
                        quoted.extend_from_slice(&self.key);
                        if let Ok(serde_json::Value::String(text)) = serde_json::from_slice(&quoted)
                        {
                            self.last = text;
                        }
                    }
                }
            } else if byte == b'"' && (self.depth == 0 || (self.depth == 1 && self.value)) {
                self.inside = true;
                self.pend.extend_from_slice(&bytes[index + 1..]);
                return self.take();
            } else if byte == b'"' {
                self.in_str = true;
                if self.depth == 1 && self.want {
                    self.in_key = true;
                    self.key.clear();
                    self.want = false;
                }
            } else if byte == b'{' || byte == b'[' {
                self.depth += 1;
                if self.depth == 1 && byte == b'{' {
                    self.want = true;
                }
            } else if byte == b'}' || byte == b']' {
                self.depth -= 1;
                if self.depth <= 0 {
                    self.over = true;
                    return String::new();
                }
            } else if byte == b':' && self.depth == 1 {
                self.value = self.last == "input";
            } else if byte == b',' && self.depth == 1 {
                self.want = true;
                self.value = false;
            } else if byte == b' ' || byte == b'\t' || byte == b'\n' || byte == b'\r' {
            } else if self.depth == 0 {
                self.over = true;
                return String::new();
            } else if self.depth == 1 {
                self.value = false;
            }
            index += 1;
        }
        String::new()
    }

    fn take(&mut self) -> String {
        let mut end = 0;
        let mut closed = false;
        while end < self.pend.len() {
            match self.pend[end] {
                b'"' => {
                    closed = true;
                    break;
                }
                b'\\' => {
                    if end + 1 >= self.pend.len() {
                        break;
                    }
                    if self.pend[end + 1] != b'u' {
                        end += 2;
                        continue;
                    }
                    if end + 6 > self.pend.len() {
                        break;
                    }
                    if !high_surrogate(&self.pend[end + 2..end + 6]) {
                        end += 6;
                        continue;
                    }
                    if end + 8 > self.pend.len() {
                        break;
                    }
                    if self.pend[end + 6] != b'\\' || self.pend[end + 7] != b'u' {
                        end += 6;
                        continue;
                    }
                    if end + 12 > self.pend.len() {
                        break;
                    }
                    end += 12;
                }
                _ => end += 1,
            }
        }
        while end > 0 && !closed && std::str::from_utf8(&self.pend[..end]).is_err() {
            end -= 1;
        }
        let mut quoted = vec![b'"'];
        quoted.extend_from_slice(&self.pend[..end]);
        quoted.push(b'"');
        let Ok(serde_json::Value::String(text)) = serde_json::from_slice(&quoted) else {
            self.over = true;
            return String::new();
        };
        self.pend.drain(..end);
        if closed {
            self.over = true;
            self.pend.clear();
        }
        text
    }
}

fn high_surrogate(hex: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(hex) else {
        return false;
    };
    let Ok(value) = u32::from_str_radix(text, 16) else {
        return false;
    };
    (0xD800..0xDC00).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::{CustomStream, custom_input_text};

    #[test]
    fn a_finished_object_yields_the_input_string() {
        assert_eq!(custom_input_text(r#"{"input":"a\nb"}"#), "a\nb");
        assert_eq!(custom_input_text(r#""plain""#), "plain");
        assert_eq!(custom_input_text("not-json"), "not-json");
    }

    #[test]
    fn chunks_emit_the_unescaped_input() {
        let mut stream = CustomStream::default();
        let mut got = String::new();
        for piece in [r#"{"input":"hel"#, r#"lo\n"#, r#"there"}"#] {
            got.push_str(&stream.feed(piece));
        }
        assert_eq!(got, "hello\nthere");
    }
}
