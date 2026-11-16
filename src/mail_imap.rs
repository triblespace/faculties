//! Small, synchronous, octet-safe, READ-ONLY IMAP4rev1 transport.
//!
//! Mail only ever observes a mailbox. This module can open a mailbox with
//! `EXAMINE` (read-only by protocol), search it, and fetch message bytes with
//! `BODY.PEEK[]`, which never sets `\Seen`. There is no `SELECT`, `STORE`,
//! `COPY`, `MOVE`, `EXPUNGE`, `DELETE`, `APPEND` or any other mutating
//! command, and there is no generic "send this command" method: the only
//! commands this type can put on the wire are the variants of the private
//! [`Command`] enum, and a test pins that set.
//!
//! Server responses are read octet-exactly. Literals (`{n}` followed by `n`
//! raw octets) are read by length, never by line, so message bodies arrive
//! byte-for-byte. Every line and literal is bounded: a message above
//! [`MAX_MESSAGE_BYTES`] is refused rather than buffered.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, RootCertStore, StreamOwned};

/// The largest single message Mail will fetch. Larger messages are refused
/// (reported, never partially stored); they stay on the server untouched.
pub const MAX_MESSAGE_BYTES: u64 = 64 * 1024 * 1024;

/// The longest CRLF-terminated text chunk accepted from the server. Long
/// `SEARCH` results on large mailboxes are the reason this is generous.
const MAX_LINE_BYTES: u64 = 16 * 1024 * 1024;

/// How many UIDs one `UID FETCH` size probe names. Keeps command lines short
/// for servers that bound them.
const SIZE_PROBE_BATCH: usize = 256;

/// Socket read/write timeout for the implicit-TLS connection.
const IO_TIMEOUT: Duration = Duration::from_secs(120);

/// Failure from the IMAP transport. No variant ever carries a credential or
/// free server text, so even a server that echoes the password cannot put it
/// into an error message.
#[derive(Debug)]
pub enum ImapError {
    /// The underlying stream failed.
    Io(io::Error),
    /// TLS setup or transport failed.
    Tls(rustls::Error),
    /// The configured host is not a valid TLS server name.
    InvalidServerName,
    /// No usable native trust anchor could be loaded.
    NoTrustAnchors,
    /// An argument (credential, mailbox name, search value) contains a byte
    /// that cannot be sent safely (NUL, CR or LF).
    InvalidArgument(&'static str),
    /// The server advertises `LOGINDISABLED`.
    LoginDisabled,
    /// The server answered a command with tagged `NO` or `BAD`. `code` is
    /// the bracketed response code atom (for example `AUTHENTICATIONFAILED`)
    /// when one was given; the human-readable text is deliberately dropped.
    Rejected {
        command: &'static str,
        code: Option<String>,
    },
    /// The server sent `BYE` outside `LOGOUT`.
    Bye,
    /// The server sent a malformed, truncated, or oversized response.
    Protocol(&'static str),
    /// A response literal exceeds [`MAX_MESSAGE_BYTES`].
    TooLarge { size: u64, limit: u64 },
    /// `EXAMINE` completed without a `UIDVALIDITY` response code.
    MissingUidValidity,
}

impl fmt::Display for ImapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "IMAP transport I/O failed: {error}"),
            Self::Tls(error) => write!(f, "IMAP TLS failed: {error}"),
            Self::InvalidServerName => f.write_str("invalid IMAP TLS server name"),
            Self::NoTrustAnchors => f.write_str("no usable native TLS trust anchors found"),
            Self::InvalidArgument(what) => {
                write!(f, "IMAP {what} contains a forbidden NUL, CR or LF byte")
            }
            Self::LoginDisabled => f.write_str("IMAP server advertises LOGINDISABLED"),
            Self::Rejected {
                command,
                code: Some(code),
            } => write!(f, "IMAP server rejected {command} [{code}]"),
            Self::Rejected {
                command,
                code: None,
            } => write!(f, "IMAP server rejected {command}"),
            Self::Bye => f.write_str("IMAP server closed the session (BYE)"),
            Self::Protocol(message) => write!(f, "malformed IMAP response: {message}"),
            Self::TooLarge { size, limit } => write!(
                f,
                "IMAP response literal of {size} bytes exceeds the {limit}-byte message cap"
            ),
            Self::MissingUidValidity => f.write_str("IMAP EXAMINE returned no UIDVALIDITY"),
        }
    }
}

impl std::error::Error for ImapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Tls(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ImapError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rustls::Error> for ImapError {
    fn from(error: rustls::Error) -> Self {
        Self::Tls(error)
    }
}

// ── search criteria ───────────────────────────────────────────────────────

/// A calendar date for IMAP `SINCE`/`BEFORE`, which compare against the
/// server's INTERNALDATE at day granularity, ignoring time and timezone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SearchDate {
    year: u16,
    month: u8,
    day: u8,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

impl SearchDate {
    /// Parse `YYYY-MM-DD` or the IMAP form `D-Mon-YYYY` (for example
    /// `1-Jan-2024`; the month name is case-insensitive).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (year, month, day) = if let [y, m, d] = text.split('-').collect::<Vec<_>>()[..] {
            if y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()) {
                (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
            } else {
                let month = MONTHS
                    .iter()
                    .position(|name| name.eq_ignore_ascii_case(m))?;
                if d.len() != 4 || !d.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                if y.is_empty() || y.len() > 2 || !y.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                (d.parse().ok()?, month as u8 + 1, y.parse().ok()?)
            }
        } else {
            return None;
        };
        let date = Self { year, month, day };
        date.is_valid().then_some(date)
    }

    fn is_valid(&self) -> bool {
        let leap = (self.year % 4 == 0 && self.year % 100 != 0) || self.year % 400 == 0;
        let days = match self.month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return false,
        };
        (1..=9999).contains(&self.year) && (1..=days).contains(&self.day)
    }

    /// RFC 3501 `date-text`: `D-Mon-YYYY`.
    pub fn imap_text(&self) -> String {
        format!(
            "{}-{}-{:04}",
            self.day,
            MONTHS[usize::from(self.month - 1)],
            self.year
        )
    }
}

