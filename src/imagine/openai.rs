//! OpenAI Image API backend: exactly one `POST /v1/images/generations` with
//! `n = 1`, never retried.
//!
//! A call costs money and can take about two minutes, so the client never
//! repeats a send: no automatic retry, no redirect (a 307/308 would resend
//! the body), no pooled connection a transport could replay. A send whose
//! outcome is unknown (timeout, connection dropped after the request left) is
//! reported as uncertain. The image is kept exactly as returned: the base64
//! is decoded once and those bytes are the image; nothing re-encodes them.
//!
//! Facts relied on, from <https://developers.openai.com/api/docs/guides/image-generation>
//! and <https://developers.openai.com/api/reference/resources/images/methods/generate>
//! (read 2026-11-13): image bytes arrive base64 in `data[].b64_json`; the
//! answer carries `created` (Unix seconds) and may echo `size`, `quality`,
//! `background`, `output_format` and `usage`; sizes are `auto` or `WxH` with
//! both edges multiples of 16, aspect between 1:3 and 3:1, no edge above
//! 3840 and 655,360 to 8,294,400 pixels in total; `output_compression`
//! applies to JPEG and WebP only; a transparent background needs PNG or WebP;
//! `gpt-image-2` accepts quality `low|medium|high|auto` only; prompts are at
//! most 32,000 characters; errors are `{"error":{"message","type","code"}}`.

use anybytes::Bytes;
use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::schemas::imagine as schema;

/// The public API root; requests go to `{base}/images/generations`.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Trusted launcher configuration naming another API root (a proxy, or a
/// fake server in tests). Read once by frontends, never by operations.
pub const BASE_URL_VARIABLE: &str = "IMAGINE_OPENAI_BASE_URL";
/// The whole request may take this long. The guide says complex prompts can
/// take up to two minutes.
pub const TIMEOUT: Duration = Duration::from_secs(180);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// The model used when a caller names none (the guide's own examples use it).
pub const DEFAULT_MODEL: &str = "gpt-image-2.5-sunburst";
pub const MAX_PROMPT_CHARS: usize = 32_000;

/// Where requests go. Captured once by a frontend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    base: String,
    timeout: Duration,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::new(DEFAULT_BASE_URL)
    }
}

impl Endpoint {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            timeout: TIMEOUT,
        }
    }

    /// Reads configuration only (`IMAGINE_OPENAI_BASE_URL`), no network.
    pub fn from_environment() -> Self {
        match std::env::var(BASE_URL_VARIABLE) {
            Ok(base) if !base.trim().is_empty() => Self::new(base.trim()),
            _ => Self::default(),
        }
    }

    /// A shorter whole-request timeout (tests).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The generations URL. The key travels only over HTTPS, or plain HTTP
    /// to this machine's loopback (a local proxy or test server).
    pub fn generations_url(&self) -> Result<reqwest::Url> {
        let url = reqwest::Url::parse(&format!(
            "{}/images/generations",
            self.base.trim_end_matches('/')
        ))
        .with_context(|| format!("invalid OpenAI base URL {:?}", self.base))?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "OpenAI base URL {:?} must be https (plain http only to loopback)",
            self.base
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "OpenAI base URL must not carry credentials"
        );
        Ok(url)
    }
}

macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $wire:literal => $id:path),+ $(,)? } default $default:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name { $($variant),+ }
        impl Default for $name {
            fn default() -> Self { Self::$default }
        }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            /// The exact word the API uses.
            pub fn wire(self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
            /// The minted id this value is stored as.
            pub fn id(self) -> triblespace::prelude::Id {
                match self { $(Self::$variant => $id),+ }
            }
            pub fn from_wire(word: &str) -> Option<Self> {
                match word { $($wire => Some(Self::$variant),)+ _ => None }
            }
        }
        impl FromStr for $name {
            type Err = anyhow::Error;
            fn from_str(word: &str) -> Result<Self> {
                Self::from_wire(word.trim()).ok_or_else(|| anyhow!(
                    "{} must be one of {}",
                    stringify!($name).to_ascii_lowercase(),
                    [$($wire),+].join(", ")
                ))
            }
        }
    };
}

