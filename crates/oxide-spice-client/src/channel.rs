//! Owned asynchronous channel framing.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxide_spice_protocol::{
    AUTH_MECHANISM_SASL, AUTH_MECHANISM_SPICE, CapabilitySet, ChannelType, DataHeader, Framing,
    LINK_HEADER_SIZE, LinkError, LinkHeader, LinkMessage, LinkReply, WaitForChannels,
    common_capability, common_client, common_server,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

use crate::ClientError;
use crate::auth::encrypt_ticket;
use crate::sasl::{
    SASL_MAX_DATA_BYTES, SASL_SECURITY_PLAINTEXT_BYTES, SaslCodec, SaslParameters,
    authenticate_sasl,
};
#[cfg(unix)]
use crate::unix_stream::ReceivedFileDescriptors;

/// Default maximum for one normal SPICE message body.
pub(crate) const DEFAULT_MAX_MESSAGE_BODY: usize = 16 * 1024 * 1024;

/// A sendable asynchronous stream kept independent of the selected transport security.
pub(crate) trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> AsyncStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// Type-erased stream that lets one supervisor own plain and TLS channel tasks uniformly.
pub(crate) type BoxedStream = Pin<Box<dyn AsyncStream>>;

/// Stable identity used by cross-channel serial barriers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ChannelIdentity {
    pub channel_type: ChannelType,
    pub channel_id: u8,
}

/// Shared monotonic completion serials for every linked channel owner.
#[derive(Clone)]
pub(crate) struct ProgressRegistry {
    entries: Arc<HashMap<ChannelIdentity, watch::Sender<u64>>>,
}

impl ProgressRegistry {
    /// Creates fixed progress slots before any channel task starts.
    pub(crate) fn new(
        channels: impl IntoIterator<Item = (ChannelIdentity, u64)>,
    ) -> Result<Self, ClientError> {
        let mut entries = HashMap::new();
        for (identity, completed_serial) in channels {
            if entries
                .insert(identity, watch::channel(completed_serial).0)
                .is_some()
            {
                return Err(ClientError::Internal("duplicate progress channel identity"));
            }
        }
        Ok(Self {
            entries: Arc::new(entries),
        })
    }

    /// Publishes a message only after its channel-specific state transition is complete.
    pub(crate) fn complete(
        &self,
        identity: ChannelIdentity,
        serial: u64,
    ) -> Result<(), ClientError> {
        let progress = self
            .entries
            .get(&identity)
            .ok_or(ClientError::Internal("missing progress channel identity"))?;
        if serial <= *progress.borrow() {
            return Err(ClientError::Internal("non-monotonic completed serial"));
        }
        progress.send_replace(serial);
        Ok(())
    }

