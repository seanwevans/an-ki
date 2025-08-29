// main.rs: entry-point

use std::env;
use tracing::error;

mod an_node;
mod api;
mod backup;
mod common;
mod config;
mod dht;
mod ki_node;
mod load_balancer;
mod messaging;
mod node_registry;
mod principal;
mod security;
mod signals;
mod task_recovery;

#[tokio::main]
async fn main() {
    // Initialize tracing subscriber for logging
    tracing_subscriber::fmt::init();

    // Determine the node type based on an environment variable or command-line argument
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        error!("Usage: distributed_neural_network [node_type]");
        std::process::exit(1);
    }

    let node_type = &args[1];

    match node_type.as_str() {
        "principal" => {
            if let Err(e) = principal::run().await {
                error!("Failed to run principal node: {:?}", e);
                std::process::exit(1);
            }
        }
        "an" => {
            if let Err(e) = an_node::run().await {
                error!("Failed to run an node: {:?}", e);
                std::process::exit(1);
            }
        }
        "ki" => {
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
