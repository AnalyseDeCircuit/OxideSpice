//! SPICE SASL negotiation and optional post-authentication security framing.

use rsasl::callback::{Context, Request, SessionCallback, SessionData};
use rsasl::mechanisms::gssapi::GSSAPI;
use rsasl::mechanisms::gssapi::properties::{GssSecurityLayer, GssService, SecurityLayer};
use rsasl::mechanisms::login::LOGIN;
use rsasl::mechanisms::plain::PLAIN;
use rsasl::mechanisms::scram::{SCRAM_SHA1, SCRAM_SHA256, SCRAM_SHA512};
use rsasl::prelude::{
    Mechanism, Mechname, Registry, SASLClient, SASLConfig, Session, SessionError, State,
};
use rsasl::property::{AuthId, AuthzId, Hostname, Password};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

use crate::ClientError;

pub(crate) const SASL_MAX_DATA_BYTES: usize = 1024 * 1024;
pub(crate) const SASL_SECURITY_PLAINTEXT_BYTES: usize = 8192;

/// Password credentials for SASL mechanisms that do not use the system Kerberos cache.
#[derive(Clone)]
pub struct SaslCredentials {
    authentication_id: String,
    authorization_id: Option<String>,
    password: Zeroizing<String>,
}

impl SaslCredentials {
    pub fn new(authentication_id: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            authentication_id: authentication_id.into(),
            authorization_id: None,
            password: Zeroizing::new(password.into()),
        }
    }

    pub fn with_authorization_id(mut self, authorization_id: impl Into<String>) -> Self {
        self.authorization_id = Some(authorization_id.into());
        self
    }
}

impl std::fmt::Debug for SaslCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SaslCredentials")
            .field("authentication_id", &self.authentication_id)
            .field("authorization_id", &self.authorization_id)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

/// Caller-selected SASL identity and mechanism family.
#[derive(Clone)]
pub struct SaslOptions {
    pub hostname: String,
    pub service: String,
    pub allow_gssapi: bool,
    pub credentials: Option<SaslCredentials>,
}

impl SaslOptions {
    pub fn gssapi(hostname: impl Into<String>) -> Self {
        Self {
            hostname: hostname.into(),
            service: "spice".to_owned(),
            allow_gssapi: true,
            credentials: None,
        }
    }

