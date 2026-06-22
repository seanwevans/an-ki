// main.rs: entry-point

use std::env;
use tracing::error;

use distributed_neural_network::{an_node, ki_node, logging_metrics, principal};

#[tokio::main]
async fn main() {
    // Initialize tracing and metrics
    logging_metrics::init_logging();
    // Spawn the metrics server as a detached background task.
    tokio::spawn(logging_metrics::run_metrics_server());

    // Determine the node type based on an environment variable or command-line argument
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        error!("Usage: distributed_neural_network [node_type]");
        std::process::exit(1);
    }

    let node_type = &args[1];

    match node_type.as_str() {
        "principal" => {
            logging_metrics::set_consensus_state(1.0);
            if let Err(e) = principal::run().await {
                error!("Failed to run principal node: {:?}", e);
                std::process::exit(1);
            }
        }
        "an" => {
            logging_metrics::set_consensus_state(0.0);
            if let Err(e) = an_node::run().await {
                error!("Failed to run an node: {:?}", e);
                std::process::exit(1);
            }
        }
        "ki" => {
            logging_metrics::set_consensus_state(0.0);
            if let Err(e) = ki_node::run().await {
                error!("Failed to run ki node: {:?}", e);
                std::process::exit(1);
            }
        }
        _ => {
            error!("Unknown node type: {}", node_type);
            std::process::exit(1);
        }
    }
}
