//! Networking utilities for establishing TLS connections between nodes.
//!
//! The [`NetworkManager`] provides retrying connection logic and tracks peers that
//! are currently connected. It is primarily used by higher level coordination
//! components to ensure reliable communication between nodes.

use crate::error::AnKiError;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout, Duration};
use tokio_rustls::{
    rustls::{ClientConfig, ServerName},
    TlsConnector,
};
use tracing::{error, info};

/// Manages outbound connections to other nodes and maintains a set of peers that
/// are currently reachable.
#[derive(Clone, Debug)]
pub struct NetworkManager {
    /// Addresses of nodes that have been successfully contacted.
    pub connected_nodes: Arc<RwLock<HashSet<SocketAddr>>>,
    /// TLS configuration used when connecting to remote nodes.
    tls_config: Arc<ClientConfig>,
}

impl NetworkManager {
    /// Creates a new [`NetworkManager`] using the provided TLS client configuration.
    ///
    /// # Parameters
    /// * `tls_config` - TLS settings used for all outbound connections.
    pub fn new(tls_config: ClientConfig) -> Self {
        NetworkManager {
            connected_nodes: Arc::new(RwLock::new(HashSet::new())),
            tls_config: Arc::new(tls_config),
        }
    }

    /// Attempts to establish a TLS connection to `address`.
    ///
    /// # Parameters
    /// * `address` - Socket address of the remote node.
    /// * `domain` - DNS name expected in the remote TLS certificate.
    /// * `retry_count` - Number of times to retry before giving up.
    /// * `timeout_duration` - How long to wait for each connection attempt.
    /// * `base_delay` - Initial delay before retrying.
    /// * `max_backoff` - Maximum delay between retries.
    ///
    /// # Errors
    /// Returns an error if a connection cannot be established after all retries
    /// or if TLS negotiation fails.
    pub async fn connect_to_node(
        &self,
        address: SocketAddr,
        domain: &str,
        retry_count: u32,
        timeout_duration: Duration,
        base_delay: Duration,
        max_backoff: Duration,
    ) -> Result<(), AnKiError> {
        for attempt in 0..retry_count {
            let connector = TlsConnector::from(self.tls_config.clone());
            let domain_name =
                ServerName::try_from(domain).map_err(|e| AnKiError::Network(e.to_string()))?;
            let fut = async {
                let tcp = TcpStream::connect(address)
                    .await
                    .map_err(|e| AnKiError::Network(e.to_string()))?;
                let _tls = connector
                    .connect(domain_name, tcp)
                    .await
                    .map_err(|e| AnKiError::Network(e.to_string()))?;
                Ok::<(), AnKiError>(())
            };

            match timeout(timeout_duration, fut).await {
                Ok(Ok(())) => {
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

            if attempt + 1 < retry_count {
                let exp_delay = base_delay.checked_mul(1 << attempt).unwrap_or(max_backoff);
                let delay = std::cmp::min(exp_delay, max_backoff);
                sleep(delay).await;
            }
        }
        Err(AnKiError::Network("Failed to connect after retries".into()))
    }

    /// Removes a node from the tracked set of connected peers.
    ///
    /// # Parameters
    /// * `address` - Address of the node to disconnect.
    pub async fn disconnect_node(&self, address: &SocketAddr) {
        let mut nodes = self.connected_nodes.write().await;
        if nodes.remove(address) {
            info!("Disconnected from node at: {}", address);
        } else {
            error!("Node not found for disconnection: {}", address);
        }
    }

    /// Returns a list of currently connected node addresses.
    pub async fn list_connected_nodes(&self) -> Vec<SocketAddr> {
        let nodes = self.connected_nodes.read().await;
        nodes.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, Certificate as RcCert, CertificateParams, ExtendedKeyUsagePurpose, IsCa,
    };
    use rustls::server::AllowAnyAuthenticatedClient;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::time::Duration;
    use tokio_rustls::{
        rustls::{
            Certificate as RustlsCert, ClientConfig, PrivateKey, RootCertStore, ServerConfig,
        },
        TlsAcceptor,
    };

    fn build_configs() -> (ClientConfig, ServerConfig) {
        // CA
        let mut ca_params = CertificateParams::new(vec!["ca".into()]);
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = RcCert::from_params(ca_params).unwrap();
        let ca_der = ca.serialize_der().unwrap();
        let ca_cert = RustlsCert(ca_der.clone());

        // server cert
        let mut server_params = CertificateParams::new(vec!["localhost".into()]);
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server = RcCert::from_params(server_params).unwrap();
        let server_der = server.serialize_der_with_signer(&ca).unwrap();
        let server_cert = RustlsCert(server_der);
        let server_key = PrivateKey(server.serialize_private_key_der());

        // client cert
        let mut client_params = CertificateParams::new(vec!["client".into()]);
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client = RcCert::from_params(client_params).unwrap();
        let client_der = client.serialize_der_with_signer(&ca).unwrap();
        let client_cert = RustlsCert(client_der);
        let client_key = PrivateKey(client.serialize_private_key_der());

        // client config
        let mut root = RootCertStore::empty();
        root.add(&ca_cert).unwrap();
        let client_config = ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root)
            .with_client_auth_cert(vec![client_cert], client_key)
            .unwrap();

        // server config with client auth
        let mut client_root = RootCertStore::empty();
        client_root.add(&ca_cert).unwrap();
        let client_auth = AllowAnyAuthenticatedClient::new(client_root);
        let server_config = ServerConfig::builder()
            .with_safe_defaults()
            .with_client_cert_verifier(Arc::new(client_auth))
            .with_single_cert(vec![server_cert], server_key)
            .unwrap();

        (client_config, server_config)
    }

    #[tokio::test]
    async fn test_connect_to_node() {
        let (client_cfg, server_cfg) = build_configs();
        let network_manager = NetworkManager::new(client_cfg);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let test_address = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_cfg));

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _ = acceptor.accept(stream).await;
            }
        });

        // Successful connection while listener is active
        let result = network_manager
            .connect_to_node(
                test_address,
                "localhost",
                3,
                Duration::from_secs(3),
                Duration::from_millis(10),
                Duration::from_millis(80),
            )
            .await;
        assert!(result.is_ok());

        // Drop listener and ensure connection now fails
        // No server running on same address
        let result = network_manager
            .connect_to_node(
                test_address,
                "localhost",
                1,
                Duration::from_secs(1),
                Duration::from_millis(10),
                Duration::from_millis(80),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_disconnect_node() {
        let (client_cfg, _) = build_configs();
        let network_manager = NetworkManager::new(client_cfg);
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
        let (client_cfg, _) = build_configs();
        let network_manager = NetworkManager::new(client_cfg);
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