    pub fn with_credentials(hostname: impl Into<String>, credentials: SaslCredentials) -> Self {
        Self {
            hostname: hostname.into(),
            service: "spice".to_owned(),
            allow_gssapi: false,
            credentials: Some(credentials),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ClientError> {
        if self.hostname.is_empty() || self.service.is_empty() {
            return Err(ClientError::Configuration(
                "SASL hostname and service must not be empty",
            ));
        }
        if !self.allow_gssapi && self.credentials.is_none() {
            return Err(ClientError::Configuration(
                "SASL requires credentials or GSSAPI",
            ));
        }
        if self
            .credentials
            .as_ref()
            .is_some_and(|credentials| credentials.authentication_id.is_empty())
        {
            return Err(ClientError::Configuration(
                "SASL authentication id must not be empty",
            ));
        }
        Ok(())
    }
}

impl std::fmt::Debug for SaslOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SaslOptions")
            .field("hostname", &self.hostname)
            .field("service", &self.service)
            .field("allow_gssapi", &self.allow_gssapi)
            .field("credentials", &self.credentials)
            .finish()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SaslParameters<'a> {
    pub options: &'a SaslOptions,
    pub require_security_layer: bool,
}

struct SaslCallback {
    hostname: String,
    service: String,
    authentication_id: Option<String>,
    authorization_id: Option<String>,
    password: Option<Zeroizing<String>>,
    security_layers: SecurityLayer,
}

impl SessionCallback for SaslCallback {
    fn callback(
        &self,
        _session_data: &SessionData,
        _context: &Context,
        request: &mut Request<'_>,
    ) -> Result<(), SessionError> {
        if let Some(authentication_id) = self.authentication_id.as_deref() {
            request.satisfy::<AuthId>(authentication_id)?;
        }
        if let Some(authorization_id) = self.authorization_id.as_deref() {
            request.satisfy::<AuthzId>(authorization_id)?;
        }
        if let Some(password) = self.password.as_deref() {
            request.satisfy::<Password>(password.as_bytes())?;
        }
        request
            .satisfy::<Hostname>(&self.hostname)?
            .satisfy::<GssService>(&self.service)?
            .satisfy::<GssSecurityLayer>(&self.security_layers)?;
        Ok(())
    }
}

/// Completed SASL mechanism retained only when it negotiated a security layer.
pub(crate) struct SaslCodec {
    session: Session,
}

impl SaslCodec {
    pub(crate) fn encode(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, ClientError> {
        let mut token = Vec::new();
        let consumed = self
            .session
            .encode(plaintext, &mut token)
            .map_err(|error| ClientError::Sasl(error.to_string()))?;
        if consumed != plaintext.len() || token.is_empty() || token.len() > SASL_MAX_DATA_BYTES {
            return Err(ClientError::Sasl(
                "invalid SASL security-layer output length".to_owned(),
            ));
        }
        let token_length = u32::try_from(token.len())
            .map_err(|_| ClientError::Sasl("SASL token length overflow".to_owned()))?;
        let mut framed = Vec::with_capacity(4 + token.len());
        framed.extend_from_slice(&token_length.to_be_bytes());
        framed.extend_from_slice(&token);
        Ok(framed)
    }

    pub(crate) fn decode(&mut self, token: &[u8]) -> Result<Vec<u8>, ClientError> {
        let mut plaintext = Vec::new();
        let consumed = self
            .session
            .decode(token, &mut plaintext)
            .map_err(|error| ClientError::Sasl(error.to_string()))?;
        if consumed != token.len() || plaintext.is_empty() {
            return Err(ClientError::Sasl(
                "invalid SASL security-layer input length".to_owned(),
            ));
        }
        Ok(plaintext)
    }
}

pub(crate) async fn authenticate_sasl<S>(
    stream: &mut S,
    parameters: SaslParameters<'_>,
) -> Result<Option<SaslCodec>, ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mechanism_list = read_bounded_bytes(stream, "SASL mechanism list").await?;
    let mechanism_names = parse_mechanism_list(&mechanism_list)?;
    let callback = sasl_callback(parameters);
    static PASSWORD_MECHANISMS: &[Mechanism] =
        &[SCRAM_SHA512, SCRAM_SHA256, SCRAM_SHA1, PLAIN, LOGIN];
    static GSSAPI_MECHANISMS: &[Mechanism] =
        &[GSSAPI, SCRAM_SHA512, SCRAM_SHA256, SCRAM_SHA1, PLAIN, LOGIN];
    let registry = if parameters.options.allow_gssapi {
        Registry::with_mechanisms(GSSAPI_MECHANISMS)
    } else {
        Registry::with_mechanisms(PASSWORD_MECHANISMS)
    };
    let config = SASLConfig::builder()
        .with_registry(registry)
        .with_callback(callback)
        .map_err(|error| ClientError::Sasl(error.to_string()))?;
    let sasl = SASLClient::new(config);
    let offered: Vec<&Mechname> = mechanism_names
        .iter()
        .map(|name| {
            Mechname::parse(name.as_bytes()).map_err(|error| ClientError::Sasl(error.to_string()))
        })
        .collect::<Result<_, _>>()?;
    let mut session = sasl
        .start_suggested(&offered)
        .map_err(|error| ClientError::Sasl(error.to_string()))?;
    let mechanism = session.get_mechname().as_bytes();
    write_u32(stream, mechanism.len(), "SASL mechanism name").await?;
    stream.write_all(mechanism).await?;