    /// Waits for all referenced channels without blocking their independent socket owners.
    pub(crate) async fn wait_for(
        &self,
        requester: ChannelIdentity,
        requester_serial: u64,
        waits: &WaitForChannels,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<(), ClientError> {
        for wait in &waits.waits {
            let target = ChannelIdentity {
                channel_type: wait.channel_type,
                channel_id: wait.channel_id,
            };
            if target == requester && wait.message_serial >= requester_serial {
                return Err(protocol_value_error("self-referential channel wait"));
            }
            let progress = self
                .entries
                .get(&target)
                .ok_or_else(|| protocol_value_error("wait for unknown channel"))?;
            let mut completed = progress.subscribe();
            while *completed.borrow() < wait.message_serial {
                if *cancel.borrow() {
                    return Err(ClientError::Cancelled);
                }
                tokio::select! {
                    changed = completed.changed() => {
                        changed.map_err(|_| ClientError::TaskTerminated)?;
                    }
                    changed = cancel.changed() => {
                        if changed.is_err() || *cancel.borrow() {
                            return Err(ClientError::Cancelled);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Handles a common Wait For Channels message after ordinary ACK control dispatch.
pub(crate) async fn handle_channel_wait(
    progress: &ProgressRegistry,
    identity: ChannelIdentity,
    serial: u64,
    cancel: &mut watch::Receiver<bool>,
    message: &IncomingMessage<'_>,
) -> Result<bool, ClientError> {
    if message.header.message_type != common_server::WAIT_FOR_CHANNELS {
        return Ok(false);
    }
    let waits = WaitForChannels::decode(message.body)?;
    progress.wait_for(identity, serial, &waits, cancel).await?;
    Ok(true)
}

/// Inputs to the full SPICE link and Ticket authentication flow.
pub(crate) struct LinkParameters<'a> {
    pub connection_id: u32,
    pub channel_type: ChannelType,
    pub channel_id: u8,
    pub common_capabilities: CapabilitySet,
    pub channel_capabilities: CapabilitySet,
    pub password: &'a str,
    pub maximum_message_body: usize,
    pub sasl: Option<SaslParameters<'a>>,
}

/// One linked channel whose transport has a single mutable owner.
pub(crate) struct Channel<S> {
    stream: S,
    framing: Framing,
    next_serial: u64,
    last_received_serial: u64,
    received_serial_base: u64,
    maximum_message_body: usize,
    peer_channel_capabilities: CapabilitySet,
    local_common_capabilities: CapabilitySet,
    local_channel_capabilities: CapabilitySet,
    write_buffer: Vec<u8>,
    sasl_codec: Option<SaslCodec>,
    sasl_decoded: Vec<u8>,
    sasl_decoded_offset: usize,
    migration_replacements: Option<mpsc::Receiver<MigrationReplacement<S>>>,
    pending_migration_replacement: Option<Box<MigrationReplacement<S>>>,
    migration_cancel: Option<watch::Receiver<bool>>,
    active_migration_generation: Option<Arc<AtomicU64>>,
    migration_activation_count: u64,
    last_migration_seamless: bool,
    #[cfg(unix)]
    received_file_descriptors: Option<ReceivedFileDescriptors>,
}

pub(crate) struct MigrationReplacement<S> {
    pub generation: u64,
    pub seamless: bool,
    pub activate_immediately: bool,
    pub channel: Channel<S>,
}

/// A borrowed message whose body resides in the channel task's reusable read buffer.
pub(crate) struct IncomingMessage<'a> {
    pub header: DataHeader,
    pub body: &'a [u8],
}

impl<S> Channel<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Reports a channel-specific capability from the authenticated Link reply.
    pub(crate) fn peer_supports(&self, capability: u32) -> bool {
        self.peer_channel_capabilities.contains(capability)
    }

    /// Returns the effective serial of the last fully read server message.
    pub(crate) const fn received_serial(&self) -> u64 {
        self.received_serial_base
            .saturating_add(self.last_received_serial)
    }

    pub(crate) fn install_migration_path(
        &mut self,
        replacements: mpsc::Receiver<MigrationReplacement<S>>,
        cancel: watch::Receiver<bool>,
        active_generation: Arc<AtomicU64>,
    ) {
        self.migration_replacements = Some(replacements);
        self.migration_cancel = Some(cancel);
        self.active_migration_generation = Some(active_generation);
    }

    pub(crate) const fn migration_activation_count(&self) -> u64 {
        self.migration_activation_count
    }

    /// Reports one newly activated transport and advances the task's observation cursor.
    pub(crate) fn observe_migration_activation(&self, observed: &mut u64) -> Option<bool> {
        if *observed == self.migration_activation_count {
            return None;
        }
        *observed = self.migration_activation_count;
        Some(self.last_migration_seamless)
    }

    pub(crate) fn local_capabilities(&self) -> (CapabilitySet, CapabilitySet) {
        (
            self.local_common_capabilities.clone(),
            self.local_channel_capabilities.clone(),
        )
    }

    /// Transfers ownership of the oldest descriptor received with Unix stream bytes.
    #[cfg(unix)]
    pub(crate) fn take_received_file_descriptor(
        &mut self,
    ) -> Result<Option<std::os::fd::OwnedFd>, ClientError> {
        let Some(received) = self.received_file_descriptors.as_ref() else {
            return Ok(None);
        };
        received
            .lock()
            .map_err(|_| ClientError::Internal("Unix file descriptor queue poisoned"))
            .map(|mut queue| queue.pop_front())
    }

    /// Reads exactly one header and one bounded body.
    pub(crate) async fn read_message(
        &mut self,
        body: &mut Vec<u8>,
    ) -> Result<DataHeader, ClientError> {
        let mut header_bytes = [0; 18];
        let header_len = self.framing.header_len();
        loop {
            match self
                .read_header_transport(&mut header_bytes[..header_len])
                .await
            {
                Ok(()) => break,
                Err(ClientError::Io(_)) if self.migration_replacements.is_some() => {
                    let serial_base = self.received_serial();
                    self.activate_replacement(serial_base).await?;
                }
                Err(error) => return Err(error),
            }
        }
        let header = DataHeader::decode(self.framing, &header_bytes[..header_len])?;
        let body_len =
            usize::try_from(header.body_size).map_err(|_| ClientError::MessageTooLarge {
                declared: header.body_size,
                maximum: self.maximum_message_body,
            })?;
        if body_len > self.maximum_message_body {
            return Err(ClientError::MessageTooLarge {
                declared: header.body_size,
                maximum: self.maximum_message_body,
            });
        }
        body.resize(body_len, 0);
        self.read_exact_transport(body).await?;
        self.last_received_serial = match header.serial {
            Some(serial) if serial > self.last_received_serial => serial,
            Some(_) => return Err(protocol_value_error("server message serial regression")),
            None => self
                .last_received_serial
                .checked_add(1)
                .ok_or_else(|| protocol_value_error("server message serial overflow"))?,
        };
        self.received_serial_base
            .checked_add(self.last_received_serial)
            .ok_or_else(|| protocol_value_error("effective message serial overflow"))?;
        if header.sub_list_offset.is_some_and(|offset| offset != 0) {
            return Err(unsupported_protocol_value("sub-message list"));
        }
        Ok(header)
    }

    async fn read_header_transport(&mut self, output: &mut [u8]) -> Result<(), ClientError> {
        loop {
            let Some(mut replacements) = self.migration_replacements.take() else {
                return self.read_exact_transport(output).await;
            };
            let result = tokio::select! {
                read = self.read_exact_transport(output) => Some(read),
                replacement = replacements.recv() => {
                    let Some(replacement) = replacement else {
                        self.pending_migration_replacement = None;
                        self.migration_cancel = None;
                        self.active_migration_generation = None;
                        return self.read_exact_transport(output).await;
                    };
                    let active_generation = self
                        .active_migration_generation
                        .as_ref()
                        .ok_or(ClientError::Internal("missing active migration generation"))?
                        .load(Ordering::Acquire);
                    if replacement.generation == active_generation {
                        self.pending_migration_replacement = Some(Box::new(replacement));
                    }
                    None
                }
            };
            self.migration_replacements = Some(replacements);
            if let Some(result) = result {
                return result;
            }
            if self
                .pending_migration_replacement
                .as_ref()
                .is_some_and(|replacement| replacement.activate_immediately)
            {
                let serial_base = self.received_serial();
                self.activate_replacement(serial_base).await?;
            }
        }
    }

    /// Writes one message with a monotonic full-header serial and no sub-messages.
    pub(crate) async fn write_message(
        &mut self,
        message_type: u16,
        body: &[u8],
    ) -> Result<(), ClientError> {
        let body_size = u32::try_from(body.len()).map_err(|_| ClientError::MessageTooLarge {
            declared: u32::MAX,
            maximum: self.maximum_message_body,
        })?;
        if body.len() > self.maximum_message_body {
            return Err(ClientError::MessageTooLarge {
                declared: body_size,
                maximum: self.maximum_message_body,
            });
        }
        self.write_buffer.clear();
        self.write_buffer
            .reserve(self.framing.header_len() + body.len());
        DataHeader::encode(
            self.framing,
            self.next_serial,
            message_type,
            body_size,
            &mut self.write_buffer,
        );
        self.write_buffer.extend_from_slice(body);
        if let Some(codec) = self.sasl_codec.as_mut() {
            for plaintext in self.write_buffer.chunks(SASL_SECURITY_PLAINTEXT_BYTES) {
                let encoded = codec.encode(plaintext)?;
                self.stream.write_all(&encoded).await?;
            }
        } else {
            self.stream.write_all(&self.write_buffer).await?;
        }
        self.next_serial = self
            .next_serial
            .checked_add(1)
            .ok_or_else(|| protocol_value_error("client message serial overflow"))?;
        Ok(())
    }

    async fn read_exact_transport(&mut self, output: &mut [u8]) -> Result<(), ClientError> {
        if self.sasl_codec.is_none() {
            self.stream.read_exact(output).await?;
            return Ok(());
        }
        let mut written = 0;
        while written < output.len() {
            if self.sasl_decoded_offset < self.sasl_decoded.len() {
                let available = self.sasl_decoded.len() - self.sasl_decoded_offset;
                let copied = available.min(output.len() - written);
                output[written..written + copied].copy_from_slice(
                    &self.sasl_decoded[self.sasl_decoded_offset..self.sasl_decoded_offset + copied],
                );
                written += copied;
                self.sasl_decoded_offset += copied;
                if self.sasl_decoded_offset == self.sasl_decoded.len() {
                    self.sasl_decoded.clear();
                    self.sasl_decoded_offset = 0;
                }
                continue;
            }
            let mut token_length_bytes = [0; 4];
            self.stream.read_exact(&mut token_length_bytes).await?;
            let token_length = usize::try_from(u32::from_be_bytes(token_length_bytes))
                .map_err(|_| protocol_value_error("SASL security token length"))?;
            if token_length == 0 || token_length > SASL_MAX_DATA_BYTES {
                return Err(protocol_value_error("SASL security token length"));
            }
            let mut token = vec![0; token_length];
            self.stream.read_exact(&mut token).await?;
            self.sasl_decoded = self
                .sasl_codec
                .as_mut()
                .ok_or(ClientError::Internal("missing SASL codec"))?
                .decode(&token)?;
        }
        Ok(())
    }

    async fn migrate_to_replacement(&mut self, body: &[u8]) -> Result<(), ClientError> {
        const NEED_FLUSH: u32 = 1 << 0;
        const NEED_DATA_TRANSFER: u32 = 1 << 1;
        if body.len() != 4 {
            return Err(protocol_size_error("channel migrate flags"));
        }
        let flags = u32::from_le_bytes(body.try_into().expect("validated migrate flags"));
        if flags & !(NEED_FLUSH | NEED_DATA_TRANSFER) != 0 {
            return Err(unsupported_protocol_value("channel migrate flags"));
        }
        let migration_serial = self.received_serial();
        let mut cancel = self
            .migration_cancel
            .take()
            .ok_or_else(|| unsupported_protocol_value("uncoordinated channel migration"))?;
        if flags & NEED_FLUSH != 0 {
            self.write_message(common_client::MIGRATE_FLUSH_MARK, &[])
                .await?;
        }
        let migration_data = if flags & NEED_DATA_TRANSFER != 0 {
            let mut migration_data = Vec::new();
            let header = tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        return Err(ClientError::Cancelled);
                    }
                    self.read_message(&mut migration_data).await
                }
                message = self.read_message(&mut migration_data) => message,
            }?;
            if header.message_type != common_server::MIGRATE_DATA {
                return Err(protocol_value_error("expected channel migration data"));
            }
            Some(migration_data)
        } else {
            None
        };
        self.migration_cancel = Some(cancel);
        self.activate_replacement(migration_serial).await?;
        if let Some(migration_data) = migration_data {
            self.write_message(common_client::MIGRATE_DATA, &migration_data)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn activate_migration_target(&mut self) -> Result<(), ClientError> {
        let serial_base = self.received_serial();
        self.activate_replacement(serial_base).await
    }

    async fn activate_replacement(&mut self, serial_base: u64) -> Result<(), ClientError> {
        let mut replacements = self
            .migration_replacements
            .take()
            .ok_or_else(|| unsupported_protocol_value("uncoordinated channel migration"))?;
        let mut cancel = self
            .migration_cancel
            .take()
            .ok_or(ClientError::Internal("missing migration cancellation path"))?;
        let active_generation = self
            .active_migration_generation
            .take()
            .ok_or(ClientError::Internal("missing active migration generation"))?;
        let mut replacement = loop {
            let mut replacement =
                if let Some(replacement) = self.pending_migration_replacement.take() {
                    *replacement
                } else {
                    tokio::select! {
                    changed = cancel.changed() => {
                        if changed.is_err() || *cancel.borrow() {
                            return Err(ClientError::Cancelled);
                        }
                        replacements.recv().await
                    }
                    replacement = replacements.recv() => replacement,
                    }
                    .ok_or(ClientError::TaskTerminated)?
                };
            if replacement.generation == active_generation.load(Ordering::Acquire) {
                replacement.channel.last_migration_seamless = replacement.seamless;
                break replacement.channel;
            }
        };
        replacement.received_serial_base = serial_base;
        replacement.migration_activation_count = self
            .migration_activation_count
            .checked_add(1)
            .ok_or_else(|| protocol_value_error("migration activation count overflow"))?;
        replacement.install_migration_path(replacements, cancel, active_generation);
        *self = replacement;
        Ok(())
    }

    /// Closes the write side so a remote owner is not left waiting for more data.
    pub(crate) async fn shutdown(&mut self) -> Result<(), ClientError> {
        self.stream.shutdown().await?;
        Ok(())
    }
}

/// Performs Link, capability negotiation, and Ticket authentication on one stream.
pub(crate) async fn link_channel<S>(
    stream: S,
    parameters: LinkParameters<'_>,
) -> Result<Channel<S>, ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    link_channel_inner(
        stream,
        parameters,
        #[cfg(unix)]
        None,
    )
    .await
}

#[cfg(unix)]
pub(crate) async fn link_channel_with_file_descriptors<S>(
    stream: S,
    parameters: LinkParameters<'_>,
    received_file_descriptors: ReceivedFileDescriptors,
) -> Result<Channel<S>, ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    link_channel_inner(stream, parameters, Some(received_file_descriptors)).await
}

async fn link_channel_inner<S>(
    mut stream: S,
    parameters: LinkParameters<'_>,
    #[cfg(unix)] received_file_descriptors: Option<ReceivedFileDescriptors>,
) -> Result<Channel<S>, ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let local_common_capabilities = parameters.common_capabilities.clone();
    let local_channel_capabilities = parameters.channel_capabilities.clone();
    let link_message = LinkMessage {
        connection_id: parameters.connection_id,
        channel_type: parameters.channel_type,
        channel_id: parameters.channel_id,
        common_capabilities: parameters.common_capabilities.clone(),
        channel_capabilities: parameters.channel_capabilities,
    };
    let link_body = link_message.encode()?;
    let link_body_size = u32::try_from(link_body.len())
        .map_err(|_| ClientError::Configuration("link body does not fit u32"))?;
    let mut request = Vec::with_capacity(LINK_HEADER_SIZE + link_body.len());
    LinkHeader::current(link_body_size).encode(&mut request);
    request.extend_from_slice(&link_body);
    stream.write_all(&request).await?;

    let mut reply_header_bytes = [0; LINK_HEADER_SIZE];
    stream.read_exact(&mut reply_header_bytes).await?;
    let reply_header = LinkHeader::decode(&reply_header_bytes)?;
    let reply_body_len = usize::try_from(reply_header.body_size)
        .map_err(|_| ClientError::Configuration("link reply size does not fit usize"))?;
    let mut reply_body = vec![0; reply_body_len];
    stream.read_exact(&mut reply_body).await?;
    let reply = LinkReply::decode(&reply_body)?;
    if reply.result != LinkError::Ok {
        return Err(ClientError::Link(reply.result));
    }

    let auth_selection = parameters
        .common_capabilities
        .contains(common_capability::AUTH_SELECTION)
        && reply
            .common_capabilities
            .contains(common_capability::AUTH_SELECTION);
    let use_sasl = auth_selection
        && parameters.sasl.is_some()
        && reply
            .common_capabilities
            .contains(common_capability::AUTH_SASL);
    let sasl_codec = if use_sasl {
        stream.write_all(&AUTH_MECHANISM_SASL.to_le_bytes()).await?;
        authenticate_sasl(
            &mut stream,
            parameters
                .sasl
                .ok_or(ClientError::Internal("missing SASL parameters"))?,
        )
        .await?
    } else {
        if auth_selection {
            if !reply
                .common_capabilities
                .contains(common_capability::AUTH_SPICE)
            {
                return Err(ClientError::AuthenticationMechanism);
            }
            stream
                .write_all(&AUTH_MECHANISM_SPICE.to_le_bytes())
                .await?;
        }
        let encrypted_ticket = encrypt_ticket(&reply.public_key_der, parameters.password)?;
        stream.write_all(&encrypted_ticket).await?;
        let mut result_bytes = [0; 4];
        stream.read_exact(&mut result_bytes).await?;
        let result = LinkError::try_from(u32::from_le_bytes(result_bytes))?;
        if result != LinkError::Ok {
            return Err(if result == LinkError::PermissionDenied {
                ClientError::Authentication
            } else {
                ClientError::Link(result)
            });
        }
        None
    };

    let framing = if parameters
        .common_capabilities
        .contains(common_capability::MINI_HEADER)
        && reply
            .common_capabilities
            .contains(common_capability::MINI_HEADER)
    {
        Framing::Mini
    } else {
        Framing::Full
    };
    Ok(Channel {
        stream,
        framing,
        next_serial: 1,
        last_received_serial: 0,
        received_serial_base: 0,
        maximum_message_body: parameters.maximum_message_body,
        peer_channel_capabilities: reply.channel_capabilities,
        local_common_capabilities,
        local_channel_capabilities,
        write_buffer: Vec::new(),
        sasl_codec,
        sasl_decoded: Vec::new(),
        sasl_decoded_offset: 0,
        migration_replacements: None,
        pending_migration_replacement: None,
        migration_cancel: None,
        active_migration_generation: None,
        migration_activation_count: 0,
        last_migration_seamless: false,
        #[cfg(unix)]
        received_file_descriptors,
    })
}

/// ACK window state inherited by every channel.
pub(crate) struct ControlState {
    generation: u32,
    window: u32,
    consumed_since_ack: u32,
    migration_activation_count: u64,
}

impl ControlState {
    /// Starts with ACK flow control disabled until the server installs a window.
    pub(crate) const fn new() -> Self {
        Self {
            generation: 0,
            window: 0,
            consumed_since_ack: 0,
            migration_activation_count: 0,
        }
    }