impl fmt::Display for SearchDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.imap_text())
    }
}

/// The IMAP `SEARCH` keys Mail exposes. All present keys are ANDed. An empty
/// criteria set searches `ALL`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchCriteria {
    /// `FROM <string>`: substring of the From header.
    pub from: Option<String>,
    /// `SUBJECT <string>`: substring of the Subject header.
    pub subject: Option<String>,
    /// `SINCE <date>`: internal date on or after this day.
    pub since: Option<SearchDate>,
    /// `BEFORE <date>`: internal date strictly before this day.
    pub before: Option<SearchDate>,
}

impl SearchCriteria {
    pub fn is_empty(&self) -> bool {
        self.from.is_none()
            && self.subject.is_none()
            && self.since.is_none()
            && self.before.is_none()
    }

    fn needs_utf8(&self) -> bool {
        [&self.from, &self.subject]
            .into_iter()
            .flatten()
            .any(|value| !value.is_ascii())
    }
}

impl fmt::Display for SearchCriteria {
    /// A human-readable rendering for receipts; not the wire form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("ALL");
        }
        let mut parts = Vec::new();
        if let Some(value) = &self.from {
            parts.push(format!("FROM {value:?}"));
        }
        if let Some(value) = &self.subject {
            parts.push(format!("SUBJECT {value:?}"));
        }
        if let Some(value) = &self.since {
            parts.push(format!("SINCE {value}"));
        }
        if let Some(value) = &self.before {
            parts.push(format!("BEFORE {value}"));
        }
        f.write_str(&parts.join(" "))
    }
}

// ── the closed command set ────────────────────────────────────────────────

/// Every command this transport can send. There is deliberately no variant
/// that changes server state, and no variant that carries a free verb.
enum Command<'a> {
    Capability,
    Login {
        username: &'a str,
        password: &'a str,
    },
    List,
    Examine {
        mailbox: &'a str,
    },
    UidSearch {
        criteria: &'a SearchCriteria,
    },
    UidFetchSizes {
        set: &'a str,
    },
    UidFetchBody {
        uid: u32,
    },
    Logout,
}

/// One piece of a serialized command: plain octets, or a synchronizing
/// literal that must wait for the server's `+` continuation.
enum Part {
    Text(Vec<u8>),
    Literal(Vec<u8>),
}

impl Command<'_> {
    /// The command verb, for error messages and the command-set test.
    fn verb(&self) -> &'static str {
        match self {
            Self::Capability => "CAPABILITY",
            Self::Login { .. } => "LOGIN",
            Self::List => "LIST",
            Self::Examine { .. } => "EXAMINE",
            Self::UidSearch { .. } => "UID SEARCH",
            Self::UidFetchSizes { .. } | Self::UidFetchBody { .. } => "UID FETCH",
            Self::Logout => "LOGOUT",
        }
    }

    fn parts(&self) -> Result<Vec<Part>, ImapError> {
        let mut parts = vec![Part::Text(self.verb().as_bytes().to_vec())];
        match self {
            Self::Capability | Self::Logout => {}
            Self::Login { username, password } => {
                push_astring(&mut parts, username.as_bytes(), "credential")?;
                push_astring(&mut parts, password.as_bytes(), "credential")?;
            }
            Self::List => push_text(&mut parts, b" \"\" \"*\""),
            Self::Examine { mailbox } => {
                push_astring(&mut parts, mailbox.as_bytes(), "mailbox name")?
            }
            Self::UidSearch { criteria } => {
                if criteria.needs_utf8() {
                    push_text(&mut parts, b" CHARSET UTF-8");
                }
                if criteria.is_empty() {
                    push_text(&mut parts, b" ALL");
                }
                if let Some(value) = &criteria.from {
                    push_text(&mut parts, b" FROM");
                    push_astring(&mut parts, value.as_bytes(), "search value")?;
                }
                if let Some(value) = &criteria.subject {
                    push_text(&mut parts, b" SUBJECT");
                    push_astring(&mut parts, value.as_bytes(), "search value")?;
                }
                if let Some(value) = &criteria.since {
                    push_text(&mut parts, b" SINCE");
                    push_astring(&mut parts, value.imap_text().as_bytes(), "search date")?;
                }
                if let Some(value) = &criteria.before {
                    push_text(&mut parts, b" BEFORE");
                    push_astring(&mut parts, value.imap_text().as_bytes(), "search date")?;
                }
            }
            Self::UidFetchSizes { set } => {
                if set.is_empty()
                    || !set
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b == b':' || b == b',')
                {
                    return Err(ImapError::InvalidArgument("UID set"));
                }
                push_text(&mut parts, b" ");
                push_text(&mut parts, set.as_bytes());
                push_text(&mut parts, b" (UID RFC822.SIZE)");
            }
            Self::UidFetchBody { uid } => {
                if *uid == 0 {
                    return Err(ImapError::InvalidArgument("UID"));
                }
                // BODY.PEEK[] never sets \Seen, unlike BODY[] or RFC822.
                push_text(&mut parts, format!(" {uid} (UID BODY.PEEK[])").as_bytes());
            }
        }
        Ok(parts)
    }
}

fn push_text(parts: &mut Vec<Part>, text: &[u8]) {
    match parts.last_mut() {
        Some(Part::Text(previous)) => previous.extend_from_slice(text),
        _ => parts.push(Part::Text(text.to_vec())),
    }
}

/// Append ` <astring>`: a quoted string when the value is 7-bit and
/// CR/LF-free, otherwise a synchronizing literal. NUL, CR and LF are refused
/// outright so no argument can smuggle in a second command line.
fn push_astring(parts: &mut Vec<Part>, value: &[u8], what: &'static str) -> Result<(), ImapError> {
    if value.iter().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
        return Err(ImapError::InvalidArgument(what));
    }
    if value.is_ascii() {
        let mut quoted = Vec::with_capacity(value.len() + 3);
        quoted.extend_from_slice(b" \"");
        for &byte in value {
            if matches!(byte, b'"' | b'\\') {
                quoted.push(b'\\');
            }
            quoted.push(byte);
        }
        quoted.push(b'"');
        push_text(parts, &quoted);
    } else {
        push_text(parts, format!(" {{{}}}", value.len()).as_bytes());
        parts.push(Part::Literal(value.to_vec()));
    }
    Ok(())
}