    let mut state = None;
    let mut client_output = Zeroizing::new(Vec::new());
    if session.are_we_first() {
        state = Some(
            session
                .step(None, &mut *client_output)
                .map_err(|error| ClientError::Sasl(error.to_string()))?,
        );
    }
    loop {
        write_sasl_step(stream, &client_output).await?;
        client_output.clear();
        let server_output = read_bounded_bytes(stream, "SASL server step").await?;
        let complete = read_u8(stream).await?;
        if complete > 1 {
            return Err(ClientError::Sasl(
                "invalid SASL continuation flag".to_owned(),
            ));
        }
        let server_input = if server_output.is_empty() {
            None
        } else {
            let Some((&0, payload)) = server_output.split_last() else {
                return Err(ClientError::Sasl(
                    "SASL server step lacks terminator".to_owned(),
                ));
            };
            Some(payload)
        };
        if server_input.is_some() || !matches!(state, Some(State::Finished(_))) {
            state = Some(
                session
                    .step(server_input, &mut *client_output)
                    .map_err(|error| ClientError::Sasl(error.to_string()))?,
            );
        }
        if complete == 0 {
            if matches!(state, Some(State::Finished(_))) {
                return Err(ClientError::Sasl(
                    "server continued a finished SASL exchange".to_owned(),
                ));
            }
            continue;
        }
        if !matches!(state, Some(State::Finished(_))) {
            return Err(ClientError::Sasl(
                "server finished an incomplete SASL exchange".to_owned(),
            ));
        }
        if !client_output.is_empty() {
            return Err(ClientError::Sasl(
                "unsent final SASL client data".to_owned(),
            ));
        }
        break;
    }

