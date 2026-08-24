//! Bounded typed transport for SPICE virtual smartcard messages.

use std::sync::Arc;

use oxide_spice_protocol::{
    MAX_SMARTCARD_DATA_BYTES, SmartcardMessage, SmartcardMessageType, encode_smartcard_message,
    smartcard_client, smartcard_server,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};

use crate::ClientError;
use crate::channel::{
    Channel, ChannelIdentity, ControlDisposition, ControlState, IncomingMessage, ProgressRegistry,
    handle_channel_wait,
};

const SMARTCARD_QUEUE_CAPACITY: usize = 16;

/// Latest transport state for one Smartcard channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartcardState {
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

/// One owned VSC message delivered to the host smartcard backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmartcardInbound {
    pub transport_generation: u64,
    pub message_type: SmartcardMessageType,
    pub reader_id: u32,
    pub data: Arc<[u8]>,
}

/// Host-facing failure for Smartcard message submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SmartcardSendError {
    #[error("Smartcard payload exceeds its configured bound")]
    ResourceLimit,
    #[error("Smartcard channel is closed")]
    Closed,
}

enum SmartcardCommand {
    Send {
        body: Box<[u8]>,
        completion: oneshot::Sender<Result<(), SmartcardSendError>>,
    },
}

/// Unique typed owner for one Smartcard channel.
pub struct SmartcardChannel {
    channel_id: u8,
    state: watch::Receiver<SmartcardState>,
    commands: mpsc::Sender<SmartcardCommand>,
    inbound: mpsc::Receiver<SmartcardInbound>,
}

impl std::fmt::Debug for SmartcardChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SmartcardChannel")
            .field("channel_id", &self.channel_id)
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

impl SmartcardChannel {
    pub const fn channel_id(&self) -> u8 {
        self.channel_id
    }

    pub fn state(&self) -> SmartcardState {
        *self.state.borrow()
    }

    pub async fn changed(&mut self) -> Result<SmartcardState, SmartcardSendError> {
        self.state
            .changed()
            .await
            .map_err(|_| SmartcardSendError::Closed)?;
        Ok(*self.state.borrow_and_update())
    }

    pub async fn next(&mut self) -> Result<SmartcardInbound, SmartcardSendError> {
        self.inbound.recv().await.ok_or(SmartcardSendError::Closed)
    }

    /// Sends one bounded VSC operation.
    pub async fn send(
        &self,
        message_type: SmartcardMessageType,
        reader_id: u32,
        data: &[u8],
    ) -> Result<(), SmartcardSendError> {
        if data.len() > MAX_SMARTCARD_DATA_BYTES {
            return Err(SmartcardSendError::ResourceLimit);
        }
        let body = encode_smartcard_message(message_type, reader_id, data)
            .map_err(|_| SmartcardSendError::ResourceLimit)?
            .into_boxed_slice();
        let (completion, completed) = oneshot::channel();
        self.commands
            .send(SmartcardCommand::Send { body, completion })
            .await
            .map_err(|_| SmartcardSendError::Closed)?;
        completed.await.map_err(|_| SmartcardSendError::Closed)?
    }
}

pub(crate) struct SmartcardTaskPaths {
    commands: mpsc::Receiver<SmartcardCommand>,
    state: watch::Sender<SmartcardState>,
    inbound: mpsc::Sender<SmartcardInbound>,
}

pub(crate) fn smartcard_channel(
    connection_generation: u64,
    channel_id: u8,
) -> (SmartcardChannel, SmartcardTaskPaths) {
    let (state_sender, state) = watch::channel(SmartcardState::Ready {
        connection_generation,
        channel_id,
        transport_generation: 0,
    });
    let (command_sender, commands) = mpsc::channel(SMARTCARD_QUEUE_CAPACITY);
    let (inbound_sender, inbound) = mpsc::channel(SMARTCARD_QUEUE_CAPACITY);
    (
        SmartcardChannel {
            channel_id,
            state,
            commands: command_sender,
            inbound,
        },
        SmartcardTaskPaths {
            commands,
            state: state_sender,
            inbound: inbound_sender,
        },
    )
}

pub(crate) async fn run_smartcard<S>(
    mut channel: Channel<S>,
    mut cancel: watch::Receiver<bool>,
    mut paths: SmartcardTaskPaths,
    connection_generation: u64,
    channel_id: u8,
    progress: ProgressRegistry,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let identity = ChannelIdentity {
        channel_type: oxide_spice_protocol::ChannelType::Smartcard,
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
                    paths.state.send_replace(SmartcardState::Closed {
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
                    transport_generation = transport_generation
                        .checked_add(1)
                        .ok_or_else(|| resource_limit_error("Smartcard transport generation"))?;
                    paths.state.send_replace(SmartcardState::Ready {
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
                if message.header.message_type != smartcard_server::DATA {
                    return Err(ClientError::UnsupportedMessage {
                        channel: "smartcard",
                        message_type: message.header.message_type,
                    });
                }
                let decoded = SmartcardMessage::decode(message.body)?;
                let inbound = SmartcardInbound {
                    transport_generation,
                    message_type: decoded.message_type,
                    reader_id: decoded.reader_id,
                    data: Arc::from(decoded.data),
                };
                match paths.inbound.try_send(inbound) {
                    Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        return Err(resource_limit_error("Smartcard message queue"));
                    }
                }
                progress.complete(identity, serial)?;
            }
            command = paths.commands.recv(), if commands_open => {
                let Some(SmartcardCommand::Send { body, completion }) = command else {
                    commands_open = false;
                    continue;
                };
                if let Err(error) = channel.write_message(smartcard_client::DATA, &body).await {
                    let _ = completion.send(Err(SmartcardSendError::Closed));
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