// ── responses ─────────────────────────────────────────────────────────────

/// One logical server response line: text chunks interleaved with the
/// literals the server announced at the end of each chunk.
#[derive(Debug)]
struct Line {
    /// Text of each chunk without its CRLF and without the trailing `{n}`.
    texts: Vec<Vec<u8>>,
    /// `literals[i]` follows `texts[i]`; there is one fewer literal than text.
    literals: Vec<Vec<u8>>,
}

impl Line {
    fn head(&self) -> &[u8] {
        &self.texts[0]
    }

    /// Tokenize the whole logical line into items.
    fn items(&self) -> Result<Vec<Item>, ImapError> {
        let mut tokens = Vec::new();
        for (index, text) in self.texts.iter().enumerate() {
            lex(text, &mut tokens)?;
            if let Some(literal) = self.literals.get(index) {
                tokens.push(Token::String(literal.clone()));
            }
        }
        let mut position = 0;
        let items = parse_items(&tokens, &mut position, false)?;
        Ok(items)
    }
}

#[derive(Debug)]
enum Token {
    Open,
    Close,
    Atom(Vec<u8>),
    String(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Item {
    Atom(Vec<u8>),
    String(Vec<u8>),
    List(Vec<Item>),
}

impl Item {
    fn atom(&self) -> Option<&[u8]> {
        match self {
            Self::Atom(value) => Some(value),
            _ => None,
        }
    }

    fn is_atom(&self, expected: &[u8]) -> bool {
        self.atom()
            .is_some_and(|value| value.eq_ignore_ascii_case(expected))
    }

    fn number(&self) -> Option<u64> {
        let value = self.atom()?;
        if value.is_empty() || value.len() > 20 || !value.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(value).ok()?.parse().ok()
    }

    /// An `astring`/`nstring` value: atom or string; `NIL` reads as `None`.
    fn string(&self) -> Option<&[u8]> {
        match self {
            Self::String(value) => Some(value),
            Self::Atom(value) if !value.eq_ignore_ascii_case(b"NIL") => Some(value),
            _ => None,
        }
    }
}

fn lex(text: &[u8], tokens: &mut Vec<Token>) -> Result<(), ImapError> {
    let mut index = 0;
    while index < text.len() {
        match text[index] {
            b' ' => index += 1,
            b'(' => {
                tokens.push(Token::Open);
                index += 1;
            }
            b')' => {
                tokens.push(Token::Close);
                index += 1;
            }
            b'"' => {
                index += 1;
                let mut value = Vec::new();
                loop {
                    match text.get(index) {
                        None => return Err(ImapError::Protocol("unterminated quoted string")),
                        Some(b'"') => {
                            index += 1;
                            break;
                        }
                        Some(b'\\') => {
                            let escaped = *text
                                .get(index + 1)
                                .ok_or(ImapError::Protocol("dangling quoted-string escape"))?;
                            value.push(escaped);
                            index += 2;
                        }
                        Some(&byte) => {
                            value.push(byte);
                            index += 1;
                        }
                    }
                }
                tokens.push(Token::String(value));
            }
            _ => {
                let start = index;
                while index < text.len() && !matches!(text[index], b' ' | b'(' | b')' | b'"') {
                    index += 1;
                }
                tokens.push(Token::Atom(text[start..index].to_vec()));
            }
        }
    }
    Ok(())
}

fn parse_items(
    tokens: &[Token],
    position: &mut usize,
    nested: bool,
) -> Result<Vec<Item>, ImapError> {
    let mut items = Vec::new();
    while let Some(token) = tokens.get(*position) {
        *position += 1;
        match token {
            Token::Open => items.push(Item::List(parse_items(tokens, position, true)?)),
            Token::Close if nested => return Ok(items),
            Token::Close => return Err(ImapError::Protocol("unbalanced parenthesis")),
            Token::Atom(value) => items.push(Item::Atom(value.clone())),
            Token::String(value) => items.push(Item::String(value.clone())),
        }
    }
    if nested {
        return Err(ImapError::Protocol("unclosed parenthesis"));
    }
    Ok(items)
}

/// Split `status-word [CODE args] text` and return the code's first atom.
fn response_code(rest: &[u8]) -> Option<String> {
    let rest = rest.strip_prefix(b"[")?;
    let end = rest.iter().position(|b| *b == b']')?;
    let code = rest[..end].split(|b| *b == b' ').next()?;
    (!code.is_empty()
        && code.len() <= 64
        && code
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.')))
    .then(|| String::from_utf8_lossy(code).into_owned())
}

/// `[UIDVALIDITY n]` anywhere in an untagged `OK` head.
fn uidvalidity_code(head: &[u8]) -> Option<u32> {
    let rest = head.strip_prefix(b"* ")?;
    let rest = strip_word(rest, b"OK")?;
    let rest = rest.strip_prefix(b"[")?;
    let rest = strip_word(rest, b"UIDVALIDITY")?;
    let end = rest.iter().position(|b| *b == b']')?;
    let value = &rest[..end];
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value: u32 = std::str::from_utf8(value).ok()?.parse().ok()?;
    (value != 0).then_some(value)
}

/// Whether `head` is `* WORD ...`. Only such lines are tokenized, so free
/// human-readable text elsewhere (an unbalanced `:)` in an `OK` greeting, say)
/// can never fail a command.
fn is_untagged(head: &[u8], word: &[u8]) -> bool {
    head.strip_prefix(b"* ")
        .and_then(|rest| strip_word(rest, word))
        .is_some()
}

/// `* <n> WORD ...`: the number and the word (`EXISTS`, `FETCH`, ...).
fn numbered(head: &[u8]) -> Option<(u32, &[u8])> {
    let rest = head.strip_prefix(b"* ")?;
    let space = rest.iter().position(|b| *b == b' ')?;
    let (digits, rest) = (&rest[..space], &rest[space + 1..]);
    if digits.is_empty() || digits.len() > 10 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let number = std::str::from_utf8(digits).ok()?.parse().ok()?;
    let end = rest.iter().position(|b| *b == b' ').unwrap_or(rest.len());
    Some((number, &rest[..end]))
}

/// Strip a case-insensitive word followed by a space (or end of input).
fn strip_word<'a>(text: &'a [u8], word: &[u8]) -> Option<&'a [u8]> {
    if text.len() < word.len() || !text[..word.len()].eq_ignore_ascii_case(word) {
        return None;
    }
    let rest = &text[word.len()..];
    match rest {
        [] => Some(rest),
        [b' ', tail @ ..] => Some(tail),
        _ => None,
    }
}

/// Compress ascending UIDs into an IMAP sequence set (`1:3,7,9:10`).
fn uid_set(uids: &[u32]) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < uids.len() {
        let start = uids[index];
        let mut end = start;
        while index + 1 < uids.len() && uids[index + 1] == end.wrapping_add(1) {
            index += 1;
            end = uids[index];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}:{end}"));
        }
        index += 1;
    }
    out
}