    /// Associates already-negotiated ACK state with a replacement transport activation.
    pub(crate) fn synchronize_migration_activation<S>(&mut self, channel: &Channel<S>)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.migration_activation_count = channel.migration_activation_count();
    }

    /// Handles common control traffic and reports whether the caller should dispatch the message.
    pub(crate) async fn handle<S>(
        &mut self,
        channel: &mut Channel<S>,
        message: &IncomingMessage<'_>,
    ) -> Result<ControlDisposition, ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if self.migration_activation_count != channel.migration_activation_count() {
            self.generation = 0;
            self.window = 0;
            self.consumed_since_ack = 0;
            self.migration_activation_count = channel.migration_activation_count();
        }
        let disposition = match message.header.message_type {
            common_server::SET_ACK => {
                if message.body.len() != 8 {
                    return Err(protocol_size_error("set ack body"));
                }
                self.generation = read_u32(message.body, 0);
                self.window = read_u32(message.body, 4);
                self.consumed_since_ack = 0;
                channel
                    .write_message(common_client::ACK_SYNC, &self.generation.to_le_bytes())
                    .await?;
                ControlDisposition::Consumed
            }
            common_server::PING => {
                if message.body.len() < 12 {
                    return Err(protocol_size_error("ping body"));
                }
                channel
                    .write_message(common_client::PONG, &message.body[..12])
                    .await?;
                ControlDisposition::Consumed
            }
            common_server::DISCONNECTING => {
                if message.body.len() != 12 {
                    return Err(protocol_size_error("disconnect body"));
                }
                return Err(ClientError::RemoteDisconnect {
                    reason: read_u32(message.body, 8),
                });
            }
            common_server::MIGRATE => {
                channel.migrate_to_replacement(message.body).await?;
                self.generation = 0;
                self.window = 0;
                self.consumed_since_ack = 0;
                self.migration_activation_count = channel.migration_activation_count();
                ControlDisposition::Consumed
            }
            _ => ControlDisposition::Dispatch,
        };

