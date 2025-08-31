// network.rs: Abstracts network operations such as connecting nodes and handling retries.

use std::collections::HashSet;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use tracing::{error, info};
use std::collections::HashSet;
use tokio_rustls::{TlsConnector, rustls::{ClientConfig, ServerName}};

#[derive(Clone, Debug)]
pub struct NetworkManager {
    pub connected_nodes: Arc<RwLock<HashSet<SocketAddr>>>,
    tls_config: Arc<ClientConfig>,
}

impl Default for NetworkManager {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkManager {
    pub fn new(tls_config: ClientConfig) -> Self {
        NetworkManager {
            connected_nodes: Arc::new(RwLock::new(HashSet::new())),
            tls_config: Arc::new(tls_config),
        }
    }

    pub async fn connect_to_node(
        &self,
        address: SocketAddr,
        retry_count: u32,
        timeout_duration: Duration,
    ) -> Result<(), Box<dyn Error>> {
        for attempt in 0..retry_count {
            let connector = TlsConnector::from(self.tls_config.clone());
            let domain_name = ServerName::try_from(domain).map_err(|e| format!("{e:?}"))?;
            let fut = async {
                let tcp = TcpStream::connect(address).await?;
                let _tls = connector.connect(domain_name, tcp).await?;
                Ok::<(), Box<dyn Error>>(())
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
    use tokio_rustls::{TlsAcceptor, rustls::{ClientConfig, ServerConfig, Certificate as RustlsCert, PrivateKey, RootCertStore}};
    use rustls::server::AllowAnyAuthenticatedClient;
    use rcgen::{Certificate as RcCert, CertificateParams, IsCa, BasicConstraints, ExtendedKeyUsagePurpose};
    use std::sync::Arc;

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
            .with_single_cert(vec![client_cert], client_key)
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
            .connect_to_node(test_address, "localhost", 3, Duration::from_secs(3))
            .await;
        assert!(result.is_ok());

        // Drop listener and ensure connection now fails
        // No server running on same address
        let result = network_manager
            .connect_to_node(test_address, "localhost", 1, Duration::from_secs(1))
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
