use super::*;
use std::sync::{Arc, Mutex};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;

/// A `Vec<u8>` sink usable as a tracing-subscriber `MakeWriter`, so a
/// test can assert on exactly what a formatter wrote without touching
/// stdout or any shared global state.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl SharedBuf {
    fn last_line_as_json(&self) -> serde_json::Value {
        let bytes = self.0.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        serde_json::from_str(text.lines().next_back().unwrap()).unwrap()
    }
}

/// [`fmt_layer`], but writing to `buf` instead of stdout, for tests.
fn fmt_layer_to<S>(buf: SharedBuf) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let dispatch = Arc::new(OnceLock::new());
    DispatchCapture(dispatch.clone()).and_then(
        tracing_subscriber::fmt::layer()
            .fmt_fields(tracing_subscriber::fmt::format::JsonFields::new())
            .event_format(CorrelatedJson {
                inner: tracing_subscriber::fmt::format().json(),
                dispatch,
            })
            .with_writer(buf),
    )
}

/// `#[tracing::instrument(skip_all)]` -- used on every handler in this
/// crate -- produces a zero-field span. Pairing `CorrelatedJson` with
/// `JsonFields` (what `fmt_layer` does) is what makes that safe to log;
/// a bare `.event_format(CorrelatedJson::default())` panics on exactly
/// this shape, since `Json` expects fields to have been *recorded* as
/// JSON, not just formatted as JSON on output.
#[test]
fn zero_field_instrumented_spans_format_without_panicking() {
    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::registry().with(fmt_layer_to(buf.clone()));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("zero_field_span");
        let _enter = span.enter();
        tracing::info!("inside a zero-field span");
    });

    let line = buf.last_line_as_json();
    assert_eq!(
        line["fields"]["message"], "inside a zero-field span",
        "{line}"
    );
}

#[test]
fn logs_without_an_active_otel_span_omit_trace_and_span_id() {
    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::registry().with(fmt_layer_to(buf.clone()));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("no otel layer installed");
    });

    let line = buf.last_line_as_json();
    assert!(line.get("trace_id").is_none(), "{line}");
    assert!(line.get("span_id").is_none(), "{line}");
}

#[test]
fn logs_inside_a_traced_span_carry_its_trace_and_span_id() {
    let buf = SharedBuf::default();
    // No exporter/processor attached: this only needs the SDK's id
    // generation and tracing_opentelemetry's span<->context bookkeeping,
    // never touches the network, and doesn't collide with any other
    // test's global state (unlike the process-wide Prometheus recorder).
    let provider = SdkTracerProvider::builder().build();
    let tracer = provider.tracer("test");
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .with(fmt_layer_to(buf.clone()));

    let (expected_trace_id, expected_span_id) =
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("test_span");
            let _enter = span.enter();
            let sc = span.context().span().span_context().clone();
            tracing::info!("inside a traced span");
            (sc.trace_id().to_string(), sc.span_id().to_string())
        });

    let _ = provider.shutdown();

    let line = buf.last_line_as_json();
    assert_eq!(line["trace_id"], expected_trace_id, "{line}");
    assert_eq!(line["span_id"], expected_span_id, "{line}");
}