/// The state of a mailbox opened with `EXAMINE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Examined {
    pub uidvalidity: u32,
    pub exists: Option<u32>,
}

// ── session ───────────────────────────────────────────────────────────────

/// A synchronous, read-only IMAP session over any bidirectional byte stream.
///
/// Dropping a session closes the transport and sends nothing. [`logout`]
/// is explicit and consuming.
///
/// [`logout`]: ImapSession::logout
pub struct ImapSession<S: Read + Write> {
    stream: BufReader<S>,
    next_tag: u32,
    preauthenticated: bool,
}

impl<S: Read + Write> ImapSession<S> {
    /// Start a session over an established stream and consume its greeting.
    pub fn new(stream: S) -> Result<Self, ImapError> {
        let mut session = Self {
            stream: BufReader::new(stream),
            next_tag: 1,
            preauthenticated: false,
        };
        let greeting = session.read_line()?;
        let head = greeting.head();
        let rest = head
            .strip_prefix(b"* ")
            .ok_or(ImapError::Protocol("greeting is not untagged"))?;
        if strip_word(rest, b"OK").is_some() {
        } else if strip_word(rest, b"PREAUTH").is_some() {
            session.preauthenticated = true;
        } else if strip_word(rest, b"BYE").is_some() {
            return Err(ImapError::Bye);
        } else {
            return Err(ImapError::Protocol(
                "greeting is neither OK, PREAUTH nor BYE",
            ));
        }
        Ok(session)
    }

    /// `CAPABILITY`: the capability atoms, upper-cased.
    pub fn capability(&mut self) -> Result<Vec<String>, ImapError> {
        let lines = self.run(Command::Capability)?;
        let mut capabilities = Vec::new();
        for line in lines {
            if !is_untagged(line.head(), b"CAPABILITY") {
                continue;
            }
            let items = line.items()?;
            if items.len() >= 2 && items[0].is_atom(b"*") && items[1].is_atom(b"CAPABILITY") {
                for item in &items[2..] {
                    if let Some(atom) = item.atom() {
                        capabilities.push(String::from_utf8_lossy(atom).to_ascii_uppercase());
                    }
                }
            }
        }
        if !capabilities
            .iter()
            .any(|value| value == "IMAP4REV1" || value == "IMAP4REV2")
        {
            return Err(ImapError::Protocol("server does not advertise IMAP4rev1"));
        }
        Ok(capabilities)
    }