wire_enum!(
    /// `quality`.
    Quality {
        Low = "low" => schema::QUALITY_LOW,
        Medium = "medium" => schema::QUALITY_MEDIUM,
        High = "high" => schema::QUALITY_HIGH,
        Xhigh = "xhigh" => schema::QUALITY_XHIGH,
        Max = "max" => schema::QUALITY_MAX,
        Auto = "auto" => schema::QUALITY_AUTO,
    } default Auto
);

wire_enum!(
    /// `output_format`. SVG and GIF exist upstream and are out of scope.
    OutputFormat {
        Png = "png" => schema::FORMAT_PNG,
        Jpeg = "jpeg" => schema::FORMAT_JPEG,
        Webp = "webp" => schema::FORMAT_WEBP,
    } default Png
);

wire_enum!(
    /// `background`.
    Background {
        Transparent = "transparent" => schema::BACKGROUND_TRANSPARENT,
        Opaque = "opaque" => schema::BACKGROUND_OPAQUE,
        Auto = "auto" => schema::BACKGROUND_AUTO,
    } default Auto
);

wire_enum!(
    /// `moderation`.
    Moderation {
        Auto = "auto" => schema::MODERATION_AUTO,
        Low = "low" => schema::MODERATION_LOW,
    } default Auto
);

impl OutputFormat {
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }
    /// The format the bytes themselves declare, by their magic number.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }
}

/// `size`: `auto`, or exact pixels.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Size {
    #[default]
    Auto,
    Exact {
        width: u32,
        height: u32,
    },
}

impl Size {
    pub fn wire(self) -> String {
        match self {
            Self::Auto => "auto".to_owned(),
            Self::Exact { width, height } => format!("{width}x{height}"),
        }
    }
    pub fn validate(self) -> Result<()> {
        let Self::Exact { width, height } = self else {
            return Ok(());
        };
        ensure!(
            width % 16 == 0 && height % 16 == 0 && width > 0 && height > 0,
            "size: width and height must be positive multiples of 16"
        );
        ensure!(
            width <= 3840 && height <= 3840,
            "size: neither edge may exceed 3840 pixels"
        );
        ensure!(
            width <= 3 * height && height <= 3 * width,
            "size: the aspect ratio must be between 1:3 and 3:1"
        );
        let pixels = u64::from(width) * u64::from(height);
        ensure!(
            (655_360..=8_294_400).contains(&pixels),
            "size: total pixels must be between 655,360 and 8,294,400"
        );
        Ok(())
    }
    /// Parse a provider-reported `WxH` (not `auto`).
    fn parse_exact(word: &str) -> Option<(u32, u32)> {
        let (width, height) = word.trim().split_once('x')?;
        Some((width.parse().ok()?, height.parse().ok()?))
    }
}

impl FromStr for Size {
    type Err = anyhow::Error;
    fn from_str(word: &str) -> Result<Self> {
        let word = word.trim();
        if word.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        let (width, height) = Self::parse_exact(&word.to_ascii_lowercase())
            .ok_or_else(|| anyhow!("size must be auto or WIDTHxHEIGHT, e.g. 1536x1024"))?;
        Ok(Self::Exact { width, height })
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        word.parse().map_err(serde::de::Error::custom)
    }
}

/// Everything an OpenAI generation request can say. `n` is always 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub prompt: String,
    pub model: String,
    pub size: Size,
    pub quality: Quality,
    pub output_format: OutputFormat,
    pub output_compression: Option<u8>,
    pub background: Background,
    pub moderation: Moderation,
}

