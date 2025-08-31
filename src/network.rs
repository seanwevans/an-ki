// network.rs: Abstracts network operations such as connecting nodes and handling retries.

use std::collections::HashSet;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use tracing::{error, info};

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

    pub async fn connect_to_node(
        &self,
        address: SocketAddr,
        retry_count: u32,
        timeout_duration: Duration,
    ) -> Result<(), Box<dyn Error>> {
        for attempt in 0..retry_count {
            match timeout(timeout_duration, TcpStream::connect(address)).await {
                Ok(Ok(_stream)) => {
                    info!("Successfully connected to node at: {}", address);
                    self.connected_nodes.write().await.insert(address);
                    return Ok(());
                }
                Ok(Err(e)) => {
                    error!(
                        "Failed to connect to node at {}: {}. Attempt {}/{}",
                        address,
                        e,
                        attempt + 1,
                        retry_count
                    );
                }
                Err(_) => {
                    error!(
                        "Connection to node at {} timed out. Attempt {}/{}",
                        address,
                        attempt + 1,
                        retry_count
                    );
                }
            }
        }
        Err("Failed to connect after retries".into())
    }

    pub async fn disconnect_node(&self, address: &SocketAddr) {
        let mut nodes = self.connected_nodes.write().await;
        if nodes.remove(address) {
            info!("Disconnected from node at: {}", address);
        } else {
            error!("Node not found for disconnection: {}", address);
        }
    }

    pub async fn list_connected_nodes(&self) -> Vec<SocketAddr> {
        let nodes = self.connected_nodes.read().await;
        nodes.iter().cloned().collect()
    }
}

impl Default for NetworkManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_connect_to_node() {
        let network_manager = NetworkManager::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let test_address = listener.local_addr().unwrap();

        // Successful connection while listener is active
        let result = network_manager
            .connect_to_node(test_address, 3, Duration::from_secs(1))
            .await;
        assert!(result.is_ok());

        // Drop listener and ensure connection now fails
        drop(listener);
        let result = network_manager
            .connect_to_node(test_address, 1, Duration::from_secs(1))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_disconnect_node() {
        let network_manager = NetworkManager::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let test_address = listener.local_addr().unwrap();
        drop(listener);
        network_manager
            .connected_nodes
            .write()
            .await
            .insert(test_address);

        network_manager.disconnect_node(&test_address).await;
        assert!(!network_manager
            .connected_nodes
            .read()
            .await
            .contains(&test_address));
    }

    #[tokio::test]
    async fn test_list_connected_nodes() {
        let network_manager = NetworkManager::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let test_address = listener.local_addr().unwrap();
        drop(listener);
        network_manager
            .connected_nodes
            .write()
            .await
            .insert(test_address);

        let nodes = network_manager.list_connected_nodes().await;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0], test_address);
    }
}