    let result = read_u32(stream).await?;
    if result != 0 {
        let _ = read_bounded_bytes(stream, "SASL rejection message").await?;
        return Err(ClientError::Authentication);
    }
    if parameters.require_security_layer && !session.has_security_layer() {
        return Err(ClientError::Sasl(
            "SASL mechanism did not negotiate a required security layer".to_owned(),
        ));
    }
    Ok(session
        .has_security_layer()
        .then_some(SaslCodec { session }))
}

fn sasl_callback(parameters: SaslParameters<'_>) -> SaslCallback {
    let credentials = parameters.options.credentials.as_ref();
    SaslCallback {
        hostname: parameters.options.hostname.clone(),
        service: parameters.options.service.clone(),
        authentication_id: credentials.map(|value| value.authentication_id.clone()),
        authorization_id: credentials.and_then(|value| value.authorization_id.clone()),
        password: credentials.map(|value| value.password.clone()),
        security_layers: if parameters.require_security_layer {
            SecurityLayer::INTEGRITY | SecurityLayer::CONFIDENTIALITY
        } else {
            SecurityLayer::all()
        },
    }
}

fn parse_mechanism_list(input: &[u8]) -> Result<Vec<&str>, ClientError> {
    let text = std::str::from_utf8(input)
        .map_err(|_| ClientError::Sasl("non-UTF-8 SASL mechanism list".to_owned()))?;
    if !text.starts_with(',') || !text.ends_with(',') {
        return Err(ClientError::Sasl(
            "malformed SASL mechanism list".to_owned(),
        ));
    }
    let mechanisms: Vec<_> = text
        .split(',')
        .filter(|mechanism| !mechanism.is_empty())
        .collect();
    if mechanisms.is_empty() || mechanisms.iter().any(|mechanism| mechanism.len() > 100) {
        return Err(ClientError::Sasl("invalid SASL mechanism list".to_owned()));
    }
    Ok(mechanisms)
}

async fn write_sasl_step<S>(stream: &mut S, output: &[u8]) -> Result<(), ClientError>
where
    S: AsyncWrite + Unpin,
{
    if output.is_empty() {
        stream.write_all(&0_u32.to_le_bytes()).await?;
        return Ok(());
    }
    let wire_length = output
        .len()
        .checked_add(1)
        .ok_or_else(|| ClientError::Sasl("SASL step length overflow".to_owned()))?;
    write_u32(stream, wire_length, "SASL client step").await?;
    stream.write_all(output).await?;
    stream.write_all(&[0]).await?;
    Ok(())
}

async fn read_bounded_bytes<S>(
    stream: &mut S,
    context: &'static str,
) -> Result<Vec<u8>, ClientError>
where
    S: AsyncRead + Unpin,
{
    let length = usize::try_from(read_u32(stream).await?)
        .map_err(|_| ClientError::Sasl(format!("{context} length overflow")))?;
    if length > SASL_MAX_DATA_BYTES {
        return Err(ClientError::Sasl(format!("{context} exceeds local bound")));
    }
    let mut output = vec![0; length];
    stream.read_exact(&mut output).await?;
    Ok(output)
}

async fn write_u32<S>(
    stream: &mut S,
    value: usize,
    context: &'static str,
) -> Result<(), ClientError>
where
    S: AsyncWrite + Unpin,
{
    let value = u32::try_from(value)
        .map_err(|_| ClientError::Sasl(format!("{context} length overflow")))?;
    stream.write_all(&value.to_le_bytes()).await?;
    Ok(())
}

async fn read_u32<S>(stream: &mut S) -> Result<u32, ClientError>
where
    S: AsyncRead + Unpin,
{
    let mut bytes = [0; 4];
    stream.read_exact(&mut bytes).await?;
    Ok(u32::from_le_bytes(bytes))
}

async fn read_u8<S>(stream: &mut S) -> Result<u8, ClientError>
where
    S: AsyncRead + Unpin,
{
    let mut byte = [0];
    stream.read_exact(&mut byte).await?;
    Ok(byte[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[test]
    fn mechanism_list_requires_comma_framing_and_bounds_names() {
        assert_eq!(
            parse_mechanism_list(b",GSSAPI,SCRAM-SHA-256,").expect("mechanisms"),
            ["GSSAPI", "SCRAM-SHA-256"]
        );
        assert!(parse_mechanism_list(b"GSSAPI").is_err());
        assert!(parse_mechanism_list(b",,").is_err());
    }

    #[tokio::test]
    async fn plain_exchange_uses_spice_lengths_and_trailing_nul() {
        let (mut client_stream, mut server_stream) = duplex(1024);
        let server = tokio::spawn(async move {
            let mechanisms = b",PLAIN,";
            server_stream
                .write_all(&(mechanisms.len() as u32).to_le_bytes())
                .await
                .expect("mechanism length");
            server_stream
                .write_all(mechanisms)
                .await
                .expect("mechanisms");
            let mechanism_length = read_test_u32(&mut server_stream).await as usize;
            let mut mechanism = vec![0; mechanism_length];
            server_stream
                .read_exact(&mut mechanism)
                .await
                .expect("selected mechanism");
            assert_eq!(mechanism, b"PLAIN");
            let step_length = read_test_u32(&mut server_stream).await as usize;
            let mut step = vec![0; step_length];
            server_stream
                .read_exact(&mut step)
                .await
                .expect("client step");
            assert_eq!(step, b"\0user\0secret\0");
            server_stream
                .write_all(&0_u32.to_le_bytes())
                .await
                .expect("empty server step");
            server_stream.write_all(&[1]).await.expect("complete flag");
            server_stream
                .write_all(&0_u32.to_le_bytes())
                .await
                .expect("auth result");
        });
        let options =
            SaslOptions::with_credentials("localhost", SaslCredentials::new("user", "secret"));
        let codec = authenticate_sasl(
            &mut client_stream,
            SaslParameters {
                options: &options,
                require_security_layer: false,
            },
        )
        .await
        .expect("PLAIN authentication");
        assert!(codec.is_none());
        server.await.expect("server task");
    }

    async fn read_test_u32<S>(stream: &mut S) -> u32
    where
        S: AsyncRead + Unpin,
    {
        let mut bytes = [0; 4];
        stream.read_exact(&mut bytes).await.expect("u32");
        u32::from_le_bytes(bytes)
    }
}
