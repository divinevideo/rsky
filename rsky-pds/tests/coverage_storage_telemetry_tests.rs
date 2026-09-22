use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn configured_otlp_endpoint_installs_a_tracer_and_can_shut_down() {
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://[invalid");
    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || CapturedLogs(sink.clone()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        assert!(rsky_pds::telemetry::layer::<tracing_subscriber::Registry>().is_none());
    });
    assert!(String::from_utf8(logs.lock().unwrap().clone())
        .unwrap()
        .contains("Failed to build OTLP span exporter"));

    std::env::set_var(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "http://127.0.0.1:9/v1/traces",
    );
    let layer = rsky_pds::telemetry::layer::<tracing_subscriber::Registry>();
    assert!(
        layer.is_some(),
        "a configured collector must enable span export"
    );
    rsky_pds::telemetry::shutdown();
    // A second shutdown is a provider error, but stopping the service must
    // remain best-effort rather than panic.
    rsky_pds::telemetry::shutdown();
}