    /// Authenticate with `LOGIN`. Validates both arguments before writing.
    pub fn login(&mut self, username: &str, password: &str) -> Result<(), ImapError> {
        for value in [username, password] {
            if value.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
                return Err(ImapError::InvalidArgument("credential"));
            }
        }
        if self.preauthenticated {
            return Ok(());
        }
        self.run(Command::Login { username, password })?;
        Ok(())
    }

    /// `LIST "" "*"`: every selectable mailbox name, in server order.
    pub fn list(&mut self) -> Result<Vec<String>, ImapError> {
        let lines = self.run(Command::List)?;
        let mut names = Vec::new();
        for line in lines {
            if !is_untagged(line.head(), b"LIST") {
                continue;
            }
            let items = line.items()?;
            if items.len() < 2 || !items[0].is_atom(b"*") || !items[1].is_atom(b"LIST") {
                continue;
            }
            let [_, _, Item::List(flags), _delimiter, name] = &items[..] else {
                return Err(ImapError::Protocol(
                    "LIST response is not (flags) delimiter name",
                ));
            };
            if flags
                .iter()
                .any(|flag| flag.is_atom(b"\\Noselect") || flag.is_atom(b"\\NonExistent"))
            {
                continue;
            }
            let name = name
                .string()
                .ok_or(ImapError::Protocol("LIST mailbox name is NIL"))?;
            let name = if name.eq_ignore_ascii_case(b"INBOX") {
                "INBOX".to_owned()
            } else {
                String::from_utf8(name.to_vec())
                    .map_err(|_| ImapError::Protocol("LIST mailbox name is not UTF-8"))?
            };
            if !names.contains(&name) {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// `EXAMINE`: open a mailbox read-only and report its UIDVALIDITY.
    pub fn examine(&mut self, mailbox: &str) -> Result<Examined, ImapError> {
        let lines = self.run(Command::Examine { mailbox })?;
        let mut uidvalidity = None;
        let mut exists = None;
        for line in &lines {
            if let Some(value) = uidvalidity_code(line.head()) {
                uidvalidity = Some(value);
                continue;
            }
            if let Some((count, word)) = numbered(line.head()) {
                if word.eq_ignore_ascii_case(b"EXISTS") {
                    exists = Some(count);
                }
            }
        }
        Ok(Examined {
            uidvalidity: uidvalidity.ok_or(ImapError::MissingUidValidity)?,
            exists,
        })
    }

    /// `UID SEARCH`: matching UIDs in the examined mailbox, ascending.
    pub fn uid_search(&mut self, criteria: &SearchCriteria) -> Result<Vec<u32>, ImapError> {
        let lines = self.run(Command::UidSearch { criteria })?;
        let mut uids = Vec::new();
        for line in lines {
            if !is_untagged(line.head(), b"SEARCH") {
                continue;
            }
            let items = line.items()?;
            if items.len() < 2 || !items[0].is_atom(b"*") || !items[1].is_atom(b"SEARCH") {
                continue;
            }
            for item in &items[2..] {
                // RFC 4731/7162 may append a parenthesized modifier; skip it.
                if matches!(item, Item::List(_)) {
                    continue;
                }
                let uid = item
                    .number()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| *n != 0)
                    .ok_or(ImapError::Protocol("SEARCH result is not a UID"))?;
                uids.push(uid);
            }
        }
        uids.sort_unstable();
        uids.dedup();
        Ok(uids)
    }

    /// `UID FETCH <set> (UID RFC822.SIZE)` in batches: the size of each named
    /// UID that still exists. UIDs the server no longer has are absent.
    pub fn uid_sizes(&mut self, uids: &[u32]) -> Result<Vec<(u32, u64)>, ImapError> {
        let mut sorted = uids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut sizes = Vec::new();
        for batch in sorted.chunks(SIZE_PROBE_BATCH) {
            let set = uid_set(batch);
            for line in self.run(Command::UidFetchSizes { set: &set })? {
                let Some(attributes) = fetch_attributes(&line)? else {
                    continue;
                };
                let uid = attribute(&attributes, b"UID")
                    .and_then(Item::number)
                    .and_then(|n| u32::try_from(n).ok());
                let size = attribute(&attributes, b"RFC822.SIZE").and_then(Item::number);
                if let (Some(uid), Some(size)) = (uid, size) {
                    if batch.binary_search(&uid).is_ok() && !sizes.iter().any(|(u, _)| *u == uid) {
                        sizes.push((uid, size));
                    }
                }
            }
        }
        sizes.sort_unstable();
        Ok(sizes)
    }

    /// `UID FETCH <uid> (UID BODY.PEEK[])`: the exact message octets, or
    /// `None` when the server no longer has that UID. Never sets `\Seen`.
    pub fn uid_fetch_body(&mut self, uid: u32) -> Result<Option<Vec<u8>>, ImapError> {
        let mut body = None;
        for line in self.run(Command::UidFetchBody { uid })? {
            let Some(attributes) = fetch_attributes(&line)? else {
                continue;
            };
            let named = attribute(&attributes, b"UID")
                .and_then(Item::number)
                .is_some_and(|n| n == u64::from(uid));
            if !named {
                continue;
            }
            if let Some(Item::String(bytes)) = attribute(&attributes, b"BODY[]") {
                body = Some(bytes.clone());
            }
        }
        Ok(body)
    }

    /// End the session with `LOGOUT`. Consuming, so it is sent at most once.
    pub fn logout(mut self) -> Result<(), ImapError> {
        self.run(Command::Logout)?;
        Ok(())
    }

    fn run(&mut self, command: Command<'_>) -> Result<Vec<Line>, ImapError> {
        let tag = format!("A{:04}", self.next_tag);
        self.next_tag += 1;
        let verb = command.verb();
        let is_logout = matches!(command, Command::Logout);
        let parts = command.parts()?;
        let mut untagged = Vec::new();

        let stream = self.stream.get_mut();
        stream.write_all(tag.as_bytes())?;
        stream.write_all(b" ")?;
        for part in parts {
            match part {
                Part::Text(text) => self.stream.get_mut().write_all(&text)?,
                Part::Literal(bytes) => {
                    // The `{n}` announcement is already written; wait for
                    // the server's go-ahead before sending the octets.
                    self.stream.get_mut().write_all(b"\r\n")?;
                    self.stream.get_mut().flush()?;
                    loop {
                        let line = self.read_line()?;
                        let head = line.head();
                        if head.starts_with(b"+") {
                            break;
                        }
                        if head.starts_with(b"* ") {
                            untagged.push(line);
                            continue;
                        }
                        return Err(self.tagged_failure(&line, &tag, verb));
                    }
                    self.stream.get_mut().write_all(&bytes)?;
                }
            }
        }
        let stream = self.stream.get_mut();
        stream.write_all(b"\r\n")?;
        stream.flush()?;

        loop {
            let line = self.read_line()?;
            let head = line.head();
            if let Some(rest) = head.strip_prefix(b"* ") {
                if strip_word(rest, b"BYE").is_some() && !is_logout {
                    return Err(ImapError::Bye);
                }
                untagged.push(line);
                continue;
            }
            if head.starts_with(b"+") {
                return Err(ImapError::Protocol("unexpected continuation request"));
            }
            let rest = head
                .strip_prefix(tag.as_bytes())
                .and_then(|rest| rest.strip_prefix(b" "))
                .ok_or(ImapError::Protocol("response carries an unexpected tag"))?;
            if strip_word(rest, b"OK").is_some() {
                return Ok(untagged);
            }
            return Err(self.tagged_failure(&line, &tag, verb));
        }
    }

    fn tagged_failure(&self, line: &Line, tag: &str, verb: &'static str) -> ImapError {
        let Some(rest) = line
            .head()
            .strip_prefix(tag.as_bytes())
            .and_then(|rest| rest.strip_prefix(b" "))
        else {
            return ImapError::Protocol("response carries an unexpected tag");
        };
        match strip_word(rest, b"NO").or_else(|| strip_word(rest, b"BAD")) {
            Some(text) => ImapError::Rejected {
                command: verb,
                code: response_code(text),
            },
            None => ImapError::Protocol("tagged status is neither OK, NO nor BAD"),
        }
    }

    /// Read one logical response line, including every literal it carries.
    fn read_line(&mut self) -> Result<Line, ImapError> {
        let mut texts = Vec::new();
        let mut literals = Vec::new();
        let mut total = 0u64;
        loop {
            let mut chunk = Vec::new();
            let read = (&mut self.stream)
                .take(MAX_LINE_BYTES + 1)
                .read_until(b'\n', &mut chunk)?;
            if read == 0 {
                return Err(ImapError::Protocol("unexpected end of stream"));
            }
            if !chunk.ends_with(b"\n") {
                return Err(if chunk.len() as u64 > MAX_LINE_BYTES {
                    ImapError::Protocol("response line exceeds the line cap")
                } else {
                    ImapError::Protocol("unexpected end of stream")
                });
            }
            if !chunk.ends_with(b"\r\n") {
                return Err(ImapError::Protocol("line is not CRLF-terminated"));
            }
            chunk.truncate(chunk.len() - 2);
            total += chunk.len() as u64;
            let literal = literal_announcement(&chunk);
            match literal {
                Some((size, marker)) => {
                    if size > MAX_MESSAGE_BYTES {
                        return Err(ImapError::TooLarge {
                            size,
                            limit: MAX_MESSAGE_BYTES,
                        });
                    }
                    total += size;
                    if total > MAX_MESSAGE_BYTES + MAX_LINE_BYTES {
                        return Err(ImapError::Protocol("response exceeds the response cap"));
                    }
                    chunk.truncate(marker);
                    texts.push(chunk);
                    let mut bytes = vec![0u8; size as usize];
                    self.stream.read_exact(&mut bytes)?;
                    literals.push(bytes);
                }
                None => {
                    texts.push(chunk);
                    return Ok(Line { texts, literals });
                }
            }
        }
    }
}

/// `{n}` at the very end of a text chunk: its size and the marker's offset.
fn literal_announcement(chunk: &[u8]) -> Option<(u64, usize)> {
    let inner = chunk.strip_suffix(b"}")?;
    let open = inner.iter().rposition(|b| *b == b'{')?;
    let digits = &inner[open + 1..];
    if digits.is_empty() || digits.len() > 20 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let size = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some((size, open))
}

/// The attribute list of an untagged `* n FETCH (...)` response.
fn fetch_attributes(line: &Line) -> Result<Option<Vec<Item>>, ImapError> {
    if !matches!(numbered(line.head()), Some((_, word)) if word.eq_ignore_ascii_case(b"FETCH")) {
        return Ok(None);
    }
    let items = line.items()?;
    match &items[..] {
        [star, number, word, Item::List(attributes)]
            if star.is_atom(b"*") && number.number().is_some() && word.is_atom(b"FETCH") =>
        {
            Ok(Some(attributes.clone()))
        }
        _ => Ok(None),
    }
}

/// The value following `name` in a FETCH attribute list.
fn attribute<'a>(attributes: &'a [Item], name: &[u8]) -> Option<&'a Item> {
    attributes
        .chunks(2)
        .find(|pair| pair[0].is_atom(name))
        .and_then(|pair| pair.get(1))
}

