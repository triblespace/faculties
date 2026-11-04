//! Bounded native MCP client/framing over an explicitly granted Unix socket.
//! No executable spawning, endpoint discovery, bearer, fallback, or retries.

use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};

use crate::out::{Out, Part};

pub const FRAME_BYTES: usize = 1024 * 1024;
pub const DIAGNOSTIC_BYTES: usize = 16 * 1024;

/// Refresh the socket timeout at each underlying read, so a slow byte stream
/// cannot turn a finite frame deadline into an unlimited sequence of timeouts.
pub(crate) struct TimedRead {
    pub stream: UnixStream,
    pub deadline: Instant,
}
impl Read for TimedRead {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let left = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "native MCP deadline")
            })?;
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(bytes)
    }
}

pub(crate) fn write_frame(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> Result<()> {
    ensure!(
        bytes.len() <= FRAME_BYTES && !bytes.contains(&b'\n'),
        "invalid or oversized native MCP frame"
    );
    let mut frame = Vec::with_capacity(bytes.len() + 1);
    frame.extend_from_slice(bytes);
    frame.push(b'\n');
    let mut remaining = frame.as_slice();
    while !remaining.is_empty() {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .context("native MCP write deadline")?;
        stream.set_write_timeout(Some(left))?;
        let written = stream.write(remaining)?;
        ensure!(written != 0, "native MCP closed while writing");
        remaining = &remaining[written..];
    }
    Ok(())
}

pub struct Client {
    input: BufReader<TimedRead>,
    output: UnixStream,
    deadline: Instant,
    next_id: u64,
}
impl Client {
    pub fn connect(path: &Path, timeout: Duration) -> Result<Self> {
        ensure!(path.is_absolute(), "native MCP socket must be absolute");
        ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(300),
            "native MCP timeout must be 1..300000 ms"
        );
        // A nonblocking AF_UNIX connect avoids an unbounded wait on a full
        // listener backlog; EAGAIN is refusal, never a retry loop.
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        ensure!(
            bytes.len() < address.sun_path.len() && !bytes.contains(&0),
            "native MCP socket path is too long or contains NUL"
        );
        address.sun_family = libc::AF_UNIX as _;
        for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
            *target = *byte as _;
        }
        let raw = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        ensure!(
            raw >= 0,
            "create native MCP socket: {}",
            std::io::Error::last_os_error()
        );
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&address) as _,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("connect to provisioned native MCP socket (not retried)");
        }
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(false)?;
        Self::connected(stream, timeout)
    }

    fn connected(stream: UnixStream, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        let mut client = Self {
            input: BufReader::new(TimedRead {
                stream: stream.try_clone()?,
                deadline,
            }),
            output: stream,
            deadline,
            next_id: 1,
        };
        let initialized = client.request(
            "initialize",
            json!({
                "protocolVersion": super::PROTOCOL_VERSION, "capabilities": {},
                "clientInfo": {"name": "faculties-cli", "version": crate::GIT_VERSION}
            }),
            &mut |_| Ok(()),
        )?;
        ensure!(
            initialized["protocolVersion"] == super::PROTOCOL_VERSION,
            "native MCP protocol mismatch"
        );
        client.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))?;
        Ok(client)
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        write_frame(
            &mut self.output,
            &serde_json::to_vec(message)?,
            self.deadline,
        )
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        diagnostic: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))?;
        let mut diagnostic_bytes = 0usize;
        for _ in 0..32 {
            let bytes = super::read_frame(&mut self.input, FRAME_BYTES)?
                .context("native MCP connection ended before its response")?;
            let response: Value = serde_json::from_slice(&bytes)?;
            ensure!(
                response["jsonrpc"] == "2.0",
                "invalid native MCP response version"
            );
            if response.get("id").is_none() && response["method"] == "notifications/message" {
                let text = response["params"]["data"]
                    .as_str()
                    .context("invalid native diagnostic")?;
                diagnostic_bytes = diagnostic_bytes
                    .checked_add(text.len())
                    .context("diagnostic length overflow")?;
                ensure!(
                    diagnostic_bytes <= DIAGNOSTIC_BYTES,
                    "native diagnostics exceed budget"
                );
                diagnostic(text)?;
                continue;
            }
            ensure!(
                response["id"].as_u64() == Some(id),
                "native MCP response id mismatch"
            );
            if let Some(error) = response.get("error") {
                bail!(
                    "native MCP error: {}",
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("invalid error")
                );
            }
            return response
                .get("result")
                .cloned()
                .context("native MCP response has no result");
        }
        bail!("native MCP notification budget exceeded")
    }

    pub fn call(
        &mut self,
        name: &str,
        arguments: Value,
        out: &mut Out<'_>,
        diagnostic: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        self.call_inner(name, arguments, out, diagnostic)
            .context("native operation outcome may be unknown; request was not retried")
    }

    fn call_inner(
        &mut self,
        name: &str,
        arguments: Value,
        out: &mut Out<'_>,
        diagnostic: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        let result = self
            .request(
                "tools/call",
                json!({"name":name, "arguments":arguments}),
                diagnostic,
            )
            .context("native operation outcome may be unknown; request was not retried")?;
        let content = result["content"]
            .as_array()
            .context("native MCP content is not an array")?;
        let failed = match result.get("isError") {
            None => false,
            Some(Value::Bool(value)) => *value,
            _ => bail!("native MCP isError is not boolean"),
        };
        let parts = content
            .iter()
            .map(decode_part)
            .collect::<Result<Vec<_>>>()?;
        if failed {
            for part in parts {
                if let Part::Text { text } = part {
                    diagnostic(&text)?;
                }
            }
            bail!("native Memory operation failed; any admitted write was not retried");
        }
        for part in parts {
            out.emit(part)?;
        }
        Ok(())
    }
}