impl Options {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            model: DEFAULT_MODEL.to_owned(),
            size: Size::default(),
            quality: Quality::default(),
            output_format: OutputFormat::default(),
            output_compression: None,
            background: Background::default(),
            moderation: Moderation::default(),
        }
    }

    /// Reject what the API documents as invalid before any credential is
    /// opened or anything is sent.
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.prompt.trim().is_empty(), "prompt must not be empty");
        ensure!(
            self.prompt.chars().count() <= MAX_PROMPT_CHARS,
            "prompt must be at most {MAX_PROMPT_CHARS} characters"
        );
        ensure!(
            !self.model.is_empty()
                && self.model.len() <= 128
                && self.model.bytes().all(|byte| byte.is_ascii_graphic()),
            "model must be a non-empty printable ASCII name"
        );
        self.size.validate()?;
        if let Some(compression) = self.output_compression {
            ensure!(compression <= 100, "output_compression must be 0-100");
            ensure!(
                matches!(self.output_format, OutputFormat::Jpeg | OutputFormat::Webp),
                "output_compression applies to jpeg and webp only"
            );
        }
        ensure!(
            self.background != Background::Transparent
                || matches!(self.output_format, OutputFormat::Png | OutputFormat::Webp),
            "a transparent background needs output_format png or webp"
        );
        ensure!(
            self.model != "gpt-image-2"
                || matches!(
                    self.quality,
                    Quality::Low | Quality::Medium | Quality::High | Quality::Auto
                ),
            "gpt-image-2 accepts quality low, medium, high or auto"
        );
        Ok(())
    }

    fn body(&self) -> Body<'_> {
        Body {
            model: &self.model,
            prompt: &self.prompt,
            n: 1,
            size: self.size.wire(),
            quality: self.quality.wire(),
            output_format: self.output_format.wire(),
            output_compression: self.output_compression,
            background: self.background.wire(),
            moderation: self.moderation.wire(),
        }
    }
}

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    prompt: &'a str,
    n: u8,
    size: String,
    quality: &'static str,
    output_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_compression: Option<u8>,
    background: &'static str,
    moderation: &'static str,
}

/// Token usage as reported; absent counts stay absent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub input_text_tokens: Option<u64>,
    pub input_image_tokens: Option<u64>,
    pub output_text_tokens: Option<u64>,
    pub output_image_tokens: Option<u64>,
}

/// One answered generation: the exact bytes and what the provider said.
#[derive(Clone, Debug)]
pub struct Generated {
    pub bytes: Bytes,
    /// The format the bytes are, by their magic number; when they declare
    /// none, the format the answer names, else the one requested.
    pub format: OutputFormat,
    pub elapsed: Duration,
    /// The answer's `created`, Unix seconds.
    pub created: Option<i64>,
    pub revised_prompt: Option<String>,
    pub size: Option<(u32, u32)>,
    pub quality: Option<Quality>,
    pub background: Option<Background>,
    pub output_format: Option<OutputFormat>,
    pub usage: Usage,
    pub request_id: Option<String>,
}

impl Generated {
    pub fn media_type(&self) -> &'static str {
        self.format.media_type()
    }
    pub fn diagnostic(&self) -> String {
        let mut text = format!(
            "openai {} {} bytes ({:.1}s)",
            self.media_type(),
            self.bytes.len(),
            self.elapsed.as_secs_f64()
        );
        if let Some((width, height)) = self.size {
            text.push_str(&format!(" {width}x{height}"));
        }
        if let Some(total) = self.usage.total_tokens {
            text.push_str(&format!(", {total} tokens"));
        }
        text
    }
}

#[derive(Deserialize)]
struct Answer {
    created: Option<i64>,
    data: Option<Vec<AnswerImage>>,
    background: Option<String>,
    output_format: Option<String>,
    quality: Option<String>,
    size: Option<String>,
    usage: Option<AnswerUsage>,
}

#[derive(Deserialize)]
struct AnswerImage {
    b64_json: Option<String>,
    revised_prompt: Option<String>,
}

#[derive(Deserialize)]
struct AnswerUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    input_tokens_details: Option<TokenDetails>,
    output_tokens_details: Option<TokenDetails>,
}

