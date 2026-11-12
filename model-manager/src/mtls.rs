use crate::config::Config;
use crate::error::{AppError, AppResult};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::ServerConfig;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

pub fn load_server_config(cfg: &Config) -> AppResult<Arc<ServerConfig>> {
    let cert_path = cfg
        .tls_cert
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("MODEL_MANAGER_TLS_CERT not set".into()))?;
    let key_path = cfg
        .tls_key
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("MODEL_MANAGER_TLS_KEY not set".into()))?;

    let cert_file = File::open(cert_path).map_err(|e| {
        AppError::BadRequest(format!("failed to open tls cert at {}: {e}", cert_path.display()))
    })?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::BadRequest(format!("failed to parse certs: {e}")))?;

    let key_file = File::open(key_path).map_err(|e| {
        AppError::BadRequest(format!("failed to open tls key at {}: {e}", key_path.display()))
    })?;
    let mut key_reader = BufReader::new(key_file);
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| AppError::BadRequest(format!("failed to parse private key: {e}")))?
        .ok_or_else(|| AppError::BadRequest("no private key found in key file".into()))?;

    let builder = ServerConfig::builder();

    let config = if let Some(ca_path) = &cfg.client_ca {
        let ca_file = File::open(ca_path).map_err(|e| {
            AppError::BadRequest(format!("failed to open client ca at {}: {e}", ca_path.display()))
        })?;
        let mut ca_reader = BufReader::new(ca_file);
        let ca_certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut ca_reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::BadRequest(format!("failed to parse client ca certs: {e}")))?;

        let mut roots = rustls::RootCertStore::empty();
        for ca in ca_certs {
            roots.add(ca).map_err(|e| AppError::BadRequest(format!("invalid client ca: {e}")))?;
        }

        let client_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| AppError::BadRequest(format!("failed to create client verifier: {e}")))?;

        builder
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(certs, key)
            .map_err(|e| AppError::BadRequest(format!("tls config error: {e}")))?
    } else {
        builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| AppError::BadRequest(format!("tls config error: {e}")))?
    };

    Ok(Arc::new(config))
}

pub fn create_tls_acceptor(cfg: &Config) -> AppResult<TlsAcceptor> {
    let server_config = load_server_config(cfg)?;
    Ok(TlsAcceptor::from(server_config))
}

pub async fn serve_tls(
    listener: tokio::net::TcpListener,
    acceptor: TlsAcceptor,
    app: axum::Router,
) -> anyhow::Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto;
    use hyper_util::service::TowerToHyperService;

    let auto = auto::Builder::new(TokioExecutor::new());

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(%err, "failed to accept TCP connection");
                continue;
            }
        };

        let acceptor = acceptor.clone();
        let app = app.clone();
        let auto = auto.clone();

        tokio::spawn(async move {
            match acceptor.accept(stream).await {
                Ok(tls_stream) => {
                    let io = TokioIo::new(tls_stream);
                    let service = TowerToHyperService::new(app);

                    if let Err(err) = auto.serve_connection(io, service).await {
                        tracing::debug!(%peer_addr, %err, "error serving TLS connection");
                    }
                }
                Err(err) => {
                    tracing::debug!(%peer_addr, %err, "failed TLS handshake");
                }
            }
        });
    }
}


