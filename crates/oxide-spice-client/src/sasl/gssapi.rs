//! RFC 4752 SASL GSSAPI over native SSPI or the platform GSS implementation.

use std::ops::Deref;

use cross_krb5::{ClientCtx, InitiateFlags, K5Ctx, PendingClientCtx, Step};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

use super::{
    SASL_MAX_DATA_BYTES, SaslParameters, read_bounded_bytes, read_u8, read_u32, write_sasl_step,
    write_u32,
};
use crate::ClientError;

const SASL_LAYER_NONE: u8 = 1;
const SASL_LAYER_INTEGRITY: u8 = 2;
const SASL_LAYER_CONFIDENTIALITY: u8 = 4;
const SASL_LAYER_MASK: u8 = SASL_LAYER_NONE | SASL_LAYER_INTEGRITY | SASL_LAYER_CONFIDENTIALITY;
const MAX_SASL_LAYER_SIZE: usize = 0x00ff_ffff;

pub(crate) struct GssapiCodec {
    context: ClientCtx,
    encrypt: bool,
    maximum_output_size: usize,
}

impl GssapiCodec {
    pub(super) fn encode(&mut self, plaintext: &[u8]) -> Result<Zeroizing<Vec<u8>>, ClientError> {
        let token = self
            .context
            .wrap(self.encrypt, plaintext)
            .map_err(gssapi_error)?;
        let token = token.deref();
        if token.is_empty()
            || token.len() > SASL_MAX_DATA_BYTES
            || token.len() > self.maximum_output_size
        {
            return Err(ClientError::Sasl(
                "GSSAPI security-layer output exceeds its negotiated bound".to_owned(),
            ));
        }
        Ok(Zeroizing::new(token.to_vec()))
    }

    pub(super) fn decode(&mut self, token: &[u8]) -> Result<Vec<u8>, ClientError> {
        let plaintext = self.context.unwrap(token).map_err(gssapi_error)?;
        let plaintext = plaintext.deref();
        if plaintext.is_empty() || plaintext.len() > SASL_MAX_DATA_BYTES {
            return Err(ClientError::Sasl(
                "invalid GSSAPI security-layer input length".to_owned(),
            ));
        }
        Ok(plaintext.to_vec())
    }
}

enum ContextPhase {
    Establishing(PendingClientCtx),
    AwaitingLayer(ClientCtx),
    AwaitingCompletion(Option<GssapiCodec>),
}

enum ContextStep {
    Continue(PendingClientCtx, Zeroizing<Vec<u8>>),
    Finished(ClientCtx, Option<Zeroizing<Vec<u8>>>),
}