/// The concrete session returned by [`connect_implicit_tls`].
pub type ImplicitTlsImapSession = ImapSession<StreamOwned<ClientConnection, TcpStream>>;

/// Connect to an IMAP server with implicit TLS (normally port 993), check its
/// capabilities, and authenticate.
pub fn connect_implicit_tls(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<ImplicitTlsImapSession, ImapError> {
    // Validate before opening a socket or writing a partial login.
    for value in [username, password] {
        if value.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
            return Err(ImapError::InvalidArgument("credential"));
        }
    }

    // Faculties selects rustls's ring backend.  Installation is process-global
    // and idempotent; an already selected provider is not an error here.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let native = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (usable, _) = roots.add_parsable_certificates(native.certs);
    if usable == 0 {
        return Err(ImapError::NoTrustAnchors);
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name =
        ServerName::try_from(host.to_owned()).map_err(|_| ImapError::InvalidServerName)?;
    let connection = ClientConnection::new(Arc::new(config), server_name)?;
    let tcp = TcpStream::connect((host, port))?;
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(IO_TIMEOUT))?;
    let tls = StreamOwned::new(connection, tcp);

    let mut session = ImapSession::new(tls)?;
    let capabilities = session.capability()?;
    if capabilities.iter().any(|value| value == "LOGINDISABLED") {
        return Err(ImapError::LoginDisabled);
    }
    session.login(username, password)?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;

    #[derive(Clone)]
    struct WriteLog(Rc<RefCell<Vec<u8>>>);

    impl WriteLog {
        fn bytes(&self) -> Vec<u8> {
            self.0.borrow().clone()
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.bytes()).into_owned()
        }
    }

    struct ScriptedStream {
        reads: Cursor<Vec<u8>>,
        writes: WriteLog,
    }

    impl Read for ScriptedStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads.read(buffer)
        }
    }

    impl Write for ScriptedStream {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes.0.borrow_mut().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn scripted(script: &[u8]) -> (ImapSession<ScriptedStream>, WriteLog) {
        let writes = WriteLog(Rc::new(RefCell::new(Vec::new())));
        let stream = ScriptedStream {
            reads: Cursor::new(script.to_vec()),
            writes: writes.clone(),
        };
        (ImapSession::new(stream).unwrap(), writes)
    }

    const GREETING: &[u8] = b"* OK [CAPABILITY IMAP4rev1] ready\r\n";

    fn script(parts: &[&[u8]]) -> Vec<u8> {
        let mut out = GREETING.to_vec();
        for part in parts {
            out.extend_from_slice(part);
        }
        out
    }

    #[test]
    fn capability_and_login_quote_and_escape_and_never_echo_the_password() {
        let (mut session, writes) = scripted(&script(&[
            b"* CAPABILITY IMAP4rev1 IDLE\r\nA0001 OK done\r\n",
            b"A0002 OK logged in\r\n",
        ]));
        assert_eq!(session.capability().unwrap(), ["IMAP4REV1", "IDLE"]);
        session.login("me@example.test", "p\"a\\ss").unwrap();
        assert_eq!(
            writes.text(),
            "A0001 CAPABILITY\r\nA0002 LOGIN \"me@example.test\" \"p\\\"a\\\\ss\"\r\n"
        );

        let (mut session, writes) = scripted(&script(&[
            b"A0001 NO [AUTHENTICATIONFAILED] wrong password very-secret\r\n",
        ]));
        let error = session.login("me", "very-secret").unwrap_err();
        assert!(matches!(
            &error,
            ImapError::Rejected { command: "LOGIN", code: Some(code) } if code == "AUTHENTICATIONFAILED"
        ));
        assert!(!error.to_string().contains("very-secret"));
        assert!(writes.text().starts_with("A0001 LOGIN "));

        let (mut session, writes) = scripted(GREETING);
        let error = session
            .login("me", "secret\r\nA2 DELETE INBOX")
            .unwrap_err();
        assert!(matches!(error, ImapError::InvalidArgument(_)));
        assert!(!error.to_string().contains("secret"));
        assert!(writes.bytes().is_empty());
    }

    #[test]
    fn non_ascii_password_goes_as_a_synchronizing_literal() {
        let (mut session, writes) = scripted(&script(&[b"+ go ahead\r\nA0001 OK\r\n"]));
        session.login("me", "pässword").unwrap();
        let mut expected = b"A0001 LOGIN \"me\" {9}\r\n".to_vec();
        expected.extend_from_slice("pässword".as_bytes());
        expected.extend_from_slice(b"\r\n");
        assert_eq!(writes.bytes(), expected);
    }

    #[test]
    fn list_returns_selectable_names_including_literals_and_skips_noselect() {
        let (mut session, writes) = scripted(&script(&[
            b"* LIST (\\HasNoChildren) \"/\" Inbox\r\n",
            b"* LIST (\\Noselect \\HasChildren) \"/\" \"[Gmail]\"\r\n",
            b"* LIST (\\HasNoChildren \\Sent) \"/\" \"[Gmail]/Sent Mail\"\r\n",
            b"* LIST () \"/\" {10}\r\nOdd \"name\"\r\n",
            b"A0001 OK LIST done\r\n",
        ]));
        assert_eq!(
            session.list().unwrap(),
            ["INBOX", "[Gmail]/Sent Mail", "Odd \"name\""]
        );
        assert_eq!(writes.text(), "A0001 LIST \"\" \"*\"\r\n");
    }

    #[test]
    fn examine_reports_uidvalidity_and_requires_it() {
        let (mut session, writes) = scripted(&script(&[
            b"* FLAGS (\\Answered \\Seen)\r\n* 3 EXISTS\r\n* 0 RECENT\r\n",
            b"* OK [UIDVALIDITY 3857529045] UIDs valid\r\n* OK [UIDNEXT 4392] next\r\n",
            b"A0001 OK [READ-ONLY] EXAMINE completed\r\n",
        ]));
        assert_eq!(
            session.examine("INBOX").unwrap(),
            Examined {
                uidvalidity: 3857529045,
                exists: Some(3)
            }
        );
        assert_eq!(writes.text(), "A0001 EXAMINE \"INBOX\"\r\n");

        let (mut session, _) = scripted(&script(&[
            b"* OK [UIDVALIDITY 7] fine :)\r\n* OK still (open\r\nA0001 OK\r\n",
        ]));
        assert_eq!(session.examine("INBOX").unwrap().uidvalidity, 7);

        let (mut session, _) = scripted(&script(&[b"* 0 EXISTS\r\nA0001 OK\r\n"]));
        assert!(matches!(
            session.examine("INBOX"),
            Err(ImapError::MissingUidValidity)
        ));
    }

    #[test]
    fn search_sends_escaped_criteria_and_parses_uids() {
        let (mut session, writes) = scripted(&script(&[
            b"* SEARCH 9 2 84\r\nA0001 OK SEARCH completed\r\n",
        ]));
        let criteria = SearchCriteria {
            from: Some("anthropic \"team\"".into()),
            subject: None,
            since: SearchDate::parse("2024-01-01"),
            before: SearchDate::parse("15-mar-2025"),
        };
        assert_eq!(session.uid_search(&criteria).unwrap(), [2, 9, 84]);
        assert_eq!(
            writes.text(),
            "A0001 UID SEARCH FROM \"anthropic \\\"team\\\"\" SINCE \"1-Jan-2024\" BEFORE \"15-Mar-2025\"\r\n"
        );

        let (mut session, writes) = scripted(&script(&[b"* SEARCH\r\nA0001 OK\r\n"]));
        assert!(session
            .uid_search(&SearchCriteria::default())
            .unwrap()
            .is_empty());
        assert_eq!(writes.text(), "A0001 UID SEARCH ALL\r\n");

        let (mut session, writes) = scripted(&script(&[b"+ ready\r\n* SEARCH 5\r\nA0001 OK\r\n"]));
        let criteria = SearchCriteria {
            subject: Some("Grüße".into()),
            ..SearchCriteria::default()
        };
        assert_eq!(session.uid_search(&criteria).unwrap(), [5]);
        let mut expected = b"A0001 UID SEARCH CHARSET UTF-8 SUBJECT {7}\r\n".to_vec();
        expected.extend_from_slice("Grüße".as_bytes());
        expected.extend_from_slice(b"\r\n");
        assert_eq!(writes.bytes(), expected);
    }

    #[test]
    fn search_dates_parse_both_forms_and_reject_nonsense() {
        assert_eq!(
            SearchDate::parse("2024-02-29").unwrap().imap_text(),
            "29-Feb-2024"
        );
        assert_eq!(
            SearchDate::parse("1-JAN-2024").unwrap().imap_text(),
            "1-Jan-2024"
        );
        for bad in [
            "2023-02-29",
            "2024-13-01",
            "32-Jan-2024",
            "yesterday",
            "1-Foo-2024",
            "",
        ] {
            assert!(SearchDate::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn sizes_batch_into_uid_sets_and_ignore_unsolicited_fetches() {
        let (mut session, writes) = scripted(&script(&[
            b"* 1 FETCH (UID 4 RFC822.SIZE 120)\r\n",
            b"* 2 FETCH (FLAGS (\\Seen))\r\n",
            b"* 3 FETCH (RFC822.SIZE 99 UID 6)\r\n",
            b"A0001 OK\r\n",
        ]));
        assert_eq!(
            session.uid_sizes(&[6, 4, 5, 9]).unwrap(),
            [(4, 120), (6, 99)]
        );
        assert_eq!(writes.text(), "A0001 UID FETCH 4:6,9 (UID RFC822.SIZE)\r\n");
    }

    #[test]
    fn body_fetch_peeks_and_preserves_literal_octets_exactly() {
        let body: &[u8] = b"From: a@b\r\nX: \xff\xfe\r\n\r\n.\r\nA0002 OK fake\r\n) {3}\r\n";
        let mut response = format!("* 7 FETCH (UID 42 BODY[] {{{}}}\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response.extend_from_slice(b" FLAGS (\\Recent))\r\nA0001 OK FETCH completed\r\n");
        let (mut session, writes) = scripted(&script(&[&response]));
        assert_eq!(session.uid_fetch_body(42).unwrap().unwrap(), body);
        assert_eq!(writes.text(), "A0001 UID FETCH 42 (UID BODY.PEEK[])\r\n");

        let (mut session, _) = scripted(&script(&[b"A0001 OK nothing\r\n"]));
        assert_eq!(session.uid_fetch_body(42).unwrap(), None);
    }

    #[test]
    fn an_oversized_literal_is_refused_without_buffering_it() {
        let announced = MAX_MESSAGE_BYTES + 1;
        let response = format!("* 1 FETCH (UID 1 BODY[] {{{announced}}}\r\nxx");
        let (mut session, _) = scripted(&script(&[response.as_bytes()]));
        assert!(matches!(
            session.uid_fetch_body(1),
            Err(ImapError::TooLarge { size, limit }) if size == announced && limit == MAX_MESSAGE_BYTES
        ));
    }

    #[test]
    fn bye_rejections_and_tag_mismatches_fail_closed() {
        let (mut session, _) = scripted(&script(&[b"* BYE shutting down\r\n"]));
        assert!(matches!(session.capability(), Err(ImapError::Bye)));

        let (mut session, _) = scripted(&script(&[b"A0001 BAD [CLIENTBUG] nope\r\n"]));
        assert!(matches!(
            session.examine("x"),
            Err(ImapError::Rejected {
                command: "EXAMINE",
                ..
            })
        ));

        let (mut session, _) = scripted(&script(&[b"B9 OK\r\n"]));
        assert!(matches!(session.capability(), Err(ImapError::Protocol(_))));

        let (session, writes) = scripted(&script(&[b"* BYE logging out\r\nA0001 OK\r\n"]));
        session.logout().unwrap();
        assert_eq!(writes.text(), "A0001 LOGOUT\r\n");
    }

    #[test]
    fn dropping_a_session_sends_nothing_and_preauth_skips_login() {
        let writes = WriteLog(Rc::new(RefCell::new(Vec::new())));
        let mut session = ImapSession::new(ScriptedStream {
            reads: Cursor::new(b"* PREAUTH welcome\r\n".to_vec()),
            writes: writes.clone(),
        })
        .unwrap();
        session.login("me", "pw").unwrap();
        drop(session);
        assert!(writes.bytes().is_empty());
    }

    /// The transport's whole vocabulary. Every `Command` variant is listed in
    /// the exhaustive match, so adding one fails to compile here until it is
    /// classified; the classification refuses every mutating verb.
    #[test]
    fn the_command_set_is_read_only() {
        let criteria = SearchCriteria::default();
        let every_command = [
            Command::Capability,
            Command::Login {
                username: "u",
                password: "p",
            },
            Command::List,
            Command::Examine { mailbox: "INBOX" },
            Command::UidSearch {
                criteria: &criteria,
            },
            Command::UidFetchSizes { set: "1:2" },
            Command::UidFetchBody { uid: 1 },
            Command::Logout,
        ];
        let allowed = [
            "CAPABILITY",
            "LOGIN",
            "LIST",
            "EXAMINE",
            "UID SEARCH",
            "UID FETCH",
            "LOGOUT",
        ];
        for command in &every_command {
            match command {
                Command::Capability
                | Command::Login { .. }
                | Command::List
                | Command::Examine { .. }
                | Command::UidSearch { .. }
                | Command::UidFetchSizes { .. }
                | Command::UidFetchBody { .. }
                | Command::Logout => {}
            }
            assert!(allowed.contains(&command.verb()), "{}", command.verb());
            let mut wire = Vec::new();
            for part in command.parts().unwrap() {
                match part {
                    Part::Text(text) | Part::Literal(text) => wire.extend_from_slice(&text),
                }
            }
            let wire = String::from_utf8(wire).unwrap();
            assert!(wire.starts_with(command.verb()));
            if command.verb() == "UID FETCH" {
                assert!(!wire.contains("BODY["), "{wire}");
                assert!(
                    !wire.contains("RFC822 ") && !wire.contains("RFC822)"),
                    "{wire}"
                );
            }
        }

        // And no mutating verb is spelled anywhere in the non-test source,
        // so none can be sent through some path this test does not list.
        let source = include_str!("mail_imap.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        for verb in [
            "SELECT",
            "STORE",
            "COPY",
            "MOVE",
            "EXPUNGE",
            "DELETE",
            "APPEND",
            "CREATE",
            "RENAME",
            "SUBSCRIBE",
            "CLOSE",
            "UNSELECT",
            "IDLE",
            "SETACL",
            "SETQUOTA",
            "SETMETADATA",
            "AUTHENTICATE",
            "STARTTLS",
        ] {
            for quoted in [
                format!("\"{verb}"),
                format!("\" {verb}"),
                format!("b\"{verb}"),
            ] {
                assert!(!code.contains(&quoted), "transport source spells {quoted}");
            }
        }
    }

    #[test]
    fn uid_sets_compress_runs() {
        assert_eq!(uid_set(&[1, 2, 3, 5, 7, 8]), "1:3,5,7:8");
        assert_eq!(uid_set(&[u32::MAX]), "4294967295");
    }
}
