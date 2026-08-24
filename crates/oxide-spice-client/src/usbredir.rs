//! Bounded raw usbredir byte transport over a SpiceVMC channel.

use std::sync::Arc;

use oxide_spice_codecs::{compress_lz4_block_if_smaller, decode_lz4_block_exact};
use oxide_spice_protocol::{
    MAX_USBREDIR_PACKET_BYTES, SpiceVmcCompressedData, encode_spicevmc_compressed_data,
    spicevmc_capability,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};

use crate::ClientError;
use crate::channel::{
    Channel, ChannelIdentity, ControlDisposition, ControlState, IncomingMessage, ProgressRegistry,
    handle_channel_wait,
};

const USBREDIR_QUEUE_CAPACITY: usize = 16;
const SPICEVMC_DATA: u16 = 101;
const SPICEVMC_COMPRESSED_DATA: u16 = 102;
/// Compression is useful only once the fixed envelope is negligible relative to the payload.
const SPICEVMC_LZ4_COMPRESSION_THRESHOLD_BYTES: usize = 1000;

/// Latest transport state for one usbredir channel id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbRedirState {
    Ready {
        connection_generation: u64,
        channel_id: u8,
        transport_generation: u64,
    },
    Closed {
        connection_generation: u64,
        channel_id: u8,
        transport_generation: u64,
    },
}

/// One raw usbredir chunk labeled with the transport instance that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbRedirInbound {
    pub transport_generation: u64,
    pub bytes: Arc<[u8]>,
}

/// Host-facing failure for raw usbredir stream I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UsbRedirSendError {
    #[error("usbredir byte chunk is empty or exceeds its configured bound")]
    InvalidData,
    #[error("usbredir channel is closed")]
    Closed,
}

enum UsbRedirCommand {
    Write {
        bytes: Box<[u8]>,
        completion: oneshot::Sender<Result<(), UsbRedirSendError>>,
    },
}

/// Unique raw stream owner suitable for a native or Rust usbredir host implementation.
pub struct UsbRedirChannel {
    channel_id: u8,
    state: watch::Receiver<UsbRedirState>,
    commands: mpsc::Sender<UsbRedirCommand>,
    inbound: mpsc::Receiver<UsbRedirInbound>,
}

impl std::fmt::Debug for UsbRedirChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsbRedirChannel")
            .field("channel_id", &self.channel_id)
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

impl UsbRedirChannel {
    pub const fn channel_id(&self) -> u8 {
        self.channel_id
    }

    pub fn state(&self) -> UsbRedirState {
        *self.state.borrow()
    }

    pub async fn changed(&mut self) -> Result<UsbRedirState, UsbRedirSendError> {
        self.state
            .changed()
            .await
            .map_err(|_| UsbRedirSendError::Closed)?;
        Ok(*self.state.borrow_and_update())
    }

    /// Receives the next reliable byte chunk from the remote usbredir stream.
    pub async fn next(&mut self) -> Result<UsbRedirInbound, UsbRedirSendError> {
        self.inbound.recv().await.ok_or(UsbRedirSendError::Closed)
    }

    /// Writes one bounded byte chunk without interpreting usbredir packet boundaries.
    pub async fn write(&self, bytes: &[u8]) -> Result<(), UsbRedirSendError> {
        if bytes.is_empty() || bytes.len() > MAX_USBREDIR_PACKET_BYTES {
            return Err(UsbRedirSendError::InvalidData);
        }
        let (completion, completed) = oneshot::channel();
        self.commands
            .send(UsbRedirCommand::Write {
                bytes: Box::from(bytes),
                completion,
            })
            .await
            .map_err(|_| UsbRedirSendError::Closed)?;
        completed.await.map_err(|_| UsbRedirSendError::Closed)?
    }
}

pub(crate) struct UsbRedirTaskPaths {
    commands: mpsc::Receiver<UsbRedirCommand>,
    state: watch::Sender<UsbRedirState>,
    inbound: mpsc::Sender<UsbRedirInbound>,
}

pub(crate) fn usbredir_channel(
    connection_generation: u64,
    channel_id: u8,
) -> (UsbRedirChannel, UsbRedirTaskPaths) {
    let (state_sender, state) = watch::channel(UsbRedirState::Ready {
        connection_generation,
        channel_id,
        transport_generation: 0,
    });
    let (command_sender, commands) = mpsc::channel(USBREDIR_QUEUE_CAPACITY);
    let (inbound_sender, inbound) = mpsc::channel(USBREDIR_QUEUE_CAPACITY);
    (
        UsbRedirChannel {
            channel_id,
            state,
            commands: command_sender,
            inbound,
        },
        UsbRedirTaskPaths {
            commands,
            state: state_sender,
            inbound: inbound_sender,
        },
    )
}