pub(super) async fn authenticate<S>(
    stream: &mut S,
    parameters: SaslParameters<'_>,
) -> Result<Option<GssapiCodec>, ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    const MECHANISM: &[u8] = b"GSSAPI";
    write_u32(stream, MECHANISM.len(), "SASL mechanism name").await?;
    stream.write_all(MECHANISM).await?;

    let service_principal = format!(
        "{}/{}",
        parameters.options.service, parameters.options.hostname
    );
    let (pending, initial_token) = start_context(service_principal).await?;
    let mut phase = ContextPhase::Establishing(pending);
    let mut client_output = initial_token;

    loop {
        write_sasl_step(stream, &client_output).await?;
        client_output.zeroize();
        let (server_input, complete) = read_server_step(stream).await?;
        phase = match phase {
            ContextPhase::Establishing(pending) => {
                if complete {
                    return Err(ClientError::Sasl(
                        "server finished an incomplete GSSAPI context exchange".to_owned(),
                    ));
                }
                let server_token = server_input.ok_or_else(|| {
                    ClientError::Sasl("GSSAPI context step lacks a server token".to_owned())
                })?;
                match step_context(pending, server_token).await? {
                    ContextStep::Continue(pending, token) => {
                        client_output = token;
                        ContextPhase::Establishing(pending)
                    }
                    ContextStep::Finished(context, final_token) => {
                        client_output = final_token.unwrap_or_default();
                        ContextPhase::AwaitingLayer(context)
                    }
                }
            }
            ContextPhase::AwaitingLayer(context) => {
                if complete {
                    return Err(ClientError::Sasl(
                        "server omitted the GSSAPI security-layer offer".to_owned(),
                    ));
                }
                let wrapped_offer = server_input.ok_or_else(|| {
                    ClientError::Sasl("GSSAPI security-layer offer is empty".to_owned())
                })?;
                let authorization_id = parameters
                    .options
                    .credentials
                    .as_ref()
                    .and_then(|credentials| credentials.authorization_id.as_deref());
                let (codec, response) = negotiate_security_layer(
                    context,
                    wrapped_offer,
                    authorization_id,
                    parameters.require_security_layer,
                )
                .await?;
                client_output = response;
                ContextPhase::AwaitingCompletion(codec)
            }
            ContextPhase::AwaitingCompletion(codec) => {
                if !complete || server_input.is_some() {
                    return Err(ClientError::Sasl(
                        "invalid GSSAPI completion response".to_owned(),
                    ));
                }
                let result = read_u32(stream).await?;
                if result != 0 {
                    let _ = read_bounded_bytes(stream, "SASL rejection message").await?;
                    return Err(ClientError::Authentication);
                }
                return Ok(codec);
            }
        };
    }
}