#[derive(Deserialize)]
struct TokenDetails {
    text_tokens: Option<u64>,
    image_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ErrorAnswer {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    message: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    code: Option<String>,
}

/// Send exactly one generation request with `key` and decode its image.
///
/// Never retried. An error before the connection exists says the request was
/// not sent; any later failure says the send is uncertain and may have been
/// charged. The key never appears in any error.
pub fn generate(endpoint: &Endpoint, key: &str, options: &Options) -> Result<Generated> {
    options.validate()?;
    let url = endpoint.generations_url()?;
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("faculties-imagine/", env!("CARGO_PKG_VERSION")))
        .timeout(endpoint.timeout)
        .connect_timeout(CONNECT_TIMEOUT.min(endpoint.timeout))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .pool_max_idle_per_host(0)
        .build()
        .context("build OpenAI HTTP client")?;
    let started = Instant::now();
    let response = client
        .post(url)
        .header(reqwest::header::AUTHORIZATION, authorization(key)?)
        .json(&options.body())
        .send()
        .map_err(|error| send_error(error, endpoint.timeout))?;
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.bytes().map_err(|error| {
        anyhow::Error::new(error.without_url()).context(format!(
            "OpenAI answered HTTP {status} but its body could not be read; the send is uncertain \
             and may have been charged; it is not retried"
        ))
    })?;
    if !status.is_success() {
        bail!(redact(
            &provider_error(status, request_id.as_deref(), &body),
            key
        ));
    }
    decode(&body, options, request_id, started.elapsed())
}

fn authorization(key: &str) -> Result<reqwest::header::HeaderValue> {
    let mut header = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|_| anyhow!("the OpenAI credential is not a valid header value"))?;
    header.set_sensitive(true);
    Ok(header)
}

fn send_error(error: reqwest::Error, timeout: Duration) -> anyhow::Error {
    let error = error.without_url();
    let message = if error.is_connect() {
        "could not connect to OpenAI; the request was not sent".to_owned()
    } else if error.is_timeout() {
        format!(
            "OpenAI did not answer within {}s; the send is uncertain (it may have been processed \
             and charged); it is not retried",
            timeout.as_secs()
        )
    } else {
        "the OpenAI request failed after it was sent; the send is uncertain (it may have been \
         processed and charged); it is not retried"
            .to_owned()
    };
    anyhow::Error::new(error).context(message)
}

fn provider_error(status: reqwest::StatusCode, request_id: Option<&str>, body: &[u8]) -> String {
    let request = request_id
        .map(|id| format!(" (request id {id})"))
        .unwrap_or_default();
    match serde_json::from_slice::<ErrorAnswer>(body) {
        Ok(ErrorAnswer { error }) => {
            let mut kind = Vec::new();
            kind.extend(error.kind);
            kind.extend(error.code.map(|code| format!("code {code}")));
            let kind = if kind.is_empty() {
                String::new()
            } else {
                format!(", {}", kind.join(", "))
            };
            format!(
                "OpenAI refused the request (HTTP {status}{kind}): {}{request}",
                error.message.as_deref().unwrap_or("no message")
            )
        }
        Err(_) => {
            let text = String::from_utf8_lossy(body);
            let text: String = text.chars().take(500).collect();
            format!("OpenAI answered HTTP {status}{request}: {text}")
        }
    }
}

/// Remove the key, and anything shaped like an OpenAI key (`sk-…`, which an
/// "incorrect API key" message echoes masked), from provider text.
fn redact(text: &str, key: &str) -> String {
    let text = if key.is_empty() {
        text.to_owned()
    } else {
        text.replace(key, "[credential]")
    };
    text.split_inclusive(char::is_whitespace)
        .map(|word| {
            let trimmed = word.trim_start_matches(|c: char| !c.is_alphanumeric());
            if trimmed.starts_with("sk-") {
                let tail: String = word.chars().filter(|c| c.is_whitespace()).collect();
                format!("[credential]{tail}")
            } else {
                word.to_owned()
            }
        })
        .collect()
}

