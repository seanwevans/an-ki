// logging_metrics.rs: Implements logging and metrics collection for monitoring node health and performance.

use lazy_static::lazy_static;
use once_cell::sync::OnceCell;
use opentelemetry::{global, trace::TracerProvider as _};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::{BatchSpanProcessor, SdkTracerProvider};
use prometheus::{
    register_counter, register_gauge, register_histogram, Counter, Encoder, Gauge, Histogram,
    TextEncoder,
};
use std::convert::Infallible;
use std::env;
use std::time::Instant;
use tracing::{debug, info};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter, Registry};
use warp::Filter;

// Metrics definitions
lazy_static! {
    static ref TASKS_PROCESSED: Counter = register_counter!(
        "tasks_processed_total",
        "Total number of tasks processed by the nodes"
    )
    .unwrap();
    static ref PROCESSING_TIME: Histogram = register_histogram!(
        "task_processing_seconds",
        "Histogram of task processing times"
    )
    .unwrap();
    static ref NODE_STATUS: Gauge =
        register_gauge!("node_status", "Current status of the node (1=up, 0=down)").unwrap();
    static ref CONSENSUS_STATE: Gauge = register_gauge!(
        "consensus_state",
        "Consensus state of the node (1=leader, 0=follower)"
    )
    .unwrap();
}

static INIT: OnceCell<()> = OnceCell::new();

pub fn init_logging() {
    INIT.get_or_init(|| {
        let endpoint = crate::config::load_settings()
            .ok()
            .and_then(|s| s.otlp_endpoint)
            .or_else(|| env::var("OTLP_ENDPOINT").ok())
            .unwrap_or_else(|| "http://localhost:4317".to_string());

        let exporter = SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .expect("failed to create OTLP exporter");

        let processor = BatchSpanProcessor::builder(exporter).build();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(processor)
            .build();

        let tracer = provider.tracer("an-ki");
        let otel_layer = OpenTelemetryLayer::new(tracer);

        global::set_tracer_provider(provider);

        Registry::default()
            .with(fmt::layer().with_target(false))
            .with(EnvFilter::from_default_env())
            .with(otel_layer)
            .init();

        set_node_status(1.0);
        info!("Logging initialized.");
    });
}

pub fn set_node_status(status: f64) {
    NODE_STATUS.set(status);
}

pub fn set_consensus_state(state: f64) {
    CONSENSUS_STATE.set(state);
}

pub fn log_task_processing(start_time: Instant) {
    let elapsed = start_time.elapsed();
    PROCESSING_TIME.observe(elapsed.as_secs_f64());
    TASKS_PROCESSED.inc();
    debug!("Task processed in {:?} seconds.", elapsed);
}

pub async fn metrics_endpoint() -> Result<impl warp::Reply, Infallible> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    Ok(warp::reply::with_header(
        buffer,
        "Content-Type",
        encoder.format_type(),
    ))
}

pub async fn run_metrics_server() {
    let metrics_route = warp::path("metrics").and_then(metrics_endpoint);
    warp::serve(metrics_route).run(([127, 0, 0, 1], 9090)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::time::Duration;
    static INIT: Lazy<()> = Lazy::new(|| {
        init_logging();
    });

    #[tokio::test]
    async fn test_logging_initialization() {
        Lazy::force(&INIT);
        info!("Testing logging initialization");
    }

    #[tokio::test]
    async fn test_metrics_endpoint() {
        Lazy::force(&INIT);
        use warp::Reply;
        let response = metrics_endpoint().await.unwrap();
        assert!(response.into_response().status().is_success());
    }

    #[test]
    fn test_log_task_processing() {
        let start_time = Instant::now() - Duration::from_millis(50);
        log_task_processing(start_time);
    }
}