async fn start_context(
    service_principal: String,
) -> Result<(PendingClientCtx, Zeroizing<Vec<u8>>), ClientError> {
    tokio::task::spawn_blocking(move || {
        ClientCtx::new(InitiateFlags::empty(), None, &service_principal, None)
            .map(|(context, token)| (context, Zeroizing::new(token.deref().to_vec())))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| ClientError::TaskTerminated)?
    .map_err(|error| ClientError::Sasl(format!("GSSAPI context initialization failed: {error}")))
}

async fn step_context(
    pending: PendingClientCtx,
    server_token: Zeroizing<Vec<u8>>,
) -> Result<ContextStep, ClientError> {
    tokio::task::spawn_blocking(move || {
        pending
            .step(&server_token)
            .map(|step| match step {
                Step::Continue((context, token)) => {
                    ContextStep::Continue(context, Zeroizing::new(token.deref().to_vec()))
                }
                Step::Finished((context, token)) => ContextStep::Finished(
                    context,
                    token.map(|token| Zeroizing::new(token.deref().to_vec())),
                ),
            })
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| ClientError::TaskTerminated)?
    .map_err(|error| ClientError::Sasl(format!("GSSAPI context step failed: {error}")))
}

async fn negotiate_security_layer(
    context: ClientCtx,
    wrapped_offer: Zeroizing<Vec<u8>>,
    authorization_id: Option<&str>,
    require_security_layer: bool,
) -> Result<(Option<GssapiCodec>, Zeroizing<Vec<u8>>), ClientError> {
    let authorization_id = Zeroizing::new(authorization_id.unwrap_or_default().as_bytes().to_vec());
    tokio::task::spawn_blocking(move || {
        let mut context = context;
        let offer = context
            .unwrap(&wrapped_offer)
            .map_err(|error| error.to_string())?;
        let offer = offer.deref();
        if offer.len() != 4 {
            return Err("GSSAPI security-layer offer must contain four bytes".to_owned());
        }
        let offered_layers = offer[0] & SASL_LAYER_MASK;
        let server_maximum =
            (usize::from(offer[1]) << 16) | (usize::from(offer[2]) << 8) | usize::from(offer[3]);
        let selected_layer = select_layer(offered_layers, require_security_layer)?;
        if selected_layer != SASL_LAYER_NONE && server_maximum == 0 {
            return Err("GSSAPI server advertised a protected layer with zero capacity".to_owned());
        }
        if selected_layer == SASL_LAYER_NONE && server_maximum != 0 {
            return Err("GSSAPI server advertised a maximum size without a layer".to_owned());
        }
        let client_maximum = if selected_layer == SASL_LAYER_NONE {
            0
        } else {
            SASL_MAX_DATA_BYTES.min(MAX_SASL_LAYER_SIZE)
        };
        let mut response = Zeroizing::new(Vec::with_capacity(4 + authorization_id.len()));
        response.push(selected_layer);
        response.push(((client_maximum >> 16) & 0xff) as u8);
        response.push(((client_maximum >> 8) & 0xff) as u8);
        response.push((client_maximum & 0xff) as u8);
        response.extend_from_slice(&authorization_id);
        let wrapped_response = context
            .wrap(false, &response)
            .map_err(|error| error.to_string())?;
        let wrapped_response = Zeroizing::new(wrapped_response.deref().to_vec());
        let codec = match selected_layer {
            SASL_LAYER_NONE => None,
            SASL_LAYER_INTEGRITY | SASL_LAYER_CONFIDENTIALITY => Some(GssapiCodec {
                context,
                encrypt: selected_layer == SASL_LAYER_CONFIDENTIALITY,
                maximum_output_size: server_maximum,
            }),
            _ => unreachable!("selected layer is validated"),
        };
        Ok((codec, wrapped_response))
    })
    .await
    .map_err(|_| ClientError::TaskTerminated)?
    .map_err(|error| ClientError::Sasl(format!("GSSAPI layer negotiation failed: {error}")))
}

fn select_layer(offered: u8, required: bool) -> Result<u8, String> {
    if required {
        if offered & SASL_LAYER_CONFIDENTIALITY != 0 {
            return Ok(SASL_LAYER_CONFIDENTIALITY);
        }
        if offered & SASL_LAYER_INTEGRITY != 0 {
            return Ok(SASL_LAYER_INTEGRITY);
        }
        return Err("GSSAPI server did not offer a required security layer".to_owned());
    }
    if offered & SASL_LAYER_NONE != 0 {
        Ok(SASL_LAYER_NONE)
    } else if offered & SASL_LAYER_CONFIDENTIALITY != 0 {
        Ok(SASL_LAYER_CONFIDENTIALITY)
    } else if offered & SASL_LAYER_INTEGRITY != 0 {
        Ok(SASL_LAYER_INTEGRITY)
    } else {
        Err("GSSAPI server offered no supported security layer".to_owned())
    }
}

async fn read_server_step<S>(
    stream: &mut S,
) -> Result<(Option<Zeroizing<Vec<u8>>>, bool), ClientError>
where
    S: AsyncRead + Unpin,
{
    let mut server_output = read_bounded_bytes(stream, "SASL server step").await?;
    let complete = read_u8(stream).await?;
    if complete > 1 {
        return Err(ClientError::Sasl(
            "invalid SASL continuation flag".to_owned(),
        ));
    }
    let payload = if server_output.is_empty() {
        None
    } else {
        if server_output.last() != Some(&0) {
            return Err(ClientError::Sasl(
                "SASL server step lacks terminator".to_owned(),
            ));
        }
        server_output.pop();
        Some(server_output)
    };
    Ok((payload, complete == 1))
}

fn gssapi_error(error: impl ToString) -> ClientError {
    ClientError::Sasl(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_selection_prefers_confidentiality_when_protection_is_required() {
        assert_eq!(
            select_layer(SASL_LAYER_INTEGRITY | SASL_LAYER_CONFIDENTIALITY, true),
            Ok(SASL_LAYER_CONFIDENTIALITY)
        );
        assert!(select_layer(SASL_LAYER_NONE, true).is_err());
    }

    #[test]
    fn tls_transport_prefers_no_additional_security_layer() {
        assert_eq!(
            select_layer(SASL_LAYER_NONE | SASL_LAYER_CONFIDENTIALITY, false),
            Ok(SASL_LAYER_NONE)
        );
    }
}
