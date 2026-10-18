use super::identity;
use openssl::{
    pkey::PKey,
    ssl::{ErrorCode, Ssl, SslContextBuilder, SslMethod, SslStream, SslVerifyMode, SslVersion},
    x509::X509,
};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
};
#[derive(Default)]
pub struct Memory {
    incoming: VecDeque<u8>,
    outgoing: Vec<u8>,
}
impl Read for Memory {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        if self.incoming.is_empty() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = b.len().min(self.incoming.len());
        for x in &mut b[..n] {
            *x = self.incoming.pop_front().unwrap();
        }
        Ok(n)
    }
}
impl Write for Memory {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.outgoing.extend(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub struct Tls {
    stream: SslStream<Memory>,
    pub ready: bool,
}
impl Tls {
    pub fn new(cert: Option<&[u8]>, key: Option<&[u8]>) -> Result<Self, String> {
        Self::with_root(cert, key, identity::AAUTO_CERT.as_bytes())
    }
    fn with_root(cert: Option<&[u8]>, key: Option<&[u8]>, root: &[u8]) -> Result<Self, String> {
        let mut ctx = SslContextBuilder::new(SslMethod::tls_client()).map_err(|e| e.to_string())?;
        ctx.set_min_proto_version(Some(SslVersion::TLS1_2))
            .map_err(|e| e.to_string())?;
        ctx.set_max_proto_version(Some(SslVersion::TLS1_2))
            .map_err(|e| e.to_string())?;
        // Android Auto is a local accessory protocol, not Web PKI with DNS identities.
        // Verify the phone chain against Google's AA root; disable hostname matching.
        ctx.cert_store_mut()
            .add_cert(X509::from_pem(root).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        ctx.set_verify(SslVerifyMode::PEER);
        let cert = X509::from_pem(cert.unwrap_or(identity::CERTIFICATE.as_bytes()))
            .map_err(|e| e.to_string())?;
        ctx.set_certificate(&cert).map_err(|e| e.to_string())?;
        let key = PKey::private_key_from_pem(key.unwrap_or(identity::PRIVATE_KEY.as_bytes()))
            .map_err(|e| e.to_string())?;
        ctx.set_private_key(&key).map_err(|e| e.to_string())?;
        ctx.check_private_key().map_err(|e| e.to_string())?;
        let mut ssl = Ssl::new(&ctx.build()).map_err(|e| e.to_string())?;
        ssl.set_connect_state();
        Ok(Self {
            stream: SslStream::new(ssl, Memory::default()).map_err(|e| e.to_string())?,
            ready: false,
        })
    }
    pub fn handshake(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        self.stream.get_mut().incoming.extend(input);
        match self.stream.connect() {
            Ok(()) => {
                self.ready = true;
                tracing::debug!(cipher=?self.stream.ssl().current_cipher().map(|c|c.name()), "AA TLS cipher");
            }
            Err(e) if matches!(e.code(), ErrorCode::WANT_READ | ErrorCode::WANT_WRITE) => (),
            Err(e) => return Err(format!("AA TLS handshake: {e}")),
        }
        Ok(std::mem::take(&mut self.stream.get_mut().outgoing))
    }
    pub fn encrypt(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        if !self.ready {
            return Err("TLS not ready".into());
        }
        let mut offset = 0;
        while offset < input.len() {
            offset += self
                .stream
                .ssl_write(&input[offset..])
                .map_err(|e| e.to_string())?;
        }
        Ok(std::mem::take(&mut self.stream.get_mut().outgoing))
    }
    pub fn decrypt(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        if !self.ready {
            return Err("encrypted frame before authentication".into());
        }
        // Each encrypted AA frame contains complete TLS records. Partial records
        // cannot be reassigned to the next frame/channel.
        let mut offset = 0;
        while offset < input.len() {
            if input.len() - offset < 5 {
                return Err("truncated TLS record".into());
            }
            let n = u16::from_be_bytes([input[offset + 3], input[offset + 4]]) as usize;
            offset += 5 + n;
            if offset > input.len() {
                return Err("truncated TLS record body".into());
            }
        }
        self.stream.get_mut().incoming.extend(input);
        let mut result = Vec::new();
        let mut b = [0u8; 16384];
        loop {
            match self.stream.ssl_read(&mut b) {
                Ok(0) => return Err("TLS closed".into()),
                Ok(n) => result.extend(&b[..n]),
                Err(e) if e.code() == ErrorCode::WANT_READ => break,
                Err(e) => return Err(e.to_string()),
            }
        }
        if result.is_empty() {
            return Err("empty encrypted AA payload".into());
        }
        Ok(result)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use openssl::{
        asn1::Asn1Time,
        hash::MessageDigest,
        rsa::Rsa,
        ssl::SslContext,
        x509::{extension::BasicConstraints, X509NameBuilder},
    };
    pub fn pair() -> (Tls, SslStream<Memory>) {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "AA test phone").unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        cert.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let cert = cert.build();
        let mut ctx = SslContext::builder(SslMethod::tls_server()).unwrap();
        ctx.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
        ctx.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
        ctx.set_certificate(&cert).unwrap();
        ctx.set_private_key(&key).unwrap();
        let mut ssl = Ssl::new(&ctx.build()).unwrap();
        ssl.set_accept_state();
        let mut server = SslStream::new(ssl, Memory::default()).unwrap();
        let mut client = Tls::with_root(None, None, &cert.to_pem().unwrap()).unwrap();
        let mut reply = vec![];
        for _ in 0..20 {
            let output = client.handshake(&reply).unwrap();
            server.get_mut().incoming.extend(output);
            match server.accept() {
                Ok(()) => (),
                Err(e) if e.code() == ErrorCode::WANT_READ => (),
                Err(e) => panic!("{e}"),
            }
            reply = std::mem::take(&mut server.get_mut().outgoing);
            if client.ready && server.ssl().is_init_finished() {
                return (client, server);
            }
        }
        panic!("TLS handshake did not finish");
    }
    pub fn decode(server: &mut SslStream<Memory>, data: &[u8]) -> Vec<u8> {
        server.get_mut().incoming.extend(data);
        let mut b = [0; 16384];
        let n = server.ssl_read(&mut b).unwrap();
        b[..n].to_vec()
    }
    #[test]
    fn many_interleaved_records_keep_tls_counters_in_order() {
        let (mut client, mut server) = pair();
        for index in 0..32 {
            let data = client.encrypt(&[index; 64]).unwrap();
            server.get_mut().incoming.extend(data);
            let mut buffer = [0; 128];
            let n = server.ssl_read(&mut buffer).unwrap();
            assert_eq!(&buffer[..n], &[index; 64]);
            server.ssl_write(&[index; 32]).unwrap();
            let data = std::mem::take(&mut server.get_mut().outgoing);
            assert_eq!(client.decrypt(&data).unwrap(), vec![index; 32]);
        }
    }

    #[test]
    fn identity_and_tls_roundtrip_preserve_records() {
        let (mut client, mut server) = pair();
        let data = client.encrypt(b"media data").unwrap();
        server.get_mut().incoming.extend(data);
        let mut b = [0; 128];
        let n = server.ssl_read(&mut b).unwrap();
        assert_eq!(&b[..n], b"media data");
        server.ssl_write(b"reply").unwrap();
        let encrypted = std::mem::take(&mut server.get_mut().outgoing);
        assert_eq!(client.decrypt(&encrypted).unwrap(), b"reply");
        assert!(client.decrypt(&[23, 3, 3, 1, 0]).is_err());
    }
}
