//! Bounded bidirectional byte streams for SPICE Port and WebDAV channels.

use std::sync::Arc;

use oxide_spice_codecs::{compress_lz4_block_if_smaller, decode_lz4_block_exact};
use oxide_spice_protocol::{
    ChannelType, MAX_PORT_DATA_BYTES, PortEvent, PortInit, SpiceVmcCompressedData,
    decode_port_data, decode_port_event, encode_port_event, encode_spicevmc_compressed_data,
    port_client, port_server, spicevmc_capability,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};

use crate::ClientError;
use crate::channel::{
    Channel, ChannelIdentity, ControlDisposition, ControlState, IncomingMessage, ProgressRegistry,
    handle_channel_wait,
};

/// Bounded host command count for one Port transport.
const PORT_COMMAND_QUEUE_CAPACITY: usize = 16;
/// Bounded inbound item count; byte limits are enforced independently per Data message.
const PORT_INBOUND_QUEUE_CAPACITY: usize = 16;
/// Compression is useful only once the fixed envelope is negligible relative to the payload.
const SPICEVMC_LZ4_COMPRESSION_THRESHOLD_BYTES: usize = 1000;

/// Latest port initialization and peer-open state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortState {
    AwaitingInit {
        connection_generation: u64,
        channel_type: ChannelType,
        channel_id: u8,
    },
    Ready {
        connection_generation: u64,
        channel_type: ChannelType,
        channel_id: u8,
        name: Arc<str>,
        opened: bool,
    },
    Closed {
        connection_generation: u64,
        channel_type: ChannelType,
        channel_id: u8,
        name: Option<Arc<str>>,
    },
}

/// One host-visible Port item whose bytes have independent bounded ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortInbound {
    Data {
        bytes: Arc<[u8]>,
        discontinuity: bool,
    },
    Break,
}

enum PortCommand {
    Write {
        bytes: Box<[u8]>,
        completion: oneshot::Sender<Result<(), PortSendError>>,
    },
    Break {
        completion: oneshot::Sender<Result<(), PortSendError>>,
    },
}

/// Host-facing Port submission failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PortSendError {
    #[error("Port is not initialized or the remote endpoint is closed")]
    Unavailable,
    #[error("Port payload exceeds the configured bound")]
    ResourceLimit,
    #[error("Port channel is closed")]
    Closed,
}

/// Unique host owner for one linked Port or WebDAV channel.
pub struct PortChannel {
    channel_type: ChannelType,
    channel_id: u8,
    state: watch::Receiver<PortState>,
    commands: mpsc::Sender<PortCommand>,
    inbound: mpsc::Receiver<PortInbound>,
}

impl std::fmt::Debug for PortChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PortChannel")
            .field("channel_type", &self.channel_type)
            .field("channel_id", &self.channel_id)
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

impl PortChannel {
    pub const fn channel_type(&self) -> ChannelType {
        self.channel_type
    }

    pub const fn channel_id(&self) -> u8 {
        self.channel_id
    }

    pub fn state(&self) -> PortState {
        self.state.borrow().clone()
    }

    pub async fn changed(&mut self) -> Result<PortState, PortSendError> {
        self.state
            .changed()
            .await
            .map_err(|_| PortSendError::Closed)?;
        Ok(self.state.borrow_and_update().clone())
    }

    pub async fn next(&mut self) -> Result<PortInbound, PortSendError> {
        self.inbound.recv().await.ok_or(PortSendError::Closed)
    }

    /// Writes one bounded byte chunk only while the remote endpoint is open.
    pub async fn write(&self, bytes: &[u8]) -> Result<(), PortSendError> {
        if bytes.len() > MAX_PORT_DATA_BYTES {
            return Err(PortSendError::ResourceLimit);
        }
        if !matches!(&*self.state.borrow(), PortState::Ready { opened: true, .. }) {
            return Err(PortSendError::Unavailable);
        }
        let (completion, completed) = oneshot::channel();
        self.commands
            .send(PortCommand::Write {
                bytes: Box::from(bytes),
                completion,
            })
            .await
            .map_err(|_| PortSendError::Closed)?;
        completed.await.map_err(|_| PortSendError::Closed)?
    }

    /// Sends the only host-managed Port event without changing open ownership locally.
    pub async fn send_break(&self) -> Result<(), PortSendError> {
        if !matches!(&*self.state.borrow(), PortState::Ready { opened: true, .. }) {
            return Err(PortSendError::Unavailable);
        }
        let (completion, completed) = oneshot::channel();
        self.commands
            .send(PortCommand::Break { completion })
            .await
            .map_err(|_| PortSendError::Closed)?;
        completed.await.map_err(|_| PortSendError::Closed)?
    }
}

/// Task-owned paths paired with one linked Port transport.
pub(crate) struct PortTaskPaths {
    commands: mpsc::Receiver<PortCommand>,
    state: watch::Sender<PortState>,
    inbound: mpsc::Sender<PortInbound>,
}

/// Creates one unique host owner and its task-private bounded paths.
pub(crate) fn port_channel(
    connection_generation: u64,
    channel_type: ChannelType,
    channel_id: u8,
) -> (PortChannel, PortTaskPaths) {
    let initial = PortState::AwaitingInit {
        connection_generation,
        channel_type,
        channel_id,
    };
    let (state_sender, state) = watch::channel(initial);
    let (command_sender, commands) = mpsc::channel(PORT_COMMAND_QUEUE_CAPACITY);
    let (inbound_sender, inbound) = mpsc::channel(PORT_INBOUND_QUEUE_CAPACITY);
    (
        PortChannel {
            channel_type,
            channel_id,
            state,
            commands: command_sender,
            inbound,
        },
        PortTaskPaths {
            commands,
            state: state_sender,
            inbound: inbound_sender,
        },
    )
}

