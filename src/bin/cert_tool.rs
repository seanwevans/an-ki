use clap::{Parser, Subcommand};
use rcgen::{BasicConstraints, Certificate, CertificateParams, ExtendedKeyUsagePurpose, IsCa};
use std::error::Error;
use std::fs;

#[derive(Parser)]
#[command(name = "cert-tool", about = "Generate node certificates for AN-KI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate a root certificate authority
    GenerateCa {
        #[arg(long)]
        cert: String,
        #[arg(long)]
        key: String,
    },
    /// Generate a node certificate signed by the CA
    GenerateNode {
        #[arg(long)]
        ca_cert: String,
        #[arg(long)]
        ca_key: String,
        #[arg(long)]
        node_id: String,
        #[arg(long)]
        cert: String,
        #[arg(long)]
        key: String,
    },
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    match cli.command {
        Commands::GenerateCa { cert, key } => {
            let mut params = CertificateParams::new(vec!["an-ki-ca".into()]);
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            let ca = Certificate::from_params(params)?;
            fs::write(cert, ca.serialize_pem()?)?;
            fs::write(key, ca.serialize_private_key_pem())?;
        }
        Commands::GenerateNode {
            ca_cert,
            ca_key,
            node_id,
            cert,
            key,
        } => {
            let ca_cert_pem = fs::read_to_string(ca_cert)?;
            let ca_key_pem = fs::read_to_string(ca_key)?;
            let key_pair = rcgen::KeyPair::from_pem(&ca_key_pem)?;
            let ca_params = CertificateParams::from_ca_cert_pem(&ca_cert_pem, key_pair)?;
            let ca_cert = Certificate::from_params(ca_params)?;
            let mut params = CertificateParams::new(vec![node_id]);
            params.extended_key_usages = vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ];
            let node = Certificate::from_params(params)?;
            fs::write(cert, node.serialize_pem_with_signer(&ca_cert)?)?;
            fs::write(key, node.serialize_private_key_pem())?;
        }
    }
    Ok(())
}
