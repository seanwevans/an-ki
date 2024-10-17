// logging_metrics.rs: Implements logging and metrics collection for monitoring node health and performance.

use tracing::{info, error, warn, debug, Level};
use tracing_subscriber::{fmt, EnvFilter};
use std::time::{Instant, Duration};
use prometheus::{Encoder, TextEncoder, Counter, Histogram, register_counter, register_histogram};
use warp::Filter;

// Metrics definitions
lazy_static! {
    static ref TASKS_PROCESSED: Counter = register_counter!(
        "tasks_processed_total",
        "Total number of tasks processed by the nodes"
    ).unwrap();
    static ref PROCESSING_TIME: Histogram = register_histogram!(
        "task_processing_seconds",
        "Histogram of task processing times"
    ).unwrap();
}

pub fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_level(true)
        .init();
    info!("Logging initialized.");
}

pub fn log_task_processing(start_time: Instant) {
    let elapsed = start_time.elapsed();
    PROCESSING_TIME.observe(elapsed.as_secs_f64());
    TASKS_PROCESSED.inc();
    debug!("Task processed in {:?} seconds.", elapsed);
}

pub async fn metrics_endpoint() -> impl warp::Reply {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    warp::reply::with_header(buffer, "Content-Type", encoder.format_type())
}

pub async fn run_metrics_server() {
    let metrics_route = warp::path("metrics").map(metrics_endpoint);
    warp::serve(metrics_route).run(([127, 0, 0, 1], 9090)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_logging_initialization() {
        init_logging();
        info!("Testing logging initialization");
    }

    #[tokio::test]
    async fn test_metrics_endpoint() {
        let response = metrics_endpoint().await;
        assert!(response.into_response().status().is_success());
    }

    #[test]
    fn test_log_task_processing() {
        let start_time = Instant::now();
        std::thread::sleep(Duration::from_millis(100));
        log_task_processing(start_time);
    }
}
