// Enough JSON to read `cargo metadata`, which is the only JSON this tool
// sees. It comes from the local cargo, so it is trusted to be well formed;
// anything else is still refused with the byte offset rather than guessed at.

#[derive(Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> &[Value] {
        match self {
            Value::Array(items) => items,
            _ => &[],
        }
    }

    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }
}

pub fn parse(text: &str) -> Result<Value, String> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
        depth: 0,
    };
    let value = reader.value()?;
    reader.skip_space();
    if reader.at != reader.bytes.len() {
        return Err(reader.error("text after the end of the JSON value"));
    }
    Ok(value)
}

// cargo metadata nests about eight deep; this only stops a runaway input
// from exhausting the stack.
const MAX_DEPTH: usize = 128;

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: usize,
}

impl Reader<'_> {
    fn error(&self, what: &str) -> String {
        format!(
            "cargo metadata output is not valid JSON at byte {}: {what}",
            self.at
        )
    }

    fn skip_space(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.at) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.error("unknown word"))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.skip_space();
        match self.bytes.get(self.at) {
            None => Err(self.error("the input ends where a value should be")),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::String),
            Some(b'[') => self.nested(Self::array),
            Some(b'{') => self.nested(Self::object),
            Some(b'-' | b'0'..=b'9') => Ok(self.number()),
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    fn nested(&mut self, read: fn(&mut Self) -> Result<Value, String>) -> Result<Value, String> {
        if self.depth == MAX_DEPTH {
            return Err(self.error("nested too deep"));
        }
        self.depth += 1;
        let value = read(self);
        self.depth -= 1;
        value
    }

    fn array(&mut self) -> Result<Value, String> {
        self.at += 1;
        let mut items = Vec::new();
        self.skip_space();
        if self.eat(b']') {
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_space();
            if self.eat(b']') {
                return Ok(Value::Array(items));
            }
            if !self.eat(b',') {
                return Err(self.error("expected , or ] in an array"));
            }
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.at += 1;
        let mut fields = Vec::new();
        self.skip_space();
        if self.eat(b'}') {
            return Ok(Value::Object(fields));
        }
        loop {
            self.skip_space();
            if self.bytes.get(self.at) != Some(&b'"') {
                return Err(self.error("expected a quoted key"));
            }
            let key = self.string()?;
            self.skip_space();
            if !self.eat(b':') {
                return Err(self.error("expected : after a key"));
            }
            let value = self.value()?;
            fields.push((key, value));
            self.skip_space();
            if self.eat(b'}') {
                return Ok(Value::Object(fields));
            }
            if !self.eat(b',') {
                return Err(self.error("expected , or } in an object"));
            }
        }
    }

    // Numbers are kept as their text: the tool never does arithmetic on them.
    fn number(&mut self) -> Value {
        let start = self.at;
        while let Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') = self.bytes.get(self.at) {
            self.at += 1;
        }
        Value::Number(String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned())
    }

    fn string(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let start = self.at;
            while let Some(&b) = self.bytes.get(self.at) {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.at += 1;
            }
            // The input came in as &str, and the run stops only at ASCII
            // bytes, so it always ends on a character boundary.
            out.push_str(
                std::str::from_utf8(&self.bytes[start..self.at])
                    .map_err(|_| self.error("broken UTF-8"))?,
            );
            match self.bytes.get(self.at) {
                None => return Err(self.error("a string is not closed")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.at += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(self.error("a control character inside a string")),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), String> {
        let Some(&b) = self.bytes.get(self.at) else {
            return Err(self.error("a string ends inside an escape"));
        };
        self.at += 1;
        match b {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.hex4()?;
                let code = if (0xD800..0xDC00).contains(&first) {
                    if !(self.eat(b'\\') && self.eat(b'u')) {
                        return Err(self.error("a high surrogate without its low half"));
                    }
                    let second = self.hex4()?;
                    if !(0xDC00..0xE000).contains(&second) {
                        return Err(self.error("a high surrogate without its low half"));
                    }
                    0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                } else {
                    first
                };
                out.push(
                    char::from_u32(code)
                        .ok_or_else(|| self.error("an escape that is not a character"))?,
                );
            }
            _ => return Err(self.error("unknown escape")),
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.error("a short \\u escape"))?;
        let text = std::str::from_utf8(digits).map_err(|_| self.error("a broken \\u escape"))?;
        let code = u32::from_str_radix(text, 16).map_err(|_| self.error("a broken \\u escape"))?;
        self.at += 4;
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_shapes_cargo_metadata_uses() {
        let value = parse(
            r#" {"packages": [{"name": "a", "license": null, "authors": [], "n": -1.5e3, "ok": true}],
                "resolve": {"nodes": []}} "#,
        )
        .unwrap();
        let package = &value.get("packages").unwrap().as_array()[0];
        assert_eq!(package.str_field("name"), Some("a"));
        assert_eq!(package.get("license"), Some(&Value::Null));
        assert_eq!(package.str_field("license"), None);
        assert!(package.get("authors").unwrap().as_array().is_empty());
        assert_eq!(package.get("n"), Some(&Value::Number("-1.5e3".into())));
        assert_eq!(package.get("ok"), Some(&Value::Bool(true)));
        assert!(
            value
                .get("resolve")
                .unwrap()
                .get("nodes")
                .unwrap()
                .as_array()
                .is_empty()
        );
    }

    #[test]
    fn decodes_escapes_and_surrogate_pairs() {
        let value = parse(r#""D:\\Code\\a\"b\"\n\u00e9\ud834\udd1e\/""#).unwrap();
        assert_eq!(value.as_str(), Some("D:\\Code\\a\"b\"\n\u{e9}\u{1D11E}/"));
        assert_eq!(
            parse("\"Émile, كتاب\"").unwrap().as_str(),
            Some("Émile, كتاب")
        );
    }

    #[test]
    fn refuses_broken_input_with_where() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "{a: 1}",
            "\"open",
            "\"\\x\"",
            "\"\\ud800\"",
            "\"\\ud800\\u0041\"",
            "\"tab\there\"",
            "nul",
            "[] []",
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains("at byte"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn stops_at_a_depth_limit() {
        let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert!(parse(&deep).unwrap_err().contains("nested too deep"));
        let fine = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(&fine).is_ok());
    }
}
