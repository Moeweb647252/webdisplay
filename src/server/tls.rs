use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::rustls;

const CERT_VERSION_MARKER_FILE: &str = "cert.version";
const REQUIRED_CERT_VERSION: &str = "2";

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
        if let Ok(version) = fs::read_to_string(CERT_VERSION_MARKER_FILE) {
            if version.trim() == REQUIRED_CERT_VERSION {
                return Ok(());
            }
        }
    }

    // WebTransport 的 serverCertificateHashes 仅支持短期证书（<= 14 天），
    // 因此这里在启动时统一生成短期开发证书。
    log::info!("正在生成自签名证书...");
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])?;
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    params.not_after = params.not_before + time::Duration::days(13);
    params.insert_extended_key_usage(rcgen::ExtendedKeyUsagePurpose::ServerAuth);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");

    let signing_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let cert = params.self_signed(&signing_key)?;

    let mut cert_file = File::create("cert.pem")?;
    cert_file.write_all(cert.pem().as_bytes())?;

    let mut key_file = File::create("key.pem")?;
    key_file.write_all(signing_key.serialize_pem().as_bytes())?;

    fs::write(CERT_VERSION_MARKER_FILE, REQUIRED_CERT_VERSION)?;

    log::info!("自签名证书已生成 (cert.pem, key.pem)");
    Ok(())
}

pub fn get_webtransport_certificate_hash_sha256() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let certs = load_certs(Path::new("cert.pem"))?;
    let leaf = certs.first().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "cert.pem 中没有证书")
    })?;

    let mut hasher = Sha256::new();
    hasher.update(leaf.as_ref());
    Ok(hasher.finalize().to_vec())
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