fn decode(
    body: &[u8],
    options: &Options,
    request_id: Option<String>,
    elapsed: Duration,
) -> Result<Generated> {
    const CHARGED: &str = "the request was probably charged; nothing was stored; it is not retried";
    let answer: Answer = serde_json::from_slice(body).map_err(|error| {
        anyhow!("OpenAI's answer is not the expected JSON ({error}); {CHARGED}")
    })?;
    let image = answer
        .data
        .unwrap_or_default()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("OpenAI's answer holds no image; {CHARGED}"))?;
    let encoded = image
        .b64_json
        .ok_or_else(|| anyhow!("OpenAI's answer holds no b64_json image; {CHARGED}"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|error| anyhow!("OpenAI's image is not valid base64 ({error}); {CHARGED}"))?;
    ensure!(!bytes.is_empty(), "OpenAI's image is empty; {CHARGED}");
    let output_format = answer
        .output_format
        .as_deref()
        .and_then(OutputFormat::from_wire);
    let format = OutputFormat::sniff(&bytes)
        .or(output_format)
        .unwrap_or(options.output_format);
    let usage = answer.usage.map_or_else(Usage::default, |usage| Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        input_text_tokens: usage
            .input_tokens_details
            .as_ref()
            .and_then(|d| d.text_tokens),
        input_image_tokens: usage
            .input_tokens_details
            .as_ref()
            .and_then(|d| d.image_tokens),
        output_text_tokens: usage
            .output_tokens_details
            .as_ref()
            .and_then(|d| d.text_tokens),
        output_image_tokens: usage
            .output_tokens_details
            .as_ref()
            .and_then(|d| d.image_tokens),
    });
    Ok(Generated {
        bytes: Bytes::from(bytes),
        format,
        elapsed,
        created: answer.created,
        revised_prompt: image.revised_prompt,
        size: answer.size.as_deref().and_then(Size::parse_exact),
        quality: answer.quality.as_deref().and_then(Quality::from_wire),
        background: answer.background.as_deref().and_then(Background::from_wire),
        output_format,
        usage,
        request_id,
    })
}

