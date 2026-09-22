use super::*;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Sink {
    type Writer = Sink;
    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

#[test]
fn lines_are_pino_shaped_with_nested_fields() {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .event_format(PinoFormat::new("pds"))
        .with_writer(sink.clone())
        .with_max_level(Level::TRACE)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(
            req.id = 7u64,
            req.method = "GET",
            res.statusCode = 200i64,
            responseTime = 1.5f64,
            cached = true,
            err = %std::io::Error::other("boom"),
            detail = ?vec![1, 2],
            "request completed"
        );
        tracing::warn!("plain");
        let failure: Box<dyn std::error::Error + 'static> = "broken".into();
        tracing::error!(nested.deep.key = "x", failure = failure.as_ref(), "deep");
        tracing::info!(message = 42u64);
        tracing::debug!("debug");
        tracing::trace!("trace");
    });
    let output = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 6, "{output}");
    assert_eq!(lines[2]["failure"], "broken");
    assert_eq!(lines[3]["msg"], "42");
    assert_eq!(lines[3]["level"], 30);
    let request = &lines[0];
    assert_eq!(request["level"], 30);
    assert_eq!(request["name"], "pds");
    assert_eq!(request["msg"], "request completed");
    assert_eq!(request["req"]["id"], 7);
    assert_eq!(request["req"]["method"], "GET");
    assert_eq!(request["res"]["statusCode"], 200);
    assert_eq!(request["responseTime"], 1.5);
    assert_eq!(request["cached"], true);
    assert_eq!(request["err"], "boom");
    assert_eq!(request["detail"], "[1, 2]");
    assert_eq!(request["pid"], std::process::id());
    assert!(request["hostname"].as_str().is_some_and(|h| !h.is_empty()));
    assert!(request["time"]
        .as_u64()
        .is_some_and(|t| t > 1_600_000_000_000));
    assert_eq!(lines[1]["level"], 40);
    assert_eq!(lines[2]["level"], 50);
    assert_eq!(lines[2]["nested"]["deep"]["key"], "x");
    assert_eq!(lines[4]["level"], 20);
    assert_eq!(lines[5]["level"], 10);
    std::io::Write::flush(&mut sink.clone()).unwrap();
}

#[test]
fn a_scalar_field_gives_way_to_a_nested_one() {
    let mut object = Map::new();
    insert_path(&mut object, "req", Value::from(1));
    insert_path(&mut object, "req.id", Value::from(2));
    assert_eq!(Value::Object(object)["req"]["id"], 2);
}

#[test]
fn format_and_hostname_come_from_the_environment() {
    std::env::set_var("PDS_LOG_FORMAT", "text");
    assert_eq!(LogFormat::from_env(), LogFormat::Text);
    std::env::set_var("PDS_LOG_FORMAT", "json");
    assert_eq!(LogFormat::from_env(), LogFormat::Json);
    std::env::set_var("PDS_LOG_FORMAT", "traced");
    assert_eq!(LogFormat::from_env(), LogFormat::Traced);
    std::env::remove_var("PDS_LOG_FORMAT");
    assert_eq!(LogFormat::from_env(), LogFormat::Json);
    assert!(!hostname().is_empty());
    // installing twice is reported, not fatal
    init(LogFormat::Text);
    init(LogFormat::Json);
    init(LogFormat::Traced);
}

#[test]
fn hostname_falls_back_when_the_system_call_fails() {
    assert_eq!(hostname_with(|_| -1), "unknown");
    assert_eq!(
        hostname_with(|buf| {
            buf[..9].copy_from_slice(b"fixture-1");
            0
        }),
        "fixture-1"
    );
}