/// Owns Port state, reads, and writes without interpreting the application byte stream.
pub(crate) async fn run_port<S>(
    mut channel: Channel<S>,
    mut cancel: watch::Receiver<bool>,
    mut paths: PortTaskPaths,
    connection_generation: u64,
    channel_type: ChannelType,
    channel_id: u8,
    progress: ProgressRegistry,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut peer_supports_lz4 = channel.peer_supports(spicevmc_capability::DATA_COMPRESS_LZ4);
    let identity = ChannelIdentity {
        channel_type,
        channel_id,
    };
    let mut control = ControlState::new();
    let mut message_body = Vec::new();
    let mut name = None::<Arc<str>>;
    let mut opened = false;
    let mut discontinuity = false;
    let mut commands_open = true;
    let mut observed_migration_activation = channel.migration_activation_count();

    loop {
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    paths.state.send_replace(PortState::Closed {
                        connection_generation,
                        channel_type,
                        channel_id,
                        name,
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
                    name = None;
                    opened = false;
                    discontinuity = true;
                    paths.state.send_replace(PortState::AwaitingInit {
                        connection_generation,
                        channel_type,
                        channel_id,
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
                match message.header.message_type {
                    port_server::INIT => {
                        if name.is_some() {
                            return Err(protocol_value_error("repeated Port Init"));
                        }
                        let init = PortInit::decode(message.body)?;
                        let port_name = Arc::<str>::from(init.name);
                        opened = init.opened;
                        name = Some(port_name.clone());
                        paths.state.send_replace(PortState::Ready {
                            connection_generation,
                            channel_type,
                            channel_id,
                            name: port_name,
                            opened,
                        });
                    }
                    port_server::DATA | port_server::COMPRESSED_DATA => {
                        let port_name = name.clone().ok_or_else(|| protocol_value_error("Port Data before Init"))?;
                        let decompressed;
                        let data = if message.header.message_type == port_server::COMPRESSED_DATA {
                            let compressed = SpiceVmcCompressedData::decode(
                                message.body,
                                MAX_PORT_DATA_BYTES,
                            )?;
                            decompressed = decode_lz4_block_exact(
                                compressed.compressed_bytes,
                                compressed.uncompressed_size,
                                MAX_PORT_DATA_BYTES,
                            )?;
                            decompressed.as_slice()
                        } else {
                            decode_port_data(message.body)?
                        };
                        if !opened {
                            opened = true;
                            paths.state.send_replace(PortState::Ready {
                                connection_generation,
                                channel_type,
                                channel_id,
                                name: port_name,
                                opened,
                            });
                        }
                        if !paths.inbound.is_closed() {
                            let inbound = PortInbound::Data {
                                bytes: Arc::from(data),
                                discontinuity,
                            };
                            match paths.inbound.try_send(inbound) {
                                Ok(()) => discontinuity = false,
                                Err(mpsc::error::TrySendError::Full(_))
                                | Err(mpsc::error::TrySendError::Closed(_)) => discontinuity = true,
                            }
                        }
                    }
                    port_server::EVENT => {
                        let port_name = name.clone().ok_or_else(|| protocol_value_error("Port Event before Init"))?;
                        match decode_port_event(message.body)? {
                            PortEvent::Opened => opened = true,
                            PortEvent::Closed => opened = false,
                            PortEvent::Break => {
                                match paths.inbound.try_send(PortInbound::Break) {
                                    Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        return Err(resource_limit_error("Port event queue"));
                                    }
                                }
                            }
                        }
                        paths.state.send_replace(PortState::Ready {
                            connection_generation,
                            channel_type,
                            channel_id,
                            name: port_name,
                            opened,
                        });
                    }
                    message_type => return Err(ClientError::UnsupportedMessage {
                        channel: "port",
                        message_type,
                    }),
                }
                progress.complete(identity, serial)?;
            }
            command = paths.commands.recv(), if commands_open => {
                let Some(command) = command else {
                    commands_open = false;
                    continue;
                };
                match command {
                    PortCommand::Write { bytes, completion } => {
                        if name.is_none() || !opened {
                            let _ = completion.send(Err(PortSendError::Unavailable));
                            continue;
                        }
                        let compressed = if peer_supports_lz4
                            && bytes.len() > SPICEVMC_LZ4_COMPRESSION_THRESHOLD_BYTES
                        {
                            compress_lz4_block_if_smaller(&bytes, MAX_PORT_DATA_BYTES)?
                        } else {
                            None
                        };
                        let (message_type, body) = match compressed {
                            Some(compressed) => (
                                port_client::COMPRESSED_DATA,
                                encode_spicevmc_compressed_data(bytes.len(), &compressed)?,
                            ),
                            None => (port_client::DATA, Vec::from(bytes)),
                        };
                        if let Err(error) = channel.write_message(message_type, &body).await {
                            let _ = completion.send(Err(PortSendError::Closed));
                            return Err(error);
                        }
                        let _ = completion.send(Ok(()));
                    }
                    PortCommand::Break { completion } => {
                        if name.is_none() || !opened {
                            let _ = completion.send(Err(PortSendError::Unavailable));
                            continue;
                        }
                        if let Err(error) = channel
                            .write_message(port_client::EVENT, &encode_port_event(PortEvent::Break))
                            .await
                        {
                            let _ = completion.send(Err(PortSendError::Closed));
                            return Err(error);
                        }
                        let _ = completion.send(Ok(()));
                    }
                }
            }
        }
    }
}

fn protocol_value_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::InvalidValue,
        0,
        context,
    )
    .into()
}

fn resource_limit_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::ResourceLimit,
        0,
        context,
    )
    .into()
}