fn decode_part(value: &Value) -> Result<Part> {
    let string = |name: &str| {
        value[name]
            .as_str()
            .with_context(|| format!("native content lacks {name}"))
    };
    match string("type")? {
        "text" => Ok(Part::Text {
            text: string("text")?.to_owned(),
        }),
        kind @ ("image" | "audio") => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(string("data")?)?
                .into();
            let mime_type = string("mimeType")?.to_owned();
            Ok(if kind == "image" {
                Part::Image { bytes, mime_type }
            } else {
                Part::Audio { bytes, mime_type }
            })
        }
        "resource" => {
            let resource = &value["resource"];
            let bytes = match (resource.get("blob"), resource.get("text")) {
                (Some(Value::String(blob)), None) => {
                    base64::engine::general_purpose::STANDARD.decode(blob)?
                }
                (None, Some(Value::String(text))) => text.as_bytes().to_vec(),
                _ => bail!("native resource must have exactly one byte payload"),
            };
            Ok(Part::Blob {
                bytes: bytes.into(),
                mime_type: resource["mimeType"]
                    .as_str()
                    .context("resource MIME missing")?
                    .to_owned(),
                uri: resource["uri"]
                    .as_str()
                    .context("resource URI missing")?
                    .to_owned(),
            })
        }
        _ => bail!("unsupported native MCP content type"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_budget_and_delimiter_are_shared_with_server() {
        assert!(crate::mcp::read_frame(&mut Cursor::new(b"1234\n"), 3).is_err());
        assert!(crate::mcp::read_frame(&mut Cursor::new(b"{}"), 8).is_err());
        assert_eq!(
            crate::mcp::read_frame(&mut Cursor::new(b"{}\r\n"), 8).unwrap(),
            Some(b"{}".to_vec())
        );
    }

    #[test]
    fn native_content_preserves_text_and_rejects_ambiguous_payloads() {
        assert_eq!(
            decode_part(&json!({"type":"text", "text":"range: x\nid: y\n"})).unwrap(),
            Part::Text {
                text: "range: x\nid: y\n".into()
            }
        );
        assert!(decode_part(&json!({"type":"resource", "resource":{"uri":"x", "mimeType":"x", "text":"x", "blob":"eA=="}})).is_err());
    }

    #[test]
    fn client_keeps_diagnostics_off_stdout_and_never_retries_a_lost_write() {
        for lose_response in [false, true] {
            let (client, mut peer) = UnixStream::pair().unwrap();
            let thread = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut input = BufReader::new(TimedRead {
                    stream: peer.try_clone().unwrap(),
                    deadline,
                });
                let init: Value = serde_json::from_slice(
                    &super::super::read_frame(&mut input, FRAME_BYTES)
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                write_frame(&mut peer, &serde_json::to_vec(&json!({"jsonrpc":"2.0", "id":init["id"], "result":{"protocolVersion":super::super::PROTOCOL_VERSION}})).unwrap(), deadline).unwrap();
                let initialized: Value = serde_json::from_slice(
                    &super::super::read_frame(&mut input, FRAME_BYTES)
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(initialized["method"], "notifications/initialized");
                let call: Value = serde_json::from_slice(
                    &super::super::read_frame(&mut input, FRAME_BYTES)
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    call["params"]["arguments"]["summary"],
                    "literal @/private\nsecond line"
                );
                if lose_response {
                    return;
                }
                write_frame(&mut peer, &serde_json::to_vec(&json!({"jsonrpc":"2.0", "method":"notifications/message", "params":{"level":"warning", "data":"partial view"}})).unwrap(), deadline).unwrap();
                write_frame(&mut peer, &serde_json::to_vec(&json!({"jsonrpc":"2.0", "id":call["id"], "result":{"content":[{"type":"text", "text":"range: a..b\nid: c\n"}], "isError":false}})).unwrap(), deadline).unwrap();
                // The frontend owns one operation; no hidden follow-up/retry.
                assert!(super::super::read_frame(&mut input, FRAME_BYTES)
                    .unwrap()
                    .is_none());
            });
            let mut client = Client::connected(client, Duration::from_secs(2)).unwrap();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let result = client.call(
                "memory_create",
                json!({"summary":"literal @/private\nsecond line"}),
                &mut Out::new(&mut |part| {
                    stdout.push(part);
                    Ok(())
                }),
                &mut |diagnostic| {
                    stderr.push(diagnostic.to_owned());
                    Ok(())
                },
            );
            if lose_response {
                assert!(format!("{:#}", result.unwrap_err()).contains("outcome may be unknown"));
                assert!(stdout.is_empty());
            } else {
                result.unwrap();
                assert_eq!(
                    stdout,
                    [Part::Text {
                        text: "range: a..b\nid: c\n".into()
                    }]
                );
                assert_eq!(stderr, ["partial view"]);
            }
            drop(client);
            thread.join().unwrap();
        }
    }
}
