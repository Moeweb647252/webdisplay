use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::rustls;

pub fn load_certs(path: &Path) -> std::io::Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    Ok(certs)
}

pub fn load_key(path: &Path) -> std::io::Result<PrivateKeyDer<'static>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let key = rustls_pemfile::private_key(&mut reader)?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No private key found"))?;
    Ok(key)
}

pub fn generate_self_signed_cert() -> Result<(), Box<dyn std::error::Error>> {
    if Path::new("cert.pem").exists() && Path::new("key.pem").exists() {
        return Ok(());
    }

    log::info!("正在生成自签名证书...");
    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let cert = rcgen::generate_simple_self_signed(subject_alt_names)?;

    let mut cert_file = File::create("cert.pem")?;
    cert_file.write_all(cert.cert.pem().as_bytes())?;

    let mut key_file = File::create("key.pem")?;
    key_file.write_all(cert.signing_key.serialize_pem().as_bytes())?;

    log::info!("自签名证书已生成 (cert.pem, key.pem)");
    Ok(())
}

pub fn get_tls_config() -> Result<Arc<rustls::ServerConfig>, Box<dyn std::error::Error>> {
    generate_self_signed_cert()?;

    let certs = load_certs(Path::new("cert.pem"))?;
    let key = load_key(Path::new("key.pem"))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(Arc::new(config))
}
