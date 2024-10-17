// network.rs: Abstracts network operations such as connecting nodes and handling retries.

use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tracing::{info, error};
use std::error::Error;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct NetworkManager {
    pub connected_nodes: Arc<RwLock<HashSet<SocketAddr>>>,
}

impl NetworkManager {
    pub fn new() -> Self {
        NetworkManager {
            connected_nodes: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub async fn connect_to_node(&self, address: SocketAddr, retry_count: u32, timeout_duration: Duration) -> Result<(), Box<dyn Error>> {
        for attempt in 0..retry_count {
            match timeout(timeout_duration, TcpStream::connect(address)).await {
                Ok(Ok(_stream)) => {
                    info!("Successfully connected to node at: {}", address);
                    self.connected_nodes.write().unwrap().insert(address);
                    return Ok(());
                }
                Ok(Err(e)) => {
                    error!("Failed to connect to node at {}: {}. Attempt {}/{}", address, e, attempt + 1, retry_count);
                }
                Err(_) => {
                    error!("Connection to node at {} timed out. Attempt {}/{}", address, attempt + 1, retry_count);
                }
            }
        }
        Err("Failed to connect after retries".into())
    }

    pub fn disconnect_node(&self, address: &SocketAddr) {
        let mut nodes = self.connected_nodes.write().unwrap();
        if nodes.remove(address) {
            info!("Disconnected from node at: {}", address);
        } else {
            error!("Node not found for disconnection: {}", address);
        }
    }

    pub fn list_connected_nodes(&self) -> Vec<SocketAddr> {
        let nodes = self.connected_nodes.read().unwrap();
        nodes.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_connect_to_node() {
        let network_manager = NetworkManager::new();
        let test_address: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        let result = network_manager.connect_to_node(test_address, 3, Duration::from_secs(1)).await;
        assert!(result.is_err()); // Expected to fail as there is no server at this address
    }

    #[test]
    fn test_disconnect_node() {
        let network_manager = NetworkManager::new();
        let test_address: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        network_manager.connected_nodes.write().unwrap().insert(test_address);

        network_manager.disconnect_node(&test_address);
        assert!(!network_manager.connected_nodes.read().unwrap().contains(&test_address));
    }

    #[test]
    fn test_list_connected_nodes() {
        let network_manager = NetworkManager::new();
        let test_address: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        network_manager.connected_nodes.write().unwrap().insert(test_address);

        let nodes = network_manager.list_connected_nodes();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0], test_address);
    }
}
