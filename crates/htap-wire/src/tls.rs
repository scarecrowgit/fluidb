//! Connection abstraction wrapping a plain [`TcpStream`] or a TLS stream.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

/// TLS certificate resolver whose active key can be atomically replaced for new handshakes.
#[derive(Debug)]
pub struct ReloadableCertResolver {
    current: RwLock<Arc<rustls::sign::CertifiedKey>>,
}

impl ReloadableCertResolver {
    /// Creates a resolver serving `current`.
    pub fn new(current: rustls::sign::CertifiedKey) -> Self {
        Self {
            current: RwLock::new(Arc::new(current)),
        }
    }

    /// Loads and validates a PEM certificate chain and private key, including that they match.
    pub fn load(cert_path: &Path, key_path: &Path) -> io::Result<rustls::sign::CertifiedKey> {
        let cert_file = std::fs::File::open(cert_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "failed to open TLS certificate '{}': {e}",
                    cert_path.display()
                ),
            )
        })?;
        let mut cert_reader = io::BufReader::new(cert_file);
        let certs = rustls_pemfile::certs(&mut cert_reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid TLS certificate PEM: {e}"),
                )
            })?;
        if certs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TLS certificate PEM contains no certificates",
            ));
        }

        let key_file = std::fs::File::open(key_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "failed to open TLS private key '{}': {e}",
                    key_path.display()
                ),
            )
        })?;
        let mut key_reader = io::BufReader::new(key_file);
        let key = rustls_pemfile::private_key(&mut key_reader)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid TLS private-key PEM: {e}"),
                )
            })?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "TLS private-key PEM contains no private key",
                )
            })?;
        let provider = rustls::crypto::ring::default_provider();
        let signing_key = provider.key_provider.load_private_key(key).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid TLS private key: {e}"),
            )
        })?;

        let certified_key = rustls::sign::CertifiedKey::new(certs, signing_key);
        match certified_key.keys_match() {
            Ok(()) => {}
            Err(rustls::Error::InconsistentKeys(rustls::InconsistentKeys::KeyMismatch)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "certificate and private key do not match",
                ));
            }
            // Some providers cannot expose a public key to compare; accept that case.
            Err(rustls::Error::InconsistentKeys(rustls::InconsistentKeys::Unknown)) => {}
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to validate TLS certificate and private key: {e}"),
                ));
            }
        }

        Ok(certified_key)
    }

    /// Atomically replaces the certificate served to future TLS handshakes.
    ///
    /// The current certificate remains active if loading or validation fails.
    pub fn reload(&self, cert_path: &Path, key_path: &Path) -> io::Result<()> {
        let loaded = Arc::new(Self::load(cert_path, key_path)?);
        *self.current.write() = loaded;
        Ok(())
    }
}

impl rustls::server::ResolvesServerCert for ReloadableCertResolver {
    fn resolve(
        &self,
        _client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(Arc::clone(&self.current.read()))
    }
}

/// A connection that is either a plain TCP socket or a TLS-wrapped one.
#[derive(Debug)]
pub enum Conn {
    /// Unencrypted TCP connection.
    Plain(TcpStream),
    /// Server-side TLS connection backed by a plain TCP socket.
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
    /// Client-side TLS connection backed by a plain TCP socket.
    TlsClient(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Conn {
    /// Returns a reference to the underlying [`TcpStream`] for socket-level operations
    /// (`set_read_timeout`, `try_clone`, `shutdown`, …) that are not available on a generic
    /// `Read + Write`.
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Conn::Plain(s) => s,
            Conn::Tls(s) => &s.sock,
            Conn::TlsClient(s) => &s.sock,
        }
    }
}

/// Completes a client-side TLS handshake over `sock`.
pub fn perform_client_tls_handshake(
    mut sock: TcpStream,
    mut conn: rustls::ClientConnection,
) -> io::Result<Conn> {
    while conn.is_handshaking() {
        conn.complete_io(&mut sock)?;
    }
    Ok(Conn::TlsClient(Box::new(rustls::StreamOwned::new(
        conn, sock,
    ))))
}

/// Completes a server-side TLS handshake over `sock`.
pub fn perform_tls_handshake(
    sock: &mut TcpStream,
    mut conn: rustls::ServerConnection,
    stop: &AtomicBool,
) -> io::Result<Conn> {
    while conn.is_handshaking() {
        match conn.complete_io(sock) {
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if stop.load(Ordering::SeqCst) {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "server shutdown requested",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(Conn::Tls(Box::new(rustls::StreamOwned::new(
        conn,
        sock.try_clone()?,
    ))))
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
            Conn::TlsClient(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
            Conn::TlsClient(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
            Conn::TlsClient(s) => s.flush(),
        }
    }
}
