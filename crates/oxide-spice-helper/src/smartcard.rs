//! PC/SC backend for one SPICE virtual smartcard channel.

use std::ffi::{CStr, CString};
use std::sync::Arc;

use oxide_spice_client::{
    SMARTCARD_UNDEFINED_READER_ID, SmartcardChannel, SmartcardInbound, SmartcardMessageType,
    SmartcardSendError,
};
use pcsc::{Card, Context, Protocols, Scope, ShareMode};

const VSC_SUCCESS: u32 = 0;
const VSC_GENERAL_ERROR: u32 = 1;
const SMARTCARD_RESPONSE_BYTES: usize = pcsc::MAX_BUFFER_SIZE_EXTENDED;

/// One PC/SC reader name retained without lossy character conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcscReader {
    name: CString,
}

impl PcscReader {
    pub fn name(&self) -> &CStr {
        &self.name
    }

    pub fn display_name(&self) -> String {
        self.name.to_string_lossy().into_owned()
    }
}

/// Failures from PC/SC ownership or VSC protocol state transitions.
#[derive(Debug, thiserror::Error)]
pub enum SmartcardRedirectionError {
    #[error("PC/SC failed: {0}")]
    Pcsc(#[from] pcsc::Error),
    #[error("SPICE Smartcard transport failed: {0}")]
    Transport(#[from] SmartcardSendError),
    #[error("Smartcard reader name contains an interior NUL byte")]
    InvalidReaderName,
    #[error("server rejected the Smartcard reader with VSC status {0}")]
    ReaderRejected(u32),
    #[error("server returned a malformed Smartcard acknowledgement")]
    InvalidAcknowledgement,
    #[error("server sent an invalid Smartcard state transition")]
    InvalidState,
    #[error("Smartcard worker panicked")]
    WorkerPanicked,
}

/// Lists every reader visible through the current PC/SC context.
pub fn list_pcsc_readers() -> Result<Vec<PcscReader>, SmartcardRedirectionError> {
    let context = Context::establish(Scope::User)?;
    Ok(context
        .list_readers_owned()?
        .into_iter()
        .map(|name| PcscReader { name })
        .collect())
}

/// Confirms that the PC/SC client library can enter its system-service boundary.
pub fn check_pcsc_client_library() -> Result<(), SmartcardRedirectionError> {
    match Context::establish(Scope::User) {
        Ok(_) | Err(pcsc::Error::NoService | pcsc::Error::ServiceStopped) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Redirects one already-selected PC/SC reader until the SPICE channel closes.
pub async fn run_smartcard_redirection(
    mut channel: SmartcardChannel,
    reader: PcscReader,
) -> Result<(), SmartcardRedirectionError> {
    let reader_name = reader.name.clone();
    let (card, atr) = tokio::task::spawn_blocking(move || open_card(&reader_name))
        .await
        .map_err(|_| SmartcardRedirectionError::WorkerPanicked)??;
    let card = Arc::new(card);

    channel
        .send(
            SmartcardMessageType::ReaderAdd,
            SMARTCARD_UNDEFINED_READER_ID,
            reader.name().to_bytes(),
        )
        .await?;
    let reader_id = receive_success_acknowledgement(&mut channel).await?;
    channel
        .send(SmartcardMessageType::Atr, reader_id, &atr)
        .await?;

    loop {
        let incoming = channel.next().await?;
        match incoming.message_type {
            SmartcardMessageType::Apdu => {
                if incoming.reader_id != reader_id {
                    return Err(SmartcardRedirectionError::InvalidState);
                }
                let card = card.clone();
                let command = incoming.data;
                let response = tokio::task::spawn_blocking(move || transmit_apdu(&card, &command))
                    .await
                    .map_err(|_| SmartcardRedirectionError::WorkerPanicked)?;
                match response {
                    Ok(response) => {
                        channel
                            .send(SmartcardMessageType::Apdu, reader_id, &response)
                            .await?;
                    }
                    Err(_) => {
                        channel
                            .send(
                                SmartcardMessageType::Error,
                                reader_id,
                                &VSC_GENERAL_ERROR.to_le_bytes(),
                            )
                            .await?;
                    }
                }
            }
            SmartcardMessageType::Flush => {
                channel
                    .send(SmartcardMessageType::FlushComplete, reader_id, &[])
                    .await?;
            }
            SmartcardMessageType::Init => {
                channel
                    .send(
                        SmartcardMessageType::Error,
                        reader_id,
                        &VSC_SUCCESS.to_le_bytes(),
                    )
                    .await?;
            }
            SmartcardMessageType::Error => {
                validate_success_acknowledgement(&incoming)?;
            }
            SmartcardMessageType::ReaderAdd
            | SmartcardMessageType::ReaderRemove
            | SmartcardMessageType::Atr
            | SmartcardMessageType::CardRemove
            | SmartcardMessageType::FlushComplete => {
                return Err(SmartcardRedirectionError::InvalidState);
            }
        }
    }
}

fn open_card(reader_name: &CStr) -> Result<(Card, Vec<u8>), pcsc::Error> {
    let context = Context::establish(Scope::User)?;
    let card = context.connect(
        reader_name,
        ShareMode::Shared,
        Protocols::T0 | Protocols::T1,
    )?;
    let atr = card.status2_owned()?.atr().to_vec();
    Ok((card, atr))
}

fn transmit_apdu(card: &Card, command: &[u8]) -> Result<Vec<u8>, pcsc::Error> {
    let mut response = vec![0; SMARTCARD_RESPONSE_BYTES];
    let response_length = card.transmit(command, &mut response)?.len();
    response.truncate(response_length);
    Ok(response)
}

async fn receive_success_acknowledgement(
    channel: &mut SmartcardChannel,
) -> Result<u32, SmartcardRedirectionError> {
    let acknowledgement = channel.next().await?;
    validate_success_acknowledgement(&acknowledgement)?;
    if acknowledgement.reader_id == SMARTCARD_UNDEFINED_READER_ID {
        return Err(SmartcardRedirectionError::InvalidAcknowledgement);
    }
    Ok(acknowledgement.reader_id)
}

fn validate_success_acknowledgement(
    acknowledgement: &SmartcardInbound,
) -> Result<(), SmartcardRedirectionError> {
    if acknowledgement.message_type != SmartcardMessageType::Error
        || acknowledgement.data.len() != size_of::<u32>()
    {
        return Err(SmartcardRedirectionError::InvalidAcknowledgement);
    }
    let status = u32::from_le_bytes(
        acknowledgement
            .data
            .as_ref()
            .try_into()
            .expect("four-byte VSC status"),
    );
    if status != VSC_SUCCESS {
        return Err(SmartcardRedirectionError::ReaderRejected(status));
    }
    Ok(())
}