#[cfg(test)]
pub(crate) mod fake {
    //! A fake OpenAI server on 127.0.0.1:0 that counts requests. No real
    //! OpenAI call happens anywhere in the tests.
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Clone)]
    pub enum Reply {
        /// Answer with this status and JSON body.
        Json(u16, String),
        /// Read the request, then close without answering.
        Drop,
        /// Read the request, then wait this long before closing.
        Stall(Duration),
    }

    pub struct Server {
        pub base: String,
        pub requests: Arc<AtomicUsize>,
        /// Each received request: its head (lowercased) and body.
        pub received: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
    }

    impl Server {
        pub fn start(reply: Reply) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}/v1", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let received = Arc::new(Mutex::new(Vec::new()));
            let (count, log) = (requests.clone(), received.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let (count, log, reply) = (count.clone(), log.clone(), reply.clone());
                    std::thread::spawn(move || serve(stream, &count, &log, &reply));
                }
            });
            Self {
                base,
                requests,
                received,
            }
        }

        pub fn count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }

        pub fn bodies(&self) -> Vec<serde_json::Value> {
            self.received
                .lock()
                .unwrap()
                .iter()
                .map(|(_, body)| serde_json::from_slice(body).unwrap())
                .collect()
        }

        pub fn heads(&self) -> Vec<String> {
            self.received
                .lock()
                .unwrap()
                .iter()
                .map(|(head, _)| head.clone())
                .collect()
        }
    }

    fn serve(
        mut stream: TcpStream,
        count: &AtomicUsize,
        log: &Mutex<Vec<(String, Vec<u8>)>>,
        reply: &Reply,
    ) {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 8192];
        let head_end = loop {
            if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break at + 4;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buffer.len() < head_end + length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        count.fetch_add(1, Ordering::SeqCst);
        log.lock()
            .unwrap()
            .push((head, buffer[head_end..head_end + length].to_vec()));
        match reply {
            Reply::Json(status, body) => {
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\nx-request-id: req_fake\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
            Reply::Drop => {}
            Reply::Stall(wait) => std::thread::sleep(*wait),
        }
    }

    /// A tiny valid image in `format`, encoded by the `image` crate.
    pub fn image(format: image::ImageFormat) -> Vec<u8> {
        let picture =
            image::RgbImage::from_fn(4, 4, |x, y| image::Rgb([x as u8 * 60, y as u8 * 60, 7]));
        let mut encoded = std::io::Cursor::new(Vec::new());
        picture.write_to(&mut encoded, format).unwrap();
        encoded.into_inner()
    }

    /// A success answer carrying `bytes` base64-encoded.
    pub fn success(bytes: &[u8], output_format: &str) -> String {
        use base64::Engine as _;
        serde_json::json!({
            "created": 1_763_000_000,
            "background": "opaque",
            "output_format": output_format,
            "quality": "medium",
            "size": "1024x1024",
            "data": [{"b64_json": base64::engine::general_purpose::STANDARD.encode(bytes)}],
            "usage": {
                "input_tokens": 50, "output_tokens": 1056, "total_tokens": 1106,
                "input_tokens_details": {"text_tokens": 50, "image_tokens": 0},
                "output_tokens_details": {"text_tokens": 0, "image_tokens": 1056}
            }
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Reply, Server};
    use super::*;

    const KEY: &str = "sk-proj-SYNTHETICKEY0123456789abcdef";

    #[test]
    fn success_decodes_exact_bytes_and_reports_the_answer() {
        let png = fake::image(image::ImageFormat::Png);
        let server = Server::start(Reply::Json(200, fake::success(&png, "png")));
        let mut options = Options::new("a lighthouse at dusk");
        options.quality = Quality::Medium;
        options.size = Size::Exact {
            width: 1024,
            height: 1024,
        };
        let generated = generate(&Endpoint::new(&server.base), KEY, &options).unwrap();
        assert_eq!(generated.bytes.as_ref(), png.as_slice());
        assert_eq!(generated.media_type(), "image/png");
        assert_eq!(generated.created, Some(1_763_000_000));
        assert_eq!(generated.size, Some((1024, 1024)));
        assert_eq!(generated.quality, Some(Quality::Medium));
        assert_eq!(generated.background, Some(Background::Opaque));
        assert_eq!(generated.usage.total_tokens, Some(1106));
        assert_eq!(generated.usage.output_image_tokens, Some(1056));
        assert_eq!(generated.request_id.as_deref(), Some("req_fake"));
        assert_eq!(server.count(), 1);
        let body = &server.bodies()[0];
        assert_eq!(body["n"], 1);
        assert_eq!(body["prompt"], "a lighthouse at dusk");
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["size"], "1024x1024");
        assert_eq!(body["quality"], "medium");
        assert_eq!(body["output_format"], "png");
        assert!(body.get("output_compression").is_none());
        let head = &server.heads()[0];
        assert!(head.starts_with("post /v1/images/generations "), "{head}");
        assert!(head.contains(&format!(
            "authorization: bearer {}",
            KEY.to_ascii_lowercase()
        )));
    }

    #[test]
    fn jpeg_bytes_keep_their_media_type() {
        let jpeg = fake::image(image::ImageFormat::Jpeg);
        let server = Server::start(Reply::Json(200, fake::success(&jpeg, "jpeg")));
        let mut options = Options::new("a cat");
        options.output_format = OutputFormat::Jpeg;
        options.output_compression = Some(80);
        let generated = generate(&Endpoint::new(&server.base), KEY, &options).unwrap();
        assert_eq!(generated.bytes.as_ref(), jpeg.as_slice());
        assert_eq!(generated.media_type(), "image/jpeg");
        assert_eq!(server.bodies()[0]["output_compression"], 80);
    }

    #[test]
    fn provider_errors_surface_the_message_and_never_the_key() {
        let body = serde_json::json!({"error": {
            "message": format!("Incorrect API key provided: {KEY}. Also sk-proj-****cdef was masked."),
            "type": "invalid_request_error", "code": "invalid_api_key"}})
        .to_string();
        let server = Server::start(Reply::Json(401, body));
        let error = generate(&Endpoint::new(&server.base), KEY, &Options::new("x")).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("Incorrect API key provided"), "{text}");
        assert!(text.contains("invalid_api_key"), "{text}");
        assert!(text.contains("req_fake"), "{text}");
        assert!(!text.contains(KEY), "{text}");
        assert!(!text.contains("sk-"), "{text}");
        assert_eq!(server.count(), 1);
    }

    #[test]
    fn a_dropped_connection_is_uncertain_and_not_retried() {
        let server = Server::start(Reply::Drop);
        let error = generate(&Endpoint::new(&server.base), KEY, &Options::new("x")).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("uncertain"), "{text}");
        assert!(text.contains("not retried"), "{text}");
        assert!(!text.contains(KEY));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(server.count(), 1);
    }

    #[test]
    fn a_timeout_is_uncertain_and_not_retried() {
        let server = Server::start(Reply::Stall(Duration::from_secs(5)));
        let endpoint = Endpoint::new(&server.base).with_timeout(Duration::from_millis(500));
        let error = generate(&endpoint, KEY, &Options::new("x")).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("uncertain"), "{text}");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(server.count(), 1);
    }

    #[test]
    fn nothing_listening_is_reported_as_not_sent() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let endpoint = Endpoint::new(format!("http://127.0.0.1:{port}/v1"));
        let error = generate(&endpoint, KEY, &Options::new("x")).unwrap_err();
        assert!(format!("{error:#}").contains("not sent"), "{error:#}");
    }

    #[test]
    fn plain_http_is_refused_off_loopback() {
        assert!(Endpoint::new("http://example.com/v1")
            .generations_url()
            .is_err());
        assert!(Endpoint::new("https://example.com/v1")
            .generations_url()
            .is_ok());
        assert!(Endpoint::new("http://127.0.0.1:9/v1")
            .generations_url()
            .is_ok());
        assert!(Endpoint::new("http://localhost:9/v1")
            .generations_url()
            .is_ok());
    }

    #[test]
    fn documented_limits_are_checked_locally() {
        let ok = |size: &str| {
            let mut options = Options::new("x");
            options.size = size.parse().unwrap();
            options.validate()
        };
        assert!(ok("auto").is_ok());
        assert!(ok("1024x1024").is_ok());
        assert!(ok("1536x864").is_ok());
        assert!(ok("3840x2160").is_ok());
        assert!(ok("1000x1000").is_err(), "not multiples of 16");
        assert!(ok("3856x2160").is_err(), "edge above 3840");
        assert!(ok("2400x640").is_err(), "aspect beyond 3:1");
        assert!(ok("512x512").is_err(), "too few pixels");
        let mut options = Options::new("x");
        options.output_compression = Some(50);
        assert!(
            options.validate().is_err(),
            "compression needs jpeg or webp"
        );
        options.output_format = OutputFormat::Webp;
        assert!(options.validate().is_ok());
        options.output_format = OutputFormat::Jpeg;
        options.background = Background::Transparent;
        assert!(options.validate().is_err(), "transparent needs png or webp");
        let mut options = Options::new("x");
        options.model = "gpt-image-2".into();
        options.quality = Quality::Xhigh;
        assert!(options.validate().is_err());
        assert!(Options::new(" ").validate().is_err());
    }

    #[test]
    fn sniffing_reads_magic_numbers() {
        assert_eq!(
            OutputFormat::sniff(&fake::image(image::ImageFormat::Png)),
            Some(OutputFormat::Png)
        );
        assert_eq!(
            OutputFormat::sniff(&fake::image(image::ImageFormat::Jpeg)),
            Some(OutputFormat::Jpeg)
        );
        assert_eq!(
            OutputFormat::sniff(&fake::image(image::ImageFormat::WebP)),
            Some(OutputFormat::Webp)
        );
        assert_eq!(OutputFormat::sniff(b"nothing"), None);
    }
}
