//! Distributed tracing: bridges the existing `tracing` spans (117
//! `#[tracing::instrument]` call sites across the handler layer) to
//! OpenTelemetry, exported via OTLP.
//!
//! This is additive to the Prometheus metrics in [`crate::metrics`], not a
//! replacement: metrics stay on the `metrics` facade / `/metrics` endpoint,
//! this only covers spans. Modeled on the reference TypeScript PDS's
//! `telemetry.ts`, which wires the OTEL SDK the same way (there, via
//! `@atproto-labs/opentelemetry-node`).
//!
//! Opt-in and zero-cost when unconfigured: tracing only initializes, and
//! the OTLP exporter only gets built, when `OTEL_EXPORTER_OTLP_ENDPOINT` is
//! set. Deployments that don't run a collector pay nothing -- no exporter,
//! no background export thread, no failed-connection log spam.

use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use rsky_common::env::env_str;
use std::fmt;
use std::sync::{Arc, OnceLock};
use tracing::dispatcher::WeakDispatch;
use tracing::{Dispatch, Subscriber};
use tracing_subscriber::fmt::format::{Format, FormatEvent, FormatFields, Json, Writer};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::layer::Layer;
use tracing_subscriber::registry::LookupSpan;

static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Builds the OTEL tracer provider and returns a `tracing` layer that
/// forwards every span through it, or `None` if
/// `OTEL_EXPORTER_OTLP_ENDPOINT` isn't set.
///
/// The returned provider must be kept alive (and ideally flushed via
/// `shutdown()`) for the process lifetime; [`init`] handles that by setting
/// it as the OTEL global provider, which owns the batch exporter thread.
pub fn layer<S>() -> Option<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    let endpoint = env_str("OTEL_EXPORTER_OTLP_ENDPOINT")?;

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(&endpoint)
        .build()
    {
        Ok(exporter) => exporter,
        Err(error) => {
            tracing::error!("Failed to build OTLP span exporter for {endpoint}: {error}");
            return None;
        }
    };

    let service_name = env_str("OTEL_SERVICE_NAME").unwrap_or_else(|| "rsky-pds".to_string());
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_attributes([KeyValue::new("service.name", service_name)])
                .build(),
        )
        .build();

    let tracer = provider.tracer("rsky-pds");
    // Keep our own handle for shutdown() -- this version of the `global`
    // module has no `shutdown_tracer_provider()`, and `set_tracer_provider`
    // takes the callee's provider by value, so the clone here is the only
    // way to flush on exit.
    let _ = TRACER_PROVIDER.set(provider.clone());
    opentelemetry::global::set_tracer_provider(provider);
    Some(tracing_opentelemetry::layer().with_tracer(tracer))
}

/// Captures the process' `Dispatch` the instant this layer is registered
/// into a subscriber -- i.e. outside of any span/event dispatch -- and
/// shares it with a [`CorrelatedJson`] formatter.
///
/// This indirection exists because `format_event` has no direct way to
/// reach a live `Dispatch`: it runs *during* dispatch, and both
/// `tracing::Span::current()` and `tracing::dispatcher::get_default()` hit
/// tracing's re-entrancy guard there, silently resolving to the no-op
/// dispatcher (this was confirmed by writing the naive
/// `Span::current().context()` version first -- it never panicked, it just
/// silently produced spans with no trace/span id). `on_register_dispatch`
/// is the one hook tracing calls outside of any dispatch, specifically to
/// let layers stash a `Dispatch` for later use -- the same pattern
/// `tracing-opentelemetry`'s own test suite uses for this exact problem.
#[derive(Clone, Default)]
struct DispatchCapture(Arc<OnceLock<WeakDispatch>>);

impl<S> Layer<S> for DispatchCapture
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_register_dispatch(&self, dispatch: &Dispatch) {
        let _ = self.0.set(dispatch.downgrade());
    }
}

/// A JSON log formatter that adds `trace_id`/`span_id` fields (hex, the same
/// representation OTEL itself uses) to every line, so a log entry can be
/// pivoted straight to the corresponding trace in the collector -- the same
/// correlation the reference TS PDS's OTEL Logs SDK gets automatically.
///
/// Implemented as parse-mutate-reserialize around tracing-subscriber's own
/// `Json` formatter (rather than a formatter written from scratch), so
/// field/span rendering stays exactly what it already produces; only the
/// two extra keys are new. `trace_id`/`span_id` are omitted entirely when
/// there's no active OTEL span (e.g. [`layer`] returned `None` because no
/// collector is configured, or the dispatch hasn't been captured yet) --
/// never emitted as `"0"*32`/`"0"*16`, which would look like a real (if
/// degenerate) id.
struct CorrelatedJson {
    inner: Format<Json>,
    dispatch: Arc<OnceLock<WeakDispatch>>,
}

impl<S, N> FormatEvent<S, N> for CorrelatedJson
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        let mut buf = String::new();
        self.inner.format_event(ctx, Writer::new(&mut buf), event)?;

        let otel_context = self
            .dispatch
            .get()
            .and_then(WeakDispatch::upgrade)
            .and_then(|dispatch| {
                let span = ctx.lookup_current()?;
                tracing_opentelemetry::get_otel_context(&span.id(), &dispatch)
            });

        if let Some(span_context) = otel_context
            .as_ref()
            .map(|cx| cx.span().span_context().clone())
            .filter(|sc| sc.is_valid())
        {
            if let Ok(serde_json::Value::Object(mut line)) =
                serde_json::from_str::<serde_json::Value>(buf.trim_end())
            {
                line.insert(
                    "trace_id".to_string(),
                    serde_json::Value::String(span_context.trace_id().to_string()),
                );
                line.insert(
                    "span_id".to_string(),
                    serde_json::Value::String(span_context.span_id().to_string()),
                );
                return writeln!(writer, "{}", serde_json::Value::Object(line));
            }
        }
        write!(writer, "{buf}")
    }
}

/// The layer to install for JSON, trace-correlated logging: [`DispatchCapture`]
/// paired with a `tracing_subscriber::fmt` layer using [`CorrelatedJson`] as
/// its event formatter and [`JsonFields`](tracing_subscriber::fmt::format::JsonFields)
/// as its span-field formatter.
///
/// The fields/formatter pairing matters, not just the event formatter alone:
/// `Json`'s span rendering treats a span's stored `FormattedFields` text as
/// a JSON fragment to embed verbatim, which only holds if fields were
/// *recorded* as JSON in the first place -- the default `DefaultFields`
/// formatter writes plain text instead, which `Json` then fails to parse.
/// Every `#[tracing::instrument(skip_all)]` span in this crate has zero
/// recorded fields, which panics under this mismatch even for an empty
/// field set, so this is not just a style nicety.
pub fn fmt_layer<S>() -> impl Layer<S>
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
            }),
    )
}

/// Flushes and shuts down the tracer provider installed by [`layer`], if
/// any. Best-effort: a failure here just means some in-flight spans may
/// not reach the collector, never a reason to fail shutdown.
pub fn shutdown() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        if let Err(error) = provider.shutdown() {
            tracing::warn!("Error shutting down OTEL tracer provider: {error}");
        }
    }
}

#[cfg(test)]
#[path = "telemetry_tests.rs"]
mod tests;
