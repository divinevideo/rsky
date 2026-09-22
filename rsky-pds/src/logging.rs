//! Log output shaped like the reference PDS's, so the same shipping rules
//! and dashboards read both: one JSON object per line with pino's numeric
//! levels, `time` in milliseconds, `pid`, `hostname`, `name`, and `msg`,
//! with every structured field alongside and dotted field names nested.

use serde_json::{Map, Value};
use std::fmt;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One pino-shaped object per line, as the reference PDS logs.
    Json,
    Text,
    /// `tracing-subscriber`'s JSON lines with the OpenTelemetry trace and
    /// span ids of the current request.
    Traced,
}

impl LogFormat {
    pub fn from_env() -> Self {
        match std::env::var("PDS_LOG_FORMAT").as_deref() {
            Ok("text") => LogFormat::Text,
            Ok("traced") => LogFormat::Traced,
            _ => LogFormat::Json,
        }
    }
}

/// pino's numeric levels.
pub fn pino_level(level: &Level) -> u8 {
    match *level {
        Level::TRACE => 10,
        Level::DEBUG => 20,
        Level::INFO => 30,
        Level::WARN => 40,
        Level::ERROR => 50,
    }
}

fn hostname() -> String {
    hostname_with(|buf| {
        // SAFETY: the buffer outlives the call and its length is passed.
        unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) }
    })
}

fn hostname_with(mut lookup: impl FnMut(&mut [u8; 256]) -> i32) -> String {
    let mut buf = [0u8; 256];
    if lookup(&mut buf) != 0 {
        return "unknown".to_string();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).to_string()
}

/// Collects an event's fields into a JSON object, nesting `a.b` under `a`.
#[derive(Default)]
struct JsonVisitor {
    fields: Map<String, Value>,
    message: Option<String>,
}

impl JsonVisitor {
    fn insert(&mut self, name: &str, value: Value) {
        if name == "message" {
            self.message = Some(match value {
                Value::String(text) => text,
                other => other.to_string(),
            });
            return;
        }
        insert_path(&mut self.fields, name, value);
    }
}

fn insert_path(object: &mut Map<String, Value>, path: &str, value: Value) {
    match path.split_once('.') {
        None => {
            object.insert(path.to_string(), value);
        }
        Some((head, rest)) => {
            let child = object
                .entry(head.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !child.is_object() {
                *child = Value::Object(Map::new());
            }
            insert_path(child.as_object_mut().expect("object"), rest, value);
        }
    }
}

impl Visit for JsonVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field.name(), Value::from(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field.name(), Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field.name(), Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field.name(), Value::from(value));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field.name(), Value::from(value));
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field.name(), Value::from(value.to_string()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.insert(field.name(), Value::from(format!("{value:?}")));
    }
}

pub struct PinoFormat {
    pid: u32,
    hostname: String,
    name: &'static str,
}

impl PinoFormat {
    pub fn new(name: &'static str) -> Self {
        PinoFormat {
            pid: std::process::id(),
            hostname: hostname(),
            name,
        }
    }

    /// The JSON line for an event, without the trailing newline.
    fn line(&self, event: &Event<'_>, time_ms: u128) -> String {
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        let mut object = Map::new();
        object.insert("level".into(), pino_level(event.metadata().level()).into());
        object.insert("time".into(), Value::from(time_ms as u64));
        object.insert("pid".into(), self.pid.into());
        object.insert("hostname".into(), self.hostname.clone().into());
        object.insert("name".into(), self.name.into());
        object.insert("target".into(), event.metadata().target().into());
        for (key, value) in visitor.fields {
            object.insert(key, value);
        }
        object.insert("msg".into(), visitor.message.unwrap_or_default().into());
        Value::Object(object).to_string()
    }
}

impl<S, N> FormatEvent<S, N> for PinoFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let time_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        writeln!(writer, "{}", self.line(event, time_ms))
    }
}

/// Installs the process-wide subscriber: `RUST_LOG` selects the level
/// (default `info`), `PDS_LOG_FORMAT` selects `json` (default), `text`, or
/// `traced`, and spans are exported when `OTEL_EXPORTER_OTLP_ENDPOINT` is
/// set.
/// Where log lines go. The server logs to stdout; a maintenance command
/// prints its JSON result there, so its logs must go to stderr or the
/// result stops being parseable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogTarget {
    Stdout,
    Stderr,
}

pub fn init(format: LogFormat) {
    init_to(format, LogTarget::Stdout)
}

pub fn init_to(format: LogFormat, target: LogTarget) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(crate::telemetry::layer());
    let installed = match (format, target) {
        (LogFormat::Json, LogTarget::Stdout) => registry
            .with(tracing_subscriber::fmt::layer().event_format(PinoFormat::new("pds")))
            .try_init(),
        (LogFormat::Json, LogTarget::Stderr) => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .event_format(PinoFormat::new("pds"))
                    .with_writer(std::io::stderr),
            )
            .try_init(),
        (LogFormat::Text, LogTarget::Stdout) => {
            registry.with(tracing_subscriber::fmt::layer()).try_init()
        }
        // a maintenance command has no request spans worth exporting
        (LogFormat::Text, LogTarget::Stderr) | (LogFormat::Traced, LogTarget::Stderr) => registry
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .try_init(),
        (LogFormat::Traced, LogTarget::Stdout) => {
            registry.with(crate::telemetry::fmt_layer()).try_init()
        }
    };
    if let Err(err) = installed {
        eprintln!("logging already initialised: {err}");
    }
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod tests;