        if self.window != 0 && message.header.message_type != common_server::SET_ACK {
            self.consumed_since_ack = self.consumed_since_ack.saturating_add(1);
            if self.consumed_since_ack >= self.window {
                channel.write_message(common_client::ACK, &[]).await?;
                self.consumed_since_ack = 0;
            }
        }
        Ok(disposition)
    }
}

/// Whether common control handling consumed the message body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlDisposition {
    Consumed,
    Dispatch,
}

/// Decodes a fixed little-endian u32 after the caller validated the body size.
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated fixed field"),
    )
}

/// Creates a payload-size error without retaining remote payload bytes.
fn protocol_size_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::InvalidValue,
        0,
        context,
    )
    .into()
}

/// Creates an invalid-value error for channel sequencing invariants.
fn protocol_value_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::InvalidValue,
        0,
        context,
    )
    .into()
}

/// Creates an unsupported error for valid framing without an implemented handler.
fn unsupported_protocol_value(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::Unsupported,
        0,
        context,
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxide_spice_protocol::{LINK_REPLY_FIXED_SIZE, SPICE_TICKET_PUBLIC_KEY_SIZE};
    use rsa::pkcs8::EncodePublicKey;
    use rsa::rand_core::OsRng;
    use rsa::{Oaep, RsaPrivateKey};
    use sha1::Sha1;
    use tokio::io::duplex;

    #[tokio::test]
    async fn link_negotiates_mini_header_and_auth_selection() {
        let (client_stream, mut server_stream) = duplex(8192);
        let server = tokio::spawn(async move {
            let mut header = [0; LINK_HEADER_SIZE];
            server_stream
                .read_exact(&mut header)
                .await
                .expect("link header");
            let link_header = LinkHeader::decode(&header).expect("valid client header");
            let mut link_body = vec![0; link_header.body_size as usize];
            server_stream
                .read_exact(&mut link_body)
                .await
                .expect("link body");

            let private_key = RsaPrivateKey::new(&mut OsRng, 1024).expect("test RSA key");
            let public_der = private_key
                .to_public_key()
                .to_public_key_der()
                .expect("public DER");
            assert_eq!(public_der.as_bytes().len(), SPICE_TICKET_PUBLIC_KEY_SIZE);
            let common_word: u32 = (1 << common_capability::AUTH_SELECTION)
                | (1 << common_capability::AUTH_SPICE)
                | (1 << common_capability::MINI_HEADER);
            let mut reply = vec![0; LINK_REPLY_FIXED_SIZE];
            reply[4..4 + SPICE_TICKET_PUBLIC_KEY_SIZE].copy_from_slice(public_der.as_bytes());
            reply[166..170].copy_from_slice(&1_u32.to_le_bytes());
            reply[174..178].copy_from_slice(&(LINK_REPLY_FIXED_SIZE as u32).to_le_bytes());
            reply.extend_from_slice(&common_word.to_le_bytes());
            let mut response = Vec::new();
            LinkHeader::current(reply.len() as u32).encode(&mut response);
            response.extend_from_slice(&reply);
            server_stream
                .write_all(&response)
                .await
                .expect("link reply");

            let mut mechanism = [0; 4];
            server_stream
                .read_exact(&mut mechanism)
                .await
                .expect("auth mechanism");
            assert_eq!(u32::from_le_bytes(mechanism), AUTH_MECHANISM_SPICE);
            let mut ticket = [0; 128];
            server_stream.read_exact(&mut ticket).await.expect("ticket");
            let clear = private_key
                .decrypt(Oaep::new::<Sha1>(), &ticket)
                .expect("decrypt ticket");
            assert_eq!(&clear, b"secret\0");
            server_stream
                .write_all(&(LinkError::Ok as u32).to_le_bytes())
                .await
                .expect("link result");
            let mut mini_header = [0; 6];
            server_stream
                .read_exact(&mut mini_header)
                .await
                .expect("mini header");
            assert_eq!(u16::from_le_bytes([mini_header[0], mini_header[1]]), 104);
            assert_eq!(
                u32::from_le_bytes(mini_header[2..6].try_into().expect("size field")),
                0
            );
        });

        let common_capabilities = CapabilitySet::from_bits([
            common_capability::AUTH_SELECTION,
            common_capability::AUTH_SPICE,
            common_capability::MINI_HEADER,
        ])
        .expect("known capability bits fit");
        let mut channel = link_channel(
            client_stream,
            LinkParameters {
                connection_id: 0,
                channel_type: ChannelType::Main,
                channel_id: 0,
                common_capabilities,
                channel_capabilities: CapabilitySet::new(),
                password: "secret",
                maximum_message_body: 1024,
                sasl: None,
            },
        )
        .await
        .expect("link succeeds");
        channel
            .write_message(104, &[])
            .await
            .expect("mini framed message");
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn full_header_allows_gaps_but_rejects_serial_regression() {
        let (client_stream, mut server_stream) = duplex(64);
        let server = tokio::spawn(async move {
            let mut header = Vec::new();
            DataHeader::encode(Framing::Full, 2, 103, 0, &mut header);
            DataHeader::encode(Framing::Full, 1, 103, 0, &mut header);
            server_stream
                .write_all(&header)
                .await
                .expect("full headers");
        });
        let mut channel = Channel {
            stream: client_stream,
            framing: Framing::Full,
            next_serial: 1,
            last_received_serial: 0,
            received_serial_base: 0,
            maximum_message_body: 1024,
            peer_channel_capabilities: CapabilitySet::new(),
            local_common_capabilities: CapabilitySet::new(),
            local_channel_capabilities: CapabilitySet::new(),
            write_buffer: Vec::new(),
            sasl_codec: None,
            sasl_decoded: Vec::new(),
            sasl_decoded_offset: 0,
            migration_replacements: None,
            pending_migration_replacement: None,
            migration_cancel: None,
            active_migration_generation: None,
            migration_activation_count: 0,
            last_migration_seamless: false,
            #[cfg(unix)]
            received_file_descriptors: None,
        };
        let mut body = Vec::new();

        channel
            .read_message(&mut body)
            .await
            .expect("forward serial gap is valid");
        assert_eq!(channel.received_serial(), 2);
        let error = channel
            .read_message(&mut body)
            .await
            .expect_err("serial regression must terminate the channel");
        assert_eq!(error.category(), crate::ErrorCategory::Protocol);
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn padded_server_ping_produces_fixed_size_pong() {
        let (client_stream, mut server_stream) = duplex(1024);
        let mut channel = Channel {
            stream: client_stream,
            framing: Framing::Mini,
            next_serial: 1,
            last_received_serial: 0,
            received_serial_base: 0,
            maximum_message_body: 1024,
            peer_channel_capabilities: CapabilitySet::new(),
            local_common_capabilities: CapabilitySet::new(),
            local_channel_capabilities: CapabilitySet::new(),
            write_buffer: Vec::new(),
            sasl_codec: None,
            sasl_decoded: Vec::new(),
            sasl_decoded_offset: 0,
            migration_replacements: None,
            pending_migration_replacement: None,
            migration_cancel: None,
            active_migration_generation: None,
            migration_activation_count: 0,
            last_migration_seamless: false,
            #[cfg(unix)]
            received_file_descriptors: None,
        };
        let mut ping_body = vec![0xA5; 128];
        ping_body[..4].copy_from_slice(&7_u32.to_le_bytes());
        ping_body[4..12].copy_from_slice(&11_u64.to_le_bytes());
        let expected_pong: [u8; 12] = ping_body[..12].try_into().expect("fixed ping fields");
        let incoming = IncomingMessage {
            header: DataHeader {
                serial: None,
                message_type: common_server::PING,
                body_size: ping_body.len() as u32,
                sub_list_offset: None,
            },
            body: &ping_body,
        };
        let server = tokio::spawn(async move {
            let mut response = [0; 18];
            server_stream
                .read_exact(&mut response)
                .await
                .expect("fixed pong response");
            assert_eq!(
                u16::from_le_bytes(response[..2].try_into().expect("pong type")),
                common_client::PONG
            );
            assert_eq!(
                u32::from_le_bytes(response[2..6].try_into().expect("pong size")),
                12
            );
            assert_eq!(&response[6..], &expected_pong);
        });

        let disposition = ControlState::new()
            .handle(&mut channel, &incoming)
            .await
            .expect("padded ping is valid");
        assert_eq!(disposition, ControlDisposition::Consumed);
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn migration_swaps_transport_and_forwards_opaque_state_first() {
        let (old_stream, mut old_server) = duplex(1024);
        let (replacement_stream, mut replacement_server) = duplex(1024);
        let mut channel = test_channel(old_stream);
        channel.last_received_serial = 7;
        let replacement = test_channel(replacement_stream);
        let (replacement_sender, replacement_receiver) = mpsc::channel(1);
        let active_generation = Arc::new(AtomicU64::new(1));
        let (_cancel_sender, cancel_receiver) = watch::channel(false);
        channel.install_migration_path(replacement_receiver, cancel_receiver, active_generation);
        replacement_sender
            .send(MigrationReplacement {
                generation: 1,
                seamless: true,
                activate_immediately: false,
                channel: replacement,
            })
            .await
            .expect("queue replacement");
        let old_peer = tokio::spawn(async move {
            let migration_data = [9, 8, 7];
            let mut wire = Vec::new();
            DataHeader::encode(
                Framing::Mini,
                1,
                common_server::MIGRATE_DATA,
                migration_data.len() as u32,
                &mut wire,
            );
            wire.extend_from_slice(&migration_data);
            old_server
                .write_all(&wire)
                .await
                .expect("source migration data");
        });
        let migrate_flags = (1_u32 << 1).to_le_bytes();
        let incoming = IncomingMessage {
            header: DataHeader {
                serial: None,
                message_type: common_server::MIGRATE,
                body_size: 4,
                sub_list_offset: None,
            },
            body: &migrate_flags,
        };
        let mut control = ControlState::new();
        assert_eq!(
            control
                .handle(&mut channel, &incoming)
                .await
                .expect("migration succeeds"),
            ControlDisposition::Consumed
        );
        assert_eq!(channel.migration_activation_count(), 1);
        assert_eq!(channel.received_serial(), 7);

        let mut header = [0; 6];
        replacement_server
            .read_exact(&mut header)
            .await
            .expect("target migration header");
        assert_eq!(
            u16::from_le_bytes(header[..2].try_into().expect("message type")),
            common_client::MIGRATE_DATA
        );
        let body_size = u32::from_le_bytes(header[2..].try_into().expect("message size"));
        let mut body = vec![0; body_size as usize];
        replacement_server
            .read_exact(&mut body)
            .await
            .expect("target migration data");
        assert_eq!(body, [9, 8, 7]);
        old_peer.await.expect("source peer");
    }

    #[tokio::test]
    async fn immediate_replacement_does_not_wait_for_source_close() {
        let (old_stream, _old_server) = duplex(1024);
        let (replacement_stream, mut replacement_server) = duplex(1024);
        let mut channel = test_channel(old_stream);
        let replacement = test_channel(replacement_stream);
        let (replacement_sender, replacement_receiver) = mpsc::channel(1);
        let active_generation = Arc::new(AtomicU64::new(1));
        let (_cancel_sender, cancel_receiver) = watch::channel(false);
        channel.install_migration_path(replacement_receiver, cancel_receiver, active_generation);
        replacement_sender
            .send(MigrationReplacement {
                generation: 1,
                seamless: false,
                activate_immediately: true,
                channel: replacement,
            })
            .await
            .expect("queue immediate replacement");
        let target_peer = tokio::spawn(async move {
            let body = [4, 5, 6];
            let mut wire = Vec::new();
            DataHeader::encode(Framing::Mini, 1, 177, body.len() as u32, &mut wire);
            wire.extend_from_slice(&body);
            replacement_server
                .write_all(&wire)
                .await
                .expect("target message");
        });

        let mut body = Vec::new();
        let header = channel
            .read_message(&mut body)
            .await
            .expect("immediate replacement message");
        assert_eq!(header.message_type, 177);
        assert_eq!(body, [4, 5, 6]);
        assert_eq!(channel.migration_activation_count(), 1);
        let mut observed_activation = 0;
        assert_eq!(
            channel.observe_migration_activation(&mut observed_activation),
            Some(false)
        );
        target_peer.await.expect("target peer");
    }

    #[tokio::test]
    async fn progress_registry_releases_cross_channel_wait_and_rejects_self_deadlock() {
        let requester = ChannelIdentity {
            channel_type: ChannelType::Display,
            channel_id: 0,
        };
        let target = ChannelIdentity {
            channel_type: ChannelType::Display,
            channel_id: 1,
        };
        let registry =
            ProgressRegistry::new([(requester, 0), (target, 0)]).expect("unique progress channels");
        let waits = WaitForChannels {
            waits: vec![oxide_spice_protocol::ChannelWait {
                channel_type: target.channel_type,
                channel_id: target.channel_id,
                message_serial: 5,
            }],
        };
        let (cancel_sender, mut cancel_receiver) = watch::channel(false);
        let waiter_registry = registry.clone();
        let waiter = tokio::spawn(async move {
            waiter_registry
                .wait_for(requester, 1, &waits, &mut cancel_receiver)
                .await
        });
        tokio::task::yield_now().await;
        registry.complete(target, 5).expect("target completion");
        waiter
            .await
            .expect("wait task")
            .expect("cross-channel wait released");
        drop(cancel_sender);

        let self_wait = WaitForChannels {
            waits: vec![oxide_spice_protocol::ChannelWait {
                channel_type: requester.channel_type,
                channel_id: requester.channel_id,
                message_serial: 2,
            }],
        };
        let (_cancel_sender, mut cancel_receiver) = watch::channel(false);
        let error = registry
            .wait_for(requester, 2, &self_wait, &mut cancel_receiver)
            .await
            .expect_err("self wait cannot make progress");
        assert_eq!(error.category(), crate::ErrorCategory::Protocol);
    }

    fn test_channel(stream: tokio::io::DuplexStream) -> Channel<tokio::io::DuplexStream> {
        Channel {
            stream,
            framing: Framing::Mini,
            next_serial: 1,
            last_received_serial: 0,
            received_serial_base: 0,
            maximum_message_body: 1024,
            peer_channel_capabilities: CapabilitySet::new(),
            local_common_capabilities: CapabilitySet::new(),
            local_channel_capabilities: CapabilitySet::new(),
            write_buffer: Vec::new(),
            sasl_codec: None,
            sasl_decoded: Vec::new(),
            sasl_decoded_offset: 0,
            migration_replacements: None,
            pending_migration_replacement: None,
            migration_cancel: None,
            active_migration_generation: None,
            migration_activation_count: 0,
            last_migration_seamless: false,
            #[cfg(unix)]
            received_file_descriptors: None,
        }
    }
}
