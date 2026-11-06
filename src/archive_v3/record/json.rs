//! The importer's JSON reader: decoded strings, exact number tokens, member
//! order kept only as read (it carries no meaning), duplicate member names
//! refused. It is the importer's own, separate from the gate's walker.

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// The exact number token.
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, name: &str) -> Option<&Json> {
        match self {
            Json::Obj(members) => members.iter().find(|(n, _)| n == name).map(|(_, v)| v),
            _ => None,
        }
    }

    /// A record-relative member path (`message.model`).
    pub fn at(&self, path: &str) -> Option<&Json> {
        path.split('.')
            .try_fold(self, |value, step| value.get(step))
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn str_at(&self, path: &str) -> Option<&str> {
        self.at(path).and_then(Json::str)
    }

    pub fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(elements) => elements,
            _ => &[],
        }
    }

    pub fn is_true(&self, path: &str) -> bool {
        matches!(self.at(path), Some(Json::Bool(true)))
    }

    /// A scalar's spelling: a string as decoded, a number by token.
    pub fn spelling(&self) -> Option<String> {
        match self {
            Json::Null => Some("null".to_owned()),
            Json::Bool(b) => Some(b.to_string()),
            Json::Num(token) => Some(token.clone()),
            Json::Str(text) => Some(text.clone()),
            Json::Arr(_) | Json::Obj(_) => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Json::Null => true,
            Json::Str(text) => text.is_empty(),
            Json::Arr(elements) => elements.is_empty(),
            Json::Obj(members) => members.is_empty(),
            Json::Bool(_) | Json::Num(_) => false,
        }
    }

    pub fn is_scalar(&self) -> bool {
        !matches!(self, Json::Arr(_) | Json::Obj(_))
    }

    /// Order-free equality; numbers by token.
    pub fn same(&self, other: &Json) -> bool {
        match (self, other) {
            (Json::Obj(a), Json::Obj(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(name, value)| other.get(name).is_some_and(|o| value.same(o)))
                    && b.iter().all(|(name, _)| self.get(name).is_some())
            }
            (Json::Arr(a), Json::Arr(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same(y))
            }
            (a, b) => a == b,
        }
    }

    /// A hash of the value: members in name order, numbers by token, so
    /// two values hash alike exactly when [`Json::same`] holds.
    pub fn hash(&self) -> [u8; 32] {
        fn feed(value: &Json, hasher: &mut blake3::Hasher) {
            match value {
                Json::Null => {
                    hasher.update(b"n");
                }
                Json::Bool(b) => {
                    hasher.update(if *b { b"t" } else { b"f" });
                }
                Json::Num(token) => {
                    hasher.update(b"#");
                    hasher.update(&(token.len() as u64).to_le_bytes());
                    hasher.update(token.as_bytes());
                }
                Json::Str(text) => {
                    hasher.update(b"s");
                    hasher.update(&(text.len() as u64).to_le_bytes());
                    hasher.update(text.as_bytes());
                }
                Json::Arr(elements) => {
                    hasher.update(b"[");
                    hasher.update(&(elements.len() as u64).to_le_bytes());
                    for element in elements {
                        feed(element, hasher);
                    }
                }
                Json::Obj(members) => {
                    let mut sorted: Vec<&(String, Json)> = members.iter().collect();
                    sorted.sort_by(|a, b| a.0.cmp(&b.0));
                    hasher.update(b"{");
                    hasher.update(&(sorted.len() as u64).to_le_bytes());
                    for (name, member) in sorted {
                        hasher.update(&(name.len() as u64).to_le_bytes());
                        hasher.update(name.as_bytes());
                        feed(member, hasher);
                    }
                }
            }
        }
        let mut hasher = blake3::Hasher::new();
        feed(self, &mut hasher);
        *hasher.finalize().as_bytes()
    }

    /// JSON text with exact number tokens, for the `json_tree` importer.
    pub fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(token) => out.push_str(token),
            Json::Str(text) => string(text, out),
            Json::Arr(elements) => {
                out.push('[');
                for (at, element) in elements.iter().enumerate() {
                    if at > 0 {
                        out.push(',');
                    }
                    element.write(out);
                }
                out.push(']');
            }
            Json::Obj(members) => {
                out.push('{');
                for (at, (name, value)) in members.iter().enumerate() {
                    if at > 0 {
                        out.push(',');
                    }
                    string(name, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

const MAX_DEPTH: usize = 512;

/// Parse one JSON document; `None` when it is not exactly one valid value.
pub fn parse(bytes: &[u8]) -> Option<Json> {
    let mut reader = Reader {
        bytes,
        at: 0,
        depth: 0,
    };
    let value = reader.value()?;
    reader.space();
    (reader.at == bytes.len()).then_some(value)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: usize,
}

impl Reader<'_> {
    fn space(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.at) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        (self.bytes.get(self.at) == Some(&byte)).then(|| self.at += 1)
    }

    fn value(&mut self) -> Option<Json> {
        self.space();
        match *self.bytes.get(self.at)? {
            b'{' => self.nested(Self::object),
            b'[' => self.nested(Self::array),
            b'"' => self.string().map(Json::Str),
            b't' => self.word(b"true", Json::Bool(true)),
            b'f' => self.word(b"false", Json::Bool(false)),
            b'n' => self.word(b"null", Json::Null),
            b'-' | b'0'..=b'9' => self.number().map(Json::Num),
            _ => None,
        }
    }

    fn nested(&mut self, read: fn(&mut Self) -> Option<Json>) -> Option<Json> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        let value = read(self);
        self.depth -= 1;
        value
    }

    fn word(&mut self, word: &[u8], value: Json) -> Option<Json> {
        self.bytes[self.at..].starts_with(word).then(|| {
            self.at += word.len();
            value
        })
    }

    fn object(&mut self) -> Option<Json> {
        self.eat(b'{')?;
        let mut members: Vec<(String, Json)> = Vec::new();
        let mut names = std::collections::HashSet::new();
        self.space();
        if self.eat(b'}').is_some() {
            return Some(Json::Obj(members));
        }
        loop {
            self.space();
            let name = self.string()?;
            self.space();
            self.eat(b':')?;
            let value = self.value()?;
            if !names.insert(name.clone()) {
                return None;
            }
            members.push((name, value));
            self.space();
            if self.eat(b',').is_some() {
                continue;
            }
            self.eat(b'}')?;
            return Some(Json::Obj(members));
        }
    }

    fn array(&mut self) -> Option<Json> {
        self.eat(b'[')?;
        let mut elements = Vec::new();
        self.space();
        if self.eat(b']').is_some() {
            return Some(Json::Arr(elements));
        }
        loop {
            elements.push(self.value()?);
            self.space();
            if self.eat(b',').is_some() {
                continue;
            }
            self.eat(b']')?;
            return Some(Json::Arr(elements));
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.bytes.get(self.at..self.at + 4)?;
        let mut code = 0;
        for &digit in digits {
            code = code * 16 + (digit as char).to_digit(16)?;
        }
        self.at += 4;
        Some(code)
    }

    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = Vec::new();
        loop {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let escape = *self.bytes.get(self.at)?;
                    self.at += 1;
                    let c = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let high = self.hex4()?;
                            let code = match high {
                                0xD800..=0xDBFF => {
                                    self.eat(b'\\')?;
                                    self.eat(b'u')?;
                                    let low = self.hex4()?;
                                    if !(0xDC00..=0xDFFF).contains(&low) {
                                        return None;
                                    }
                                    0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                                }
                                0xDC00..=0xDFFF => return None,
                                code => code,
                            };
                            char::from_u32(code)?
                        }
                        _ => return None,
                    };
                    let mut buffer = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                }
                0x00..=0x1F => return None,
                byte => out.push(byte),
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while let Some(b'0'..=b'9') = self.bytes.get(self.at) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Option<String> {
        let start = self.at;
        self.eat(b'-');
        match self.bytes.get(self.at)? {
            b'0' => self.at += 1,
            b'1'..=b'9' => {
                self.digits();
            }
            _ => return None,
        }
        if self.eat(b'.').is_some() && self.digits() == 0 {
            return None;
        }
        if let Some(b'e' | b'E') = self.bytes.get(self.at) {
            self.at += 1;
            if let Some(b'+' | b'-') = self.bytes.get(self.at) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return None;
            }
        }
        String::from_utf8(self.bytes[start..self.at].to_vec()).ok()
    }
}