/// Owns raw usbredir ordering, LZ4 transport compression, and reliable cancellation.
pub(crate) async fn run_usbredir<S>(
    mut channel: Channel<S>,
    mut cancel: watch::Receiver<bool>,
    mut paths: UsbRedirTaskPaths,
    connection_generation: u64,
    channel_id: u8,
    progress: ProgressRegistry,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut peer_supports_lz4 = channel.peer_supports(spicevmc_capability::DATA_COMPRESS_LZ4);
    let identity = ChannelIdentity {
        channel_type: oxide_spice_protocol::ChannelType::UsbRedirection,
        channel_id,
    };
    let mut control = ControlState::new();
    let mut message_body = Vec::new();
    let mut commands_open = true;
    let mut transport_generation = 0_u64;
    let mut observed_migration_activation = channel.migration_activation_count();

    loop {
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    paths.state.send_replace(UsbRedirState::Closed {
                        connection_generation,
                        channel_id,
                        transport_generation,
                    });
                    let _ = channel.shutdown().await;
                    return Ok(());
                }
            }
            incoming = channel.read_message(&mut message_body) => {
                let header = incoming?;
                let message = IncomingMessage { header, body: &message_body };
                let serial = channel.received_serial();
                if let Some(seamless) =
                    channel.observe_migration_activation(&mut observed_migration_activation)
                    && !seamless
                {
                    peer_supports_lz4 =
                        channel.peer_supports(spicevmc_capability::DATA_COMPRESS_LZ4);
                    transport_generation = transport_generation
                        .checked_add(1)
                        .ok_or_else(|| resource_limit_error("usbredir transport generation"))?;
                    paths.state.send_replace(UsbRedirState::Ready {
                        connection_generation,
                        channel_id,
                        transport_generation,
                    });
                }
                if control.handle(&mut channel, &message).await? == ControlDisposition::Consumed {
                    progress.complete(identity, serial)?;
                    continue;
                }
                if handle_channel_wait(&progress, identity, serial, &mut cancel, &message).await? {
                    progress.complete(identity, serial)?;
                    continue;
                }
                let bytes: Arc<[u8]> = match message.header.message_type {
                    SPICEVMC_DATA => Arc::from(message.body),
                    SPICEVMC_COMPRESSED_DATA => {
                        let compressed = SpiceVmcCompressedData::decode(
                            message.body,
                            MAX_USBREDIR_PACKET_BYTES,
                        )?;
                        decode_lz4_block_exact(
                            compressed.compressed_bytes,
                            compressed.uncompressed_size,
                            MAX_USBREDIR_PACKET_BYTES,
                        )?
                        .into()
                    }
                    message_type => return Err(ClientError::UnsupportedMessage {
                        channel: "usbredir",
                        message_type,
                    }),
                };
                let inbound = UsbRedirInbound {
                    transport_generation,
                    bytes,
                };
                match paths.inbound.try_send(inbound) {
                    Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        return Err(resource_limit_error("usbredir byte queue"));
                    }
                }
                progress.complete(identity, serial)?;
            }
            command = paths.commands.recv(), if commands_open => {
                let Some(UsbRedirCommand::Write { bytes, completion }) = command else {
                    commands_open = false;
                    continue;
                };
                let compressed = if peer_supports_lz4
                    && bytes.len() > SPICEVMC_LZ4_COMPRESSION_THRESHOLD_BYTES
                {
                    compress_lz4_block_if_smaller(&bytes, MAX_USBREDIR_PACKET_BYTES)?
                } else {
                    None
                };
                let (message_type, wire_body) = match compressed {
                    Some(compressed) => (
                        SPICEVMC_COMPRESSED_DATA,
                        encode_spicevmc_compressed_data(bytes.len(), &compressed)?,
                    ),
                    None => (SPICEVMC_DATA, Vec::from(bytes)),
                };
                if let Err(error) = channel.write_message(message_type, &wire_body).await {
                    let _ = completion.send(Err(UsbRedirSendError::Closed));
                    return Err(error);
                }
                let _ = completion.send(Ok(()));
            }
        }
    }
}

fn resource_limit_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::ResourceLimit,
        0,
        context,
    )
    .into()
}
