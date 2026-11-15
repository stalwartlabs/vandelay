/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::io::{self, Read, Write};
use std::mem;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use rustls_platform_verifier::BuilderVerifierExt;
use url::Url;

use crate::error::Error;
use crate::net::http::TlsPolicy;

const SOCKET_TIMEOUT: Duration = Duration::from_secs(60);
const INFLATE_CHUNK: usize = 8192;
const INFLATE_RATIO: usize = 8;
const DEFLATE_SLACK: usize = 256;

#[derive(Debug)]
pub enum StreamError {
    Io(io::Error),
    Tls(String),
    CompressedStartTls,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Io(e) => write!(f, "transport error: {e}"),
            StreamError::Tls(m) => write!(f, "tls error: {m}"),
            StreamError::CompressedStartTls => {
                f.write_str("protocol error: cannot STARTTLS after COMPRESS DEFLATE")
            }
        }
    }
}

impl From<io::Error> for StreamError {
    fn from(e: io::Error) -> StreamError {
        StreamError::Io(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectMode {
    ImplicitTls,
    Plain,
    StartTls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleartext {
    Forbid,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlTls {
    Implicit,
    Negotiated,
}

#[derive(Debug, Clone, Copy)]
pub struct Scheme {
    pub plain: &'static str,
    pub tls: &'static str,
    pub plain_port: u16,
    pub tls_port: u16,
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: UrlTls,
}

impl Endpoint {
    pub fn parse(url: &str, scheme: &Scheme) -> Result<Endpoint, Error> {
        let parsed =
            Url::parse(url).map_err(|e| Error::Usage(format!("invalid --url {url:?}: {e}")))?;
        let tls = match parsed.scheme() {
            s if s == scheme.tls => UrlTls::Implicit,
            s if s == scheme.plain => UrlTls::Negotiated,
            other => {
                return Err(Error::Usage(format!(
                    "--url scheme must be {} or {}, got {other}",
                    scheme.plain, scheme.tls
                )));
            }
        };
        let host = parsed
            .host_str()
            .ok_or_else(|| Error::Usage(format!("--url missing host: {url}")))?
            .to_owned();
        let port = parsed.port().unwrap_or(match tls {
            UrlTls::Implicit => scheme.tls_port,
            UrlTls::Negotiated => scheme.plain_port,
        });
        Ok(Endpoint { host, port, tls })
    }

    pub fn session_url(&self, scheme: &Scheme) -> String {
        let name = match self.tls {
            UrlTls::Implicit => scheme.tls,
            UrlTls::Negotiated => scheme.plain,
        };
        format!("{name}://{}:{}", self.host, self.port)
    }
}

pub struct Connector {
    config: Arc<ClientConfig>,
}

impl Connector {
    pub fn new(tls: TlsPolicy) -> Result<Connector, StreamError> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let builder = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| StreamError::Tls(format!("rustls protocol versions: {e}")))?;
        let config = match tls {
            TlsPolicy::AcceptInvalid => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth(),
            TlsPolicy::Verify => builder
                .with_platform_verifier()
                .map_err(|e| StreamError::Tls(format!("rustls platform verifier: {e}")))?
                .with_no_client_auth(),
        };
        Ok(Connector {
            config: Arc::new(config),
        })
    }

    pub fn connect_plain(&self, host: &str, port: u16) -> Result<Stream, StreamError> {
        Ok(Stream::Tcp(Connector::tcp(host, port)?))
    }

    pub fn connect_tls(&self, host: &str, port: u16) -> Result<Stream, StreamError> {
        let tcp = Connector::tcp(host, port)?;
        self.wrap_tls(tcp, host)
    }

    fn tcp(host: &str, port: u16) -> Result<TcpStream, StreamError> {
        let tcp = TcpStream::connect((host, port))?;
        tcp.set_read_timeout(Some(SOCKET_TIMEOUT))?;
        tcp.set_write_timeout(Some(SOCKET_TIMEOUT))?;
        tcp.set_nodelay(true)?;
        Ok(tcp)
    }

    fn wrap_tls(&self, tcp: TcpStream, host: &str) -> Result<Stream, StreamError> {
        let server_name = ServerName::try_from(host.to_owned())
            .map_err(|e| StreamError::Tls(format!("invalid server name {host}: {e}")))?;
        let conn = ClientConnection::new(self.config.clone(), server_name)
            .map_err(|e| StreamError::Tls(format!("rustls client: {e}")))?;
        Ok(Stream::Tls(Box::new(StreamOwned::new(conn, tcp))))
    }
}

pub enum Stream {
    Tcp(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
    Deflate(Box<Deflate>),
    Closed,
    #[cfg(test)]
    Mock(Box<MockStream>),
}

impl Stream {
    pub fn is_tls(&self) -> bool {
        match self {
            Stream::Tls(_) => true,
            Stream::Deflate(deflate) => deflate.inner.is_tls(),
            Stream::Tcp(_) | Stream::Closed => false,
            #[cfg(test)]
            Stream::Mock(_) => false,
        }
    }

    pub fn take(&mut self) -> Stream {
        mem::replace(self, Stream::Closed)
    }

    pub fn upgrade_tls(self, connector: &Connector, host: &str) -> Result<Stream, StreamError> {
        match self {
            Stream::Tcp(tcp) => connector.wrap_tls(tcp, host),
            Stream::Deflate(_) => Err(StreamError::CompressedStartTls),
            other => Ok(other),
        }
    }

    pub fn compress(self, primer: &[u8]) -> Stream {
        Stream::Deflate(Box::new(Deflate {
            inner: self,
            decompress: Decompress::new(false),
            compress: Compress::new(Compression::default(), false),
            rx_raw: primer.to_vec(),
            rx_raw_pos: 0,
            rx_decoded: Vec::new(),
            rx_decoded_pos: 0,
            tx: Vec::new(),
        }))
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(tcp) => tcp.read(buf),
            Stream::Tls(tls) => tls.read(buf),
            Stream::Deflate(deflate) => deflate.read(buf),
            Stream::Closed => Ok(0),
            #[cfg(test)]
            Stream::Mock(mock) => mock.reader.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(tcp) => tcp.write(buf),
            Stream::Tls(tls) => tls.write(buf),
            Stream::Deflate(deflate) => deflate.write(buf),
            Stream::Closed => Ok(buf.len()),
            #[cfg(test)]
            Stream::Mock(mock) => {
                mock.written.extend_from_slice(buf);
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(tcp) => tcp.flush(),
            Stream::Tls(tls) => tls.flush(),
            Stream::Deflate(deflate) => deflate.inner.flush(),
            Stream::Closed => Ok(()),
            #[cfg(test)]
            Stream::Mock(_) => Ok(()),
        }
    }
}

pub struct Deflate {
    inner: Stream,
    decompress: Decompress,
    compress: Compress,
    rx_raw: Vec<u8>,
    rx_raw_pos: usize,
    rx_decoded: Vec<u8>,
    rx_decoded_pos: usize,
    tx: Vec<u8>,
}

impl Read for Deflate {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(mut avail) = self.rx_decoded.get(self.rx_decoded_pos..)
                && !avail.is_empty()
            {
                let n = avail.read(out)?;
                self.rx_decoded_pos += n;
                return Ok(n);
            }
            if self.rx_raw_pos >= self.rx_raw.len() {
                self.rx_raw.clear();
                self.rx_raw_pos = 0;
                self.rx_raw.resize(INFLATE_CHUNK, 0);
                let n = self.inner.read(&mut self.rx_raw)?;
                if n == 0 {
                    return Ok(0);
                }
                self.rx_raw.truncate(n);
            }
            self.rx_decoded.clear();
            self.rx_decoded_pos = 0;
            self.rx_decoded
                .resize(out.len().max(INFLATE_CHUNK) * INFLATE_RATIO, 0);
            let in_before = self.decompress.total_in();
            let out_before = self.decompress.total_out();
            self.decompress
                .decompress(
                    self.rx_raw.get(self.rx_raw_pos..).unwrap_or_default(),
                    &mut self.rx_decoded,
                    FlushDecompress::None,
                )
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let consumed = (self.decompress.total_in() - in_before) as usize;
            let produced = (self.decompress.total_out() - out_before) as usize;
            self.rx_raw_pos += consumed;
            self.rx_decoded.truncate(produced);
            if produced == 0 && consumed == 0 {
                self.rx_raw.clear();
                self.rx_raw_pos = 0;
            }
        }
    }
}

impl Write for Deflate {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx.clear();
        self.tx.resize(buf.len() + DEFLATE_SLACK, 0);
        let in_before = self.compress.total_in();
        let out_before = self.compress.total_out();
        self.compress
            .compress(buf, &mut self.tx, FlushCompress::Sync)
            .map_err(io::Error::other)?;
        let consumed = (self.compress.total_in() - in_before) as usize;
        let produced = (self.compress.total_out() - out_before) as usize;
        self.inner
            .write_all(self.tx.get(..produced).unwrap_or_default())?;
        Ok(consumed)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
pub struct MockStream {
    reader: io::Cursor<Vec<u8>>,
    written: Vec<u8>,
}

#[cfg(test)]
impl Stream {
    pub fn mock(server_says: &[u8]) -> Stream {
        Stream::Mock(Box::new(MockStream {
            reader: io::Cursor::new(server_says.to_vec()),
            written: Vec::new(),
        }))
    }

    pub fn written(&self) -> &[u8] {
        match self {
            Stream::Mock(mock) => &mock.written,
            _ => &[],
        }
    }
}

#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAP: Scheme = Scheme {
        plain: "imap",
        tls: "imaps",
        plain_port: 143,
        tls_port: 993,
    };

    fn deflated(plaintext: &[u8]) -> Vec<u8> {
        let mut compressor = Compress::new(Compression::default(), false);
        let mut out = vec![0u8; plaintext.len() + 256];
        compressor
            .compress(plaintext, &mut out, FlushCompress::Sync)
            .unwrap();
        out.truncate(compressor.total_out() as usize);
        out
    }

    fn read_all(stream: &mut Stream, want: usize) -> Vec<u8> {
        let mut got = vec![0u8; 256];
        let mut total = 0usize;
        while total < want {
            let n = stream.read(&mut got[total..]).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        got.truncate(total);
        got
    }

    #[test]
    fn connector_constructs_with_either_policy() {
        Connector::new(TlsPolicy::Verify).unwrap();
        Connector::new(TlsPolicy::AcceptInvalid).unwrap();
    }

    #[test]
    fn deflate_writes_round_trip_through_peer_decompressor() {
        let mut stream = Stream::mock(b"").compress(b"");
        stream.write_all(b"hello world").unwrap();
        stream.write_all(b" again").unwrap();
        stream.flush().unwrap();
        let Stream::Deflate(deflate) = &stream else {
            panic!("not deflate");
        };
        let mut inflater = Decompress::new(false);
        let mut plain = vec![0u8; 64];
        inflater
            .decompress(deflate.inner.written(), &mut plain, FlushDecompress::Sync)
            .unwrap();
        plain.truncate(inflater.total_out() as usize);
        assert_eq!(plain, b"hello world again");
    }

    #[test]
    fn deflate_reads_decompressed_payload_from_inner() {
        let plaintext = b"* OK [CAPABILITY IMAP4rev2 COMPRESS=DEFLATE] hi\r\n";
        let mut stream = Stream::mock(&deflated(plaintext)).compress(b"");
        assert_eq!(read_all(&mut stream, plaintext.len()), plaintext);
    }

    #[test]
    fn deflate_consumes_primer_before_reading_inner() {
        let plaintext = b"primed payload\r\n";
        let mut stream = Stream::mock(b"").compress(&deflated(plaintext));
        assert_eq!(read_all(&mut stream, plaintext.len()), plaintext);
    }

    #[test]
    fn starttls_after_compress_is_refused() {
        let connector = Connector::new(TlsPolicy::AcceptInvalid).unwrap();
        let err = Stream::mock(b"")
            .compress(b"")
            .upgrade_tls(&connector, "example.com")
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "protocol error: cannot STARTTLS after COMPRESS DEFLATE"
        );
    }

    #[test]
    fn endpoint_defaults_port_per_scheme() {
        let e = Endpoint::parse("imaps://mail.example.com", &IMAP).unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.tls),
            ("mail.example.com", 993, UrlTls::Implicit)
        );
        let e = Endpoint::parse("imap://mail.example.com", &IMAP).unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.tls),
            ("mail.example.com", 143, UrlTls::Negotiated)
        );
        let e = Endpoint::parse("imaps://mail.example.com:1993", &IMAP).unwrap();
        assert_eq!(e.port, 1993);
        assert_eq!(e.session_url(&IMAP), "imaps://mail.example.com:1993");
    }

    #[test]
    fn endpoint_rejects_other_schemes_and_missing_host() {
        let err = Endpoint::parse("http://example.com", &IMAP).unwrap_err();
        assert!(
            matches!(&err, Error::Usage(m) if m == "--url scheme must be imap or imaps, got http"),
            "{err:?}"
        );
        assert!(matches!(
            Endpoint::parse("imap:///path", &IMAP),
            Err(Error::Usage(_))
        ));
    }
}
