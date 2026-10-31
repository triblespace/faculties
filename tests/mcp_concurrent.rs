//! The stdio transport answers a quick tool call while a slow one is still
//! running, matching responses to requests by id.

use anyhow::Result;
use faculties::mcp::{self, Faculty, Limits, Tool};
use faculties::out::Out;
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const SCHEMA: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
const WAIT: Duration = Duration::from_secs(10);

/// Output shared with the slow handler, so it can wait for another response.
#[derive(Clone, Default)]
struct Wire(Arc<(Mutex<Vec<u8>>, Condvar)>);

impl Write for Wire {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 .0.lock().unwrap().extend_from_slice(bytes);
        self.0 .1.notify_all();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Wire {
    fn lines(&self) -> Vec<serde_json::Value> {
        let bytes = self.0 .0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

struct Fixture {
    wire: Wire,
}

impl Faculty for Fixture {
    fn tools(&self) -> &[Tool] {
        &[
            Tool {
                name: "test_slow",
                description: "Runs until the quick call has been answered",
                input_schema: SCHEMA,
            },
            Tool {
                name: "test_quick",
                description: "Answers at once",
                input_schema: SCHEMA,
            },
        ]
    }

    fn call(&self, name: &str, _arguments: anybytes::Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "test_quick" => out.line("quick"),
            "test_slow" => {
                let (bytes, answered) = &*self.wire.0;
                let (bytes, timeout) = answered
                    .wait_timeout_while(bytes.lock().unwrap(), WAIT, |bytes| {
                        !String::from_utf8_lossy(bytes).contains(r#""id":3"#)
                    })
                    .unwrap();
                drop(bytes);
                out.line(if timeout.timed_out() {
                    "slow: the quick call was never answered while this ran"
                } else {
                    "slow: released by the quick answer"
                })
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn a_quick_call_is_answered_while_a_slow_call_runs() {
    let wire = Wire::default();
    let fixture = Fixture { wire: wire.clone() };
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"test_slow","arguments":{}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"test_quick","arguments":{}}}"#,
        "\n",
    );
    mcp::serve(
        &[&fixture],
        Limits::default(),
        std::io::Cursor::new(input),
        wire.clone(),
    )
    .unwrap();

    let lines = wire.lines();
    let ids: Vec<_> = lines.iter().map(|line| line["id"].clone()).collect();
    assert_eq!(ids, [1, 3, 2]);
    assert_eq!(lines[1]["result"]["content"][0]["text"], "quick\n");
    assert_eq!(
        lines[2]["result"]["content"][0]["text"],
        "slow: released by the quick answer\n"
    );
}

/// Emits a line every few milliseconds until its output is refused.
struct Ticking;

impl Faculty for Ticking {
    fn tools(&self) -> &[Tool] {
        &[
            Tool {
                name: "test_ticking",
                description: "Emits until its output is refused",
                input_schema: SCHEMA,
            },
            Tool {
                name: "test_quick",
                description: "Answers at once",
                input_schema: SCHEMA,
            },
        ]
    }

    fn call(&self, name: &str, _arguments: anybytes::Bytes, out: &mut Out<'_>) -> Result<()> {
        if name == "test_quick" {
            return out.line("quick");
        }
        let start = std::time::Instant::now();
        while start.elapsed() < WAIT {
            out.line("tick")?;
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}

#[test]
fn a_cancelled_call_gets_no_response_and_its_output_is_refused() {
    let wire = Wire::default();
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"test_ticking","arguments":{}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2,"reason":"client timeout"}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"test_quick","arguments":{}}}"#,
        "\n",
    );
    let start = std::time::Instant::now();
    mcp::serve(
        &[&Ticking],
        Limits::default(),
        std::io::Cursor::new(input),
        wire.clone(),
    )
    .unwrap();
    // The handler stopped at its next emission, long before it would finish.
    assert!(start.elapsed() < WAIT / 2, "took {:?}", start.elapsed());
    let ids: Vec<_> = wire.lines().iter().map(|line| line["id"].clone()).collect();
    assert_eq!(ids, [1, 3]);
}
