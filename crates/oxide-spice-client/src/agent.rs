//! Bounded host API and task-owned state for the Main-channel Agent tunnel.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oxide_spice_protocol::{
    AGENT_PROTOCOL, AgentAudioVolumeSync, AgentCapabilities, AgentClipboardSelection,
    AgentClipboardType, AgentDisplayDevice, AgentFileTransferFailure, AgentFileTransferStatus,
    AgentMessage, AgentMonitorConfig, AgentStreamDecoder, CapabilitySet, OutboundAgentMessage,
    agent_capability, agent_message, decode_audio_volume_sync, decode_clipboard_data,
    decode_clipboard_grab, decode_clipboard_release, decode_clipboard_request,
    decode_file_transfer_failure, decode_file_transfer_status, decode_graphics_device_info,
    encode_agent_tokens, encode_audio_volume_sync, encode_clipboard_data, encode_clipboard_grab,
    encode_clipboard_release, encode_clipboard_request, encode_file_transfer_data,
    encode_file_transfer_start, encode_file_transfer_status, encode_monitors_config, main_client,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::ClientError;
use crate::channel::Channel;

const AGENT_COMMAND_QUEUE_CAPACITY: usize = 16;
const MAX_ACTIVE_FILE_TRANSFERS: usize = 8;
pub(crate) const AGENT_RECEIVE_WINDOW: u32 = 10;
const AGENT_EVENT_QUEUE_CAPACITY: usize = AGENT_RECEIVE_WINDOW as usize;
pub const MAX_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;

/// Agent capabilities relevant to the implemented host API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFeatures {
    pub clipboard_by_demand: bool,
    pub clipboard_selection: bool,
    pub clipboard_grab_serial: bool,
    pub monitor_config: bool,
    pub sparse_monitors: bool,
    pub monitor_positions: bool,
    pub monitor_physical_size: bool,
    pub file_transfer_disabled: bool,
    pub file_transfer_detailed_errors: bool,
    pub audio_volume_sync: bool,
    pub graphics_device_info: bool,
}

/// Latest guest audio volume update for one direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAudioVolumeState {
    pub connection_generation: u64,
    pub agent_generation: u64,
    pub is_playback: bool,
    pub muted: bool,
    pub volumes: Arc<[u16]>,
}

/// Latest guest mapping from SPICE displays to graphics devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentGraphicsDeviceState {
    pub connection_generation: u64,
    pub agent_generation: u64,
    pub displays: Arc<[AgentDisplayDevice]>,
}

/// Dynamic guest Agent state within one Main connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Disconnected {
        connection_generation: u64,
        agent_generation: u64,
        reason: Option<u32>,
    },
    Negotiating {
        connection_generation: u64,
        agent_generation: u64,
    },
    Ready {
        connection_generation: u64,
        agent_generation: u64,
        features: AgentFeatures,
    },
}

impl AgentState {
    pub const fn agent_generation(&self) -> u64 {
        match self {
            Self::Disconnected {
                agent_generation, ..
            }
            | Self::Negotiating {
                agent_generation, ..
            }
            | Self::Ready {
                agent_generation, ..
            } => *agent_generation,
        }
    }

    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

/// Latest remote ownership offer and its advertised wire formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardOffer {
    pub connection_generation: u64,
    pub agent_generation: u64,
    pub revision: u64,
    pub selection: AgentClipboardSelection,
    pub types: Arc<[u32]>,
}

impl ClipboardOffer {
    pub fn supports(&self, clipboard_type: AgentClipboardType) -> bool {
        self.types.contains(&(clipboard_type as u32))
    }
}

/// Latest independent ownership offers for all negotiated selections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClipboardOffers {
    values: [Option<ClipboardOffer>; 3],
}

impl ClipboardOffers {
    pub fn get(&self, selection: AgentClipboardSelection) -> Option<ClipboardOffer> {
        self.values[selection as usize].clone()
    }

    pub(crate) fn replace(&mut self, offer: ClipboardOffer) {
        let index = offer.selection as usize;
        self.values[index] = Some(offer);
    }

    pub(crate) fn clear(&mut self, selection: AgentClipboardSelection) {
        self.values[selection as usize] = None;
    }

    fn snapshot(&self) -> [Option<ClipboardOffer>; 3] {
        self.values.clone()
    }
}

/// One reliable request for a clipboard format currently owned by the host.
#[derive(Debug)]
pub struct ClipboardRequest {
    pub request_id: u64,
    pub connection_generation: u64,
    pub agent_generation: u64,
    pub selection: AgentClipboardSelection,
    pub clipboard_type: AgentClipboardType,
    pub(crate) _credit: Option<Arc<InboundAgentCredit>>,
}

/// Reliable Agent events that require a host response.
#[derive(Debug)]
pub enum AgentEvent {
    ClipboardRequested(ClipboardRequest),
}

/// Single-consumer reliable Agent event stream.
pub struct AgentEvents {
    receiver: mpsc::Receiver<AgentEvent>,
}

impl std::fmt::Debug for AgentEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentEvents")
            .finish_non_exhaustive()
    }
}

impl AgentEvents {
    pub async fn next(&mut self) -> Result<AgentEvent, AgentSendError> {
        self.receiver.recv().await.ok_or(AgentSendError::Closed)
    }
}

/// Guest monitor geometry requested through the Agent protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestMonitor {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub x: i32,
    pub y: i32,
    pub width_mm: Option<u16>,
    pub height_mm: Option<u16>,
}

impl From<GuestMonitor> for AgentMonitorConfig {
    fn from(value: GuestMonitor) -> Self {
        Self {
            width: value.width,
            height: value.height,
            depth: value.depth,
            x: value.x,
            y: value.y,
            width_mm: value.width_mm,
            height_mm: value.height_mm,
        }
    }
}

/// Latest desired guest monitor layout, coalesced before wire fragmentation begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestMonitorLayout {
    pub monitors: Arc<[GuestMonitor]>,
}

/// Host-facing Agent submission failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AgentSendError {
    #[error("guest Agent is not ready")]
    Unavailable,
    #[error("guest Agent does not support the requested operation")]
    Unsupported,
    #[error("Agent channel is closed")]
    Closed,
    #[error("Agent generation changed before the operation completed")]
    StaleGeneration,
    #[error("Agent payload exceeds the configured bound")]
    ResourceLimit,
    #[error("Agent payload is invalid")]
    InvalidData,
}

/// Host-provided file metadata without filesystem ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTransferMetadata {
    pub file_name: String,
    pub size: u64,
}

/// Observable state for one outgoing file transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTransferState {
    WaitingForGuest,
    Sending { accepted_bytes: u64 },
    AwaitingCompletion { accepted_bytes: u64 },
    Completed,
    Cancelled,
    Failed { failure: AgentFileTransferFailure },
    AgentDisconnected,
}

/// Unique streaming owner for one outgoing file transfer.
pub struct OutgoingFileTransfer {
    transfer_id: u32,
    agent_generation: u64,
    declared_size: u64,
    accepted_bytes: u64,
    command_sender: mpsc::Sender<AgentCommand>,
    state: watch::Receiver<FileTransferState>,
    terminal: bool,
}

impl std::fmt::Debug for OutgoingFileTransfer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutgoingFileTransfer")
            .field("transfer_id", &self.transfer_id)
            .field("declared_size", &self.declared_size)
            .field("accepted_bytes", &self.accepted_bytes)
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

impl OutgoingFileTransfer {
    pub const fn id(&self) -> u32 {
        self.transfer_id
    }

    pub fn state(&self) -> FileTransferState {
        *self.state.borrow()
    }

    /// Waits for the next guest-visible transfer state and acknowledges terminal ownership.
    pub async fn changed(&mut self) -> Result<FileTransferState, AgentSendError> {
        self.state
            .changed()
            .await
            .map_err(|_| AgentSendError::Closed)?;
        let state = *self.state.borrow_and_update();
        if matches!(
            state,
            FileTransferState::Completed
                | FileTransferState::Cancelled
                | FileTransferState::Failed { .. }
                | FileTransferState::AgentDisconnected
        ) {
            self.terminal = true;
        }
        Ok(state)
    }

    pub async fn wait_until_sending(&mut self) -> Result<(), AgentSendError> {
        loop {
            let current = *self.state.borrow_and_update();
            match current {
                FileTransferState::Sending { .. } => return Ok(()),
                FileTransferState::Cancelled
                | FileTransferState::Failed { .. }
                | FileTransferState::AgentDisconnected => {
                    return Err(AgentSendError::StaleGeneration);
                }
                FileTransferState::Completed | FileTransferState::AwaitingCompletion { .. } => {
                    return Err(AgentSendError::InvalidData);
                }
                FileTransferState::WaitingForGuest => {
                    self.state
                        .changed()
                        .await
                        .map_err(|_| AgentSendError::Closed)?;
                }
            }
        }
    }

    pub async fn send_chunk(&mut self, bytes: &[u8]) -> Result<(), AgentSendError> {
        if bytes.is_empty()
            || bytes.len() > oxide_spice_protocol::MAX_AGENT_FILE_CHUNK_BYTES
            || !matches!(*self.state.borrow(), FileTransferState::Sending { .. })
        {
            return Err(AgentSendError::InvalidData);
        }
        let byte_count = u64::try_from(bytes.len()).map_err(|_| AgentSendError::ResourceLimit)?;
        let accepted_bytes = self
            .accepted_bytes
            .checked_add(byte_count)
            .filter(|total| *total <= self.declared_size)
            .ok_or(AgentSendError::ResourceLimit)?;
        let (completion, completed) = oneshot::channel();
        self.command_sender
            .send(AgentCommand::FileData {
                agent_generation: self.agent_generation,
                transfer_id: self.transfer_id,
                data: bytes.to_vec().into_boxed_slice(),
                completion,
            })
            .await
            .map_err(|_| AgentSendError::Closed)?;
        completed.await.map_err(|_| AgentSendError::Closed)??;
        self.accepted_bytes = accepted_bytes;
        Ok(())
    }

    pub async fn finish(self) -> Result<(), AgentSendError> {
        match self.finish_with_state().await? {
            FileTransferState::Completed => Ok(()),
            FileTransferState::Cancelled | FileTransferState::AgentDisconnected => {
                Err(AgentSendError::StaleGeneration)
            }
            FileTransferState::Failed { .. } => Err(AgentSendError::InvalidData),
            FileTransferState::WaitingForGuest
            | FileTransferState::Sending { .. }
            | FileTransferState::AwaitingCompletion { .. } => {
                unreachable!("finish_with_state returns only a terminal state")
            }
        }
    }

    /// Finishes the byte stream and preserves the guest's terminal status for host reporting.
    pub async fn finish_with_state(mut self) -> Result<FileTransferState, AgentSendError> {
        if self.accepted_bytes != self.declared_size {
            return Err(AgentSendError::InvalidData);
        }
        if self.declared_size == 0 {
            let (completion, completed) = oneshot::channel();
            self.command_sender
                .send(AgentCommand::FileData {
                    agent_generation: self.agent_generation,
                    transfer_id: self.transfer_id,
                    data: Box::new([]),
                    completion,
                })
                .await
                .map_err(|_| AgentSendError::Closed)?;
            completed.await.map_err(|_| AgentSendError::Closed)??;
        }
        loop {
            let current = *self.state.borrow_and_update();
            match current {
                FileTransferState::Completed
                | FileTransferState::Cancelled
                | FileTransferState::Failed { .. }
                | FileTransferState::AgentDisconnected => {
                    self.terminal = true;
                    return Ok(current);
                }
                _ => self
                    .state
                    .changed()
                    .await
                    .map_err(|_| AgentSendError::Closed)?,
            }
        }
    }

    pub async fn cancel(mut self) -> Result<(), AgentSendError> {
        let (completion, completed) = oneshot::channel();
        self.command_sender
            .send(AgentCommand::CancelFile {
                agent_generation: self.agent_generation,
                transfer_id: self.transfer_id,
                completion,
            })
            .await
            .map_err(|_| AgentSendError::Closed)?;
        completed.await.map_err(|_| AgentSendError::Closed)??;
        self.terminal = true;
        Ok(())
    }
}

impl Drop for OutgoingFileTransfer {
    fn drop(&mut self) {
        if self.terminal {
            return;
        }
        let (completion, _) = oneshot::channel();
        let _ = self.command_sender.try_send(AgentCommand::CancelFile {
            agent_generation: self.agent_generation,
            transfer_id: self.transfer_id,
            completion,
        });
    }
}

pub(crate) enum AgentCommand {
    SyncAudioVolume {
        agent_generation: u64,
        volume: AgentAudioVolumeSync,
    },
    OfferClipboard {
        agent_generation: u64,
        selection: AgentClipboardSelection,
        types: Arc<[AgentClipboardType]>,
    },
    ReleaseClipboard {
        agent_generation: u64,
        selection: AgentClipboardSelection,
    },
    RequestClipboard {
        agent_generation: u64,
        selection: AgentClipboardSelection,
        clipboard_type: AgentClipboardType,
        response: oneshot::Sender<Result<Arc<[u8]>, AgentSendError>>,
    },
    ProvideClipboard {
        agent_generation: u64,
        request_id: u64,
        data: Arc<[u8]>,
    },
    StartFile {
        agent_generation: u64,
        metadata: FileTransferMetadata,
        response: oneshot::Sender<Result<FileTransferRegistration, AgentSendError>>,
    },
    FileData {
        agent_generation: u64,
        transfer_id: u32,
        data: Box<[u8]>,
        completion: oneshot::Sender<Result<(), AgentSendError>>,
    },
    CancelFile {
        agent_generation: u64,
        transfer_id: u32,
        completion: oneshot::Sender<Result<(), AgentSendError>>,
    },
}

pub(crate) struct FileTransferRegistration {
    transfer_id: u32,
    state: watch::Receiver<FileTransferState>,
}

/// Cloneable command and latest-state handle that never owns the Main task.
#[derive(Clone)]
pub struct AgentHandle {
    command_sender: mpsc::Sender<AgentCommand>,
    state_receiver: watch::Receiver<AgentState>,
    offer_receiver: watch::Receiver<ClipboardOffers>,
    layout_sender: watch::Sender<Option<GuestMonitorLayout>>,
    audio_volume_receiver: watch::Receiver<Option<AgentAudioVolumeState>>,
    graphics_device_receiver: watch::Receiver<Option<AgentGraphicsDeviceState>>,
}

impl std::fmt::Debug for AgentHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentHandle")
            .field("state", &*self.state_receiver.borrow())
            .finish_non_exhaustive()
    }
}

impl AgentHandle {
    pub fn state(&self) -> AgentState {
        self.state_receiver.borrow().clone()
    }

    pub fn clipboard_offer(&self, selection: AgentClipboardSelection) -> Option<ClipboardOffer> {
        self.offer_receiver.borrow().get(selection)
    }

    pub fn clipboard_offers(&self) -> [Option<ClipboardOffer>; 3] {
        self.offer_receiver.borrow().snapshot()
    }

    pub async fn clipboard_offers_changed(
        &mut self,
    ) -> Result<[Option<ClipboardOffer>; 3], AgentSendError> {
        self.offer_receiver
            .changed()
            .await
            .map_err(|_| AgentSendError::Closed)?;
        Ok(self.offer_receiver.borrow_and_update().snapshot())
    }

    pub async fn changed(&mut self) -> Result<AgentState, AgentSendError> {
        self.state_receiver
            .changed()
            .await
            .map_err(|_| AgentSendError::Closed)?;
        Ok(self.state_receiver.borrow_and_update().clone())
    }

    pub fn audio_volume(&self) -> Option<AgentAudioVolumeState> {
        self.audio_volume_receiver.borrow().clone()
    }

    /// Waits for the next guest audio-volume update or reset.
    pub async fn audio_volume_changed(
        &mut self,
    ) -> Result<Option<AgentAudioVolumeState>, AgentSendError> {
        self.audio_volume_receiver
            .changed()
            .await
            .map_err(|_| AgentSendError::Closed)?;
        Ok(self.audio_volume_receiver.borrow_and_update().clone())
    }

    pub fn graphics_devices(&self) -> Option<AgentGraphicsDeviceState> {
        self.graphics_device_receiver.borrow().clone()
    }

    /// Waits for the next guest graphics-device mapping or reset.
    pub async fn graphics_devices_changed(
        &mut self,
    ) -> Result<Option<AgentGraphicsDeviceState>, AgentSendError> {
        self.graphics_device_receiver
            .changed()
            .await
            .map_err(|_| AgentSendError::Closed)?;
        Ok(self.graphics_device_receiver.borrow_and_update().clone())
    }

    pub async fn sync_audio_volume(
        &self,
        is_playback: bool,
        muted: bool,
        volumes: impl Into<Vec<u16>>,
    ) -> Result<(), AgentSendError> {
        let state = self.state_receiver.borrow().clone();
        let AgentState::Ready {
            agent_generation,
            features,
            ..
        } = state
        else {
            return Err(AgentSendError::Unavailable);
        };
        if !features.audio_volume_sync {
            return Err(AgentSendError::Unsupported);
        }
        let volume = AgentAudioVolumeSync {
            is_playback,
            muted,
            volumes: volumes.into(),
        };
        encode_audio_volume_sync(&volume).map_err(|error| match error.kind {
            oxide_spice_protocol::DecodeErrorKind::ResourceLimit => AgentSendError::ResourceLimit,
            _ => AgentSendError::InvalidData,
        })?;
        self.send(AgentCommand::SyncAudioVolume {
            agent_generation,
            volume,
        })
        .await
    }

    pub async fn wait_ready(&self) -> Result<AgentState, AgentSendError> {
        let mut state = self.state_receiver.clone();
        loop {
            let current = state.borrow_and_update().clone();
            if current.is_ready() {
                return Ok(current);
            }
            state.changed().await.map_err(|_| AgentSendError::Closed)?;
        }
    }

    pub async fn wait_ready_after(
        &self,
        agent_generation: u64,
    ) -> Result<AgentState, AgentSendError> {
        let mut state = self.state_receiver.clone();
        loop {
            let current = state.borrow_and_update().clone();
            if current.is_ready() && current.agent_generation() > agent_generation {
                return Ok(current);
            }
            state.changed().await.map_err(|_| AgentSendError::Closed)?;
        }
    }

    pub async fn wait_clipboard_offer(
        &self,
        selection: AgentClipboardSelection,
    ) -> Result<ClipboardOffer, AgentSendError> {
        let mut offers = self.offer_receiver.clone();
        loop {
            if let Some(offer) = offers.borrow_and_update().get(selection) {
                return Ok(offer);
            }
            offers.changed().await.map_err(|_| AgentSendError::Closed)?;
        }
    }

    pub async fn offer_clipboard_text(
        &self,
        selection: AgentClipboardSelection,
    ) -> Result<(), AgentSendError> {
        self.offer_clipboard(selection, Arc::from([AgentClipboardType::Utf8Text]))
            .await
    }

    pub async fn offer_clipboard(
        &self,
        selection: AgentClipboardSelection,
        types: Arc<[AgentClipboardType]>,
    ) -> Result<(), AgentSendError> {
        let generation = self.ready_generation_for_selection(selection)?;
        if types.is_empty()
            || types.len() > oxide_spice_protocol::MAX_AGENT_CLIPBOARD_TYPES
            || types.contains(&AgentClipboardType::None)
        {
            return Err(AgentSendError::InvalidData);
        }
        self.send(AgentCommand::OfferClipboard {
            agent_generation: generation,
            selection,
            types,
        })
        .await
    }

    pub async fn release_clipboard(
        &self,
        selection: AgentClipboardSelection,
    ) -> Result<(), AgentSendError> {
        let generation = self.ready_generation_for_selection(selection)?;
        self.send(AgentCommand::ReleaseClipboard {
            agent_generation: generation,
            selection,
        })
        .await
    }

    pub async fn request_clipboard_text(
        &self,
        selection: AgentClipboardSelection,
    ) -> Result<Arc<str>, AgentSendError> {
        let data = self
            .request_clipboard(selection, AgentClipboardType::Utf8Text)
            .await?;
        let text = std::str::from_utf8(&data).map_err(|_| AgentSendError::InvalidData)?;
        Ok(Arc::from(text))
    }

    pub async fn request_clipboard(
        &self,
        selection: AgentClipboardSelection,
        clipboard_type: AgentClipboardType,
    ) -> Result<Arc<[u8]>, AgentSendError> {
        let generation = self.ready_generation_for_selection(selection)?;
        let offer = self
            .clipboard_offer(selection)
            .ok_or(AgentSendError::Unavailable)?;
        if !offer.supports(clipboard_type) || clipboard_type == AgentClipboardType::None {
            return Err(AgentSendError::Unsupported);
        }
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(AgentCommand::RequestClipboard {
            agent_generation: generation,
            selection,
            clipboard_type,
            response: response_sender,
        })
        .await?;
        response_receiver
            .await
            .map_err(|_| AgentSendError::Closed)?
    }

    pub async fn provide_clipboard_text(
        &self,
        request_id: u64,
        text: impl Into<Arc<str>>,
    ) -> Result<(), AgentSendError> {
        let text = text.into();
        self.provide_clipboard(request_id, Arc::from(text.as_bytes()))
            .await
    }

    pub async fn provide_clipboard(
        &self,
        request_id: u64,
        data: Arc<[u8]>,
    ) -> Result<(), AgentSendError> {
        let generation = self.ready_generation()?;
        if data.len() > MAX_CLIPBOARD_BYTES {
            return Err(AgentSendError::ResourceLimit);
        }
        self.send(AgentCommand::ProvideClipboard {
            agent_generation: generation,
            request_id,
            data,
        })
        .await
    }

    pub fn set_monitor_layout(&self, layout: GuestMonitorLayout) -> Result<(), AgentSendError> {
        if layout.monitors.len() > oxide_spice_protocol::MAX_AGENT_MONITORS {
            return Err(AgentSendError::ResourceLimit);
        }
        if layout
            .monitors
            .iter()
            .any(|monitor| monitor.depth == 0 || monitor.depth > 32)
        {
            return Err(AgentSendError::InvalidData);
        }
        if let AgentState::Ready { features, .. } = &*self.state_receiver.borrow()
            && !features.sparse_monitors
            && layout
                .monitors
                .iter()
                .any(|monitor| monitor.width == 0 || monitor.height == 0)
        {
            return Err(AgentSendError::Unsupported);
        }
        self.layout_sender
            .send(Some(layout))
            .map_err(|_| AgentSendError::Closed)
    }

    pub async fn start_file_transfer(
        &self,
        metadata: FileTransferMetadata,
    ) -> Result<OutgoingFileTransfer, AgentSendError> {
        let state = self.state_receiver.borrow().clone();
        let AgentState::Ready {
            agent_generation,
            features,
            ..
        } = state
        else {
            return Err(AgentSendError::Unavailable);
        };
        if features.file_transfer_disabled {
            return Err(AgentSendError::Unsupported);
        }
        oxide_spice_protocol::encode_file_transfer_start(1, &metadata.file_name, metadata.size)
            .map_err(|error| match error.kind {
                oxide_spice_protocol::DecodeErrorKind::ResourceLimit => {
                    AgentSendError::ResourceLimit
                }
                _ => AgentSendError::InvalidData,
            })?;
        let declared_size = metadata.size;
        let (response, registered) = oneshot::channel();
        self.send(AgentCommand::StartFile {
            agent_generation,
            metadata,
            response,
        })
        .await?;
        let registration = registered.await.map_err(|_| AgentSendError::Closed)??;
        Ok(OutgoingFileTransfer {
            transfer_id: registration.transfer_id,
            agent_generation,
            declared_size,
            accepted_bytes: 0,
            command_sender: self.command_sender.clone(),
            state: registration.state,
            terminal: false,
        })
    }

    fn ready_generation(&self) -> Result<u64, AgentSendError> {
        let state = self.state_receiver.borrow();
        if !state.is_ready() {
            return Err(AgentSendError::Unavailable);
        }
        Ok(state.agent_generation())
    }

    fn ready_generation_for_selection(
        &self,
        selection: AgentClipboardSelection,
    ) -> Result<u64, AgentSendError> {
        let state = self.state_receiver.borrow();
        let AgentState::Ready {
            agent_generation,
            features,
            ..
        } = &*state
        else {
            return Err(AgentSendError::Unavailable);
        };
        if !features.clipboard_by_demand
            || selection != AgentClipboardSelection::Clipboard && !features.clipboard_selection
        {
            return Err(AgentSendError::Unsupported);
        }
        Ok(*agent_generation)
    }

    async fn send(&self, command: AgentCommand) -> Result<(), AgentSendError> {
        self.command_sender
            .send(command)
            .await
            .map_err(|_| AgentSendError::Closed)
    }
}

pub(crate) struct AgentTaskPaths {
    pub commands: mpsc::Receiver<AgentCommand>,
    pub state: watch::Sender<AgentState>,
    pub offers: watch::Sender<ClipboardOffers>,
    pub layout: watch::Receiver<Option<GuestMonitorLayout>>,
    pub events: mpsc::Sender<AgentEvent>,
    pub audio_volume: watch::Sender<Option<AgentAudioVolumeState>>,
    pub graphics_devices: watch::Sender<Option<AgentGraphicsDeviceState>>,
    pub credit_returns: Arc<AgentCreditReturns>,
}

#[derive(Debug)]
pub(crate) struct AgentCreditReturns {
    state: Mutex<AgentCreditReturnState>,
    pub notify: Notify,
}

#[derive(Debug)]
struct AgentCreditReturnState {
    generation: u64,
    pending: u32,
}

impl AgentCreditReturns {
    fn new() -> Self {
        Self {
            state: Mutex::new(AgentCreditReturnState {
                generation: 0,
                pending: 0,
            }),
            notify: Notify::new(),
        }
    }

    /// Atomically changes generation and discards credits from the prior Agent stream.
    pub fn reset(&self, generation: u64) {
        let mut state = self.state.lock().expect("Agent credit return state");
        state.generation = generation;
        state.pending = 0;
    }

    /// Returns one fragment credit only while its originating generation remains current.
    pub fn return_now(&self, generation: u64) {
        let returned = {
            let mut state = self.state.lock().expect("Agent credit return state");
            if state.generation != generation {
                false
            } else {
                state.pending = state
                    .pending
                    .checked_add(1)
                    .expect("bounded Agent receive window");
                true
            }
        };
        if returned {
            self.notify.notify_one();
        }
    }

    /// Takes all credits accumulated for the active Agent generation.
    pub fn take(&self, generation: u64) -> u32 {
        let mut state = self.state.lock().expect("Agent credit return state");
        if state.generation != generation {
            return 0;
        }
        std::mem::take(&mut state.pending)
    }
}

#[derive(Debug)]
pub(crate) struct InboundAgentCredit {
    generation: u64,
    returns: Arc<AgentCreditReturns>,
}

impl Drop for InboundAgentCredit {
    fn drop(&mut self) {
        self.returns.return_now(self.generation);
    }
}

pub(crate) fn inbound_credit(
    returns: Arc<AgentCreditReturns>,
    generation: u64,
) -> Arc<InboundAgentCredit> {
    Arc::new(InboundAgentCredit {
        generation,
        returns,
    })
}

pub(crate) fn agent_paths(
    connection_generation: u64,
) -> (AgentHandle, AgentEvents, AgentTaskPaths) {
    let (command_sender, commands) = mpsc::channel(AGENT_COMMAND_QUEUE_CAPACITY);
    let (state, state_receiver) = watch::channel(AgentState::Disconnected {
        connection_generation,
        agent_generation: 0,
        reason: None,
    });
    let (offers, offer_receiver) = watch::channel(ClipboardOffers::default());
    let (layout_sender, layout) = watch::channel(None);
    let (events, event_receiver) = mpsc::channel(AGENT_EVENT_QUEUE_CAPACITY);
    let (audio_volume, audio_volume_receiver) = watch::channel(None);
    let (graphics_devices, graphics_device_receiver) = watch::channel(None);
    let credit_returns = Arc::new(AgentCreditReturns::new());
    (
        AgentHandle {
            command_sender,
            state_receiver,
            offer_receiver,
            layout_sender,
            audio_volume_receiver,
            graphics_device_receiver,
        },
        AgentEvents {
            receiver: event_receiver,
        },
        AgentTaskPaths {
            commands,
            state,
            offers,
            layout,
            events,
            audio_volume,
            graphics_devices,
            credit_returns,
        },
    )
}

pub(crate) fn features_from_capabilities(capabilities: &CapabilitySet) -> AgentFeatures {
    use oxide_spice_protocol::agent_capability;

    AgentFeatures {
        clipboard_by_demand: capabilities.contains(agent_capability::CLIPBOARD_BY_DEMAND),
        clipboard_selection: capabilities.contains(agent_capability::CLIPBOARD_SELECTION),
        clipboard_grab_serial: capabilities.contains(agent_capability::CLIPBOARD_GRAB_SERIAL),
        monitor_config: capabilities.contains(agent_capability::MONITORS_CONFIG),
        sparse_monitors: capabilities.contains(agent_capability::SPARSE_MONITORS_CONFIG),
        monitor_positions: capabilities.contains(agent_capability::MONITORS_CONFIG_POSITION),
        monitor_physical_size: capabilities.contains(agent_capability::MONITORS_PHYSICAL_SIZE),
        file_transfer_disabled: capabilities.contains(agent_capability::FILE_TRANSFER_DISABLED),
        file_transfer_detailed_errors: capabilities
            .contains(agent_capability::FILE_TRANSFER_DETAILED_ERRORS),
        audio_volume_sync: capabilities.contains(agent_capability::AUDIO_VOLUME_SYNC),
        graphics_device_info: capabilities.contains(agent_capability::GRAPHICS_DEVICE_INFO),
    }
}

struct PendingClipboardRead {
    generation: u64,
    clipboard_type: AgentClipboardType,
    response: oneshot::Sender<Result<Arc<[u8]>, AgentSendError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferPhase {
    WaitingForGuest,
    Sending,
    AwaitingCompletion,
}

struct TransferRuntime {
    generation: u64,
    total_bytes: u64,
    accepted_bytes: u64,
    phase: TransferPhase,
    state: watch::Sender<FileTransferState>,
}

struct AgentSinks {
    state: watch::Sender<AgentState>,
    offers: watch::Sender<ClipboardOffers>,
    events: mpsc::Sender<AgentEvent>,
    audio_volume: watch::Sender<Option<AgentAudioVolumeState>>,
    graphics_devices: watch::Sender<Option<AgentGraphicsDeviceState>>,
    credit_returns: Arc<AgentCreditReturns>,
}

/// Main-task state for one dynamically reconnectable Agent byte stream.
pub(crate) struct AgentRuntime {
    connection_generation: u64,
    generation: u64,
    connected: bool,
    ready: bool,
    outbound_tokens: u64,
    inbound_tokens: u32,
    decoder: AgentStreamDecoder,
    decoded_messages: Vec<AgentMessage>,
    peer_capabilities: CapabilitySet,
    outbound: Option<OutboundAgentMessage>,
    outbound_fragment: Vec<u8>,
    outbound_completion: Option<oneshot::Sender<Result<(), AgentSendError>>>,
    announce_pending: bool,
    max_clipboard_pending: bool,
    sent_layout_generation: u64,
    layout_dirty: bool,
    offer_revision: u64,
    outgoing_clipboard_serial: [u32; 3],
    incoming_clipboard_serial: [u32; 3],
    desired_clipboard_offers: [Option<Arc<[AgentClipboardType]>>; 3],
    sent_offer_generation: [u64; 3],
    next_request_id: u64,
    pending_reads: HashMap<AgentClipboardSelection, PendingClipboardRead>,
    pending_host_requests: HashMap<u64, (u64, AgentClipboardSelection, AgentClipboardType)>,
    next_transfer_id: u32,
    transfers: HashMap<u32, TransferRuntime>,
    sinks: AgentSinks,
}

impl AgentRuntime {
    pub fn new(
        connection_generation: u64,
        connected: bool,
        outbound_tokens: u64,
        paths: AgentTaskPaths,
    ) -> (
        Self,
        mpsc::Receiver<AgentCommand>,
        watch::Receiver<Option<GuestMonitorLayout>>,
    ) {
        let AgentTaskPaths {
            commands,
            state,
            offers,
            layout,
            events,
            audio_volume,
            graphics_devices,
            credit_returns,
        } = paths;
        (
            Self {
                connection_generation,
                generation: 0,
                connected,
                ready: false,
                outbound_tokens,
                inbound_tokens: 0,
                decoder: AgentStreamDecoder::new(
                    oxide_spice_protocol::DEFAULT_MAX_AGENT_MESSAGE_BYTES,
                )
                .expect("nonzero Agent message bound"),
                decoded_messages: Vec::new(),
                peer_capabilities: CapabilitySet::new(),
                outbound: None,
                outbound_fragment: Vec::with_capacity(
                    oxide_spice_protocol::MAX_AGENT_FRAGMENT_BYTES,
                ),
                outbound_completion: None,
                announce_pending: false,
                max_clipboard_pending: false,
                sent_layout_generation: 0,
                layout_dirty: false,
                offer_revision: 0,
                outgoing_clipboard_serial: [0; 3],
                incoming_clipboard_serial: [0; 3],
                desired_clipboard_offers: std::array::from_fn(|_| None),
                sent_offer_generation: [0; 3],
                next_request_id: 1,
                pending_reads: HashMap::new(),
                pending_host_requests: HashMap::new(),
                next_transfer_id: 1,
                transfers: HashMap::new(),
                sinks: AgentSinks {
                    state,
                    offers,
                    events,
                    audio_volume,
                    graphics_devices,
                    credit_returns,
                },
            },
            commands,
            layout,
        )
    }

    pub async fn initialize<S>(&mut self, channel: &mut Channel<S>) -> Result<(), ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if self.connected {
            self.begin_generation(channel).await?;
        }
        Ok(())
    }

    /// Replaces Agent token and stream state supplied by a non-seamless target Main Init.
    pub async fn reinitialize_after_migration<S>(
        &mut self,
        connected: bool,
        outbound_tokens: u64,
        disconnect_reason: Option<u32>,
        channel: &mut Channel<S>,
    ) -> Result<(), ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.disconnect(disconnect_reason);
        self.outbound_tokens = outbound_tokens;
        self.connected = connected;
        if connected {
            self.begin_generation(channel).await?;
        }
        Ok(())
    }

    pub const fn can_send(&self) -> bool {
        self.connected && self.outbound_tokens != 0 && self.outbound.is_some()
    }

    pub const fn can_accept_command(&self) -> bool {
        self.ready
            && self.outbound.is_none()
            && !self.announce_pending
            && !self.max_clipboard_pending
    }

    pub const fn connected(&self) -> bool {
        self.connected
    }

    pub fn credit_returns(&self) -> &Arc<AgentCreditReturns> {
        &self.sinks.credit_returns
    }

    pub fn note_layout_changed(&mut self) {
        self.layout_dirty = true;
    }

    pub async fn send_one<S>(&mut self, channel: &mut Channel<S>) -> Result<(), ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let complete = {
            let outbound = self
                .outbound
                .as_mut()
                .ok_or(ClientError::Internal("missing outbound Agent message"))?;
            outbound.next_fragment(&mut self.outbound_fragment)?;
            outbound.is_complete()
        };
        channel
            .write_message(main_client::AGENT_DATA, &self.outbound_fragment)
            .await?;
        self.outbound_tokens = self
            .outbound_tokens
            .checked_sub(1)
            .ok_or(ClientError::Internal("Agent outbound token underflow"))?;
        if complete {
            self.outbound = None;
            if let Some(completion) = self.outbound_completion.take() {
                let _ = completion.send(Ok(()));
            }
        }
        Ok(())
    }

    pub async fn send_returned_credits<S>(
        &mut self,
        channel: &mut Channel<S>,
    ) -> Result<(), ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let returned = self.sinks.credit_returns.take(self.generation);
        if returned == 0 {
            return Ok(());
        }
        self.inbound_tokens = self
            .inbound_tokens
            .checked_add(returned)
            .filter(|tokens| *tokens <= AGENT_RECEIVE_WINDOW)
            .ok_or_else(|| protocol_error("Agent inbound token window"))?;
        channel
            .write_message(main_client::AGENT_TOKEN, &encode_agent_tokens(returned))
            .await
    }

    pub fn prepare_internal(
        &mut self,
        layout: &mut watch::Receiver<Option<GuestMonitorLayout>>,
    ) -> Result<(), ClientError> {
        if !self.connected || self.outbound.is_some() {
            return Ok(());
        }
        if self.announce_pending {
            self.announce_pending = false;
            let capabilities = local_agent_capabilities()?;
            self.queue(
                agent_message::ANNOUNCE_CAPABILITIES,
                AgentCapabilities {
                    request_reply: !self.ready,
                    capabilities,
                }
                .encode(),
            )?;
            return Ok(());
        }
        if self.max_clipboard_pending {
            self.max_clipboard_pending = false;
            let maximum = i32::try_from(MAX_CLIPBOARD_BYTES)
                .expect("clipboard limit fits signed Agent field");
            self.queue(agent_message::MAX_CLIPBOARD, maximum.to_le_bytes().to_vec())?;
            return Ok(());
        }
        if self.ready
            && (self.sent_layout_generation != self.generation
                || self.layout_dirty
                || layout.has_changed().unwrap_or(false))
        {
            let desired = layout.borrow_and_update().clone();
            self.sent_layout_generation = self.generation;
            self.layout_dirty = false;
            if let Some(layout) = desired {
                let features = features_from_capabilities(&self.peer_capabilities);
                if features.monitor_config {
                    if !features.sparse_monitors
                        && layout
                            .monitors
                            .iter()
                            .any(|monitor| monitor.width == 0 || monitor.height == 0)
                    {
                        return Ok(());
                    }
                    let monitors: Vec<_> = layout
                        .monitors
                        .iter()
                        .copied()
                        .map(|monitor| {
                            let mut monitor = AgentMonitorConfig::from(monitor);
                            if !features.monitor_physical_size {
                                monitor.width_mm = None;
                                monitor.height_mm = None;
                            }
                            monitor
                        })
                        .collect();
                    let payload = encode_monitors_config(
                        &monitors,
                        features.monitor_positions,
                        features.sparse_monitors,
                        features.monitor_physical_size,
                    )?;
                    self.queue(agent_message::MONITORS_CONFIG, payload)?;
                }
            }
            return Ok(());
        }
        if self.ready {
            let features = features_from_capabilities(&self.peer_capabilities);
            for selection_index in 0..self.desired_clipboard_offers.len() {
                if let Some(types) = self.desired_clipboard_offers[selection_index].clone()
                    && self.sent_offer_generation[selection_index] != self.generation
                {
                    let selection = match selection_index {
                        0 => AgentClipboardSelection::Clipboard,
                        1 => AgentClipboardSelection::Primary,
                        2 => AgentClipboardSelection::Secondary,
                        _ => unreachable!("fixed clipboard selection array"),
                    };
                    if selection != AgentClipboardSelection::Clipboard
                        && !features.clipboard_selection
                    {
                        self.sent_offer_generation[selection_index] = self.generation;
                        continue;
                    }
                    let serial = if features.clipboard_grab_serial {
                        let serial = self.outgoing_clipboard_serial[selection_index];
                        self.outgoing_clipboard_serial[selection_index] = serial.wrapping_add(1);
                        Some(serial)
                    } else {
                        None
                    };
                    let payload = encode_clipboard_grab(
                        selection,
                        serial,
                        &types,
                        features.clipboard_selection,
                    )?;
                    self.sent_offer_generation[selection_index] = self.generation;
                    self.queue(agent_message::CLIPBOARD_GRAB, payload)?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    pub fn accept_command(&mut self, command: AgentCommand) -> Result<(), ClientError> {
        let command_generation = match &command {
            AgentCommand::SyncAudioVolume {
                agent_generation, ..
            }
            | AgentCommand::OfferClipboard {
                agent_generation, ..
            }
            | AgentCommand::ReleaseClipboard {
                agent_generation, ..
            }
            | AgentCommand::RequestClipboard {
                agent_generation, ..
            }
            | AgentCommand::ProvideClipboard {
                agent_generation, ..
            }
            | AgentCommand::StartFile {
                agent_generation, ..
            }
            | AgentCommand::FileData {
                agent_generation, ..
            }
            | AgentCommand::CancelFile {
                agent_generation, ..
            } => *agent_generation,
        };
        if command_generation != self.generation || !self.ready {
            match command {
                AgentCommand::RequestClipboard { response, .. } => {
                    let _ = response.send(Err(AgentSendError::StaleGeneration));
                }
                AgentCommand::StartFile { response, .. } => {
                    let _ = response.send(Err(AgentSendError::StaleGeneration));
                }
                AgentCommand::FileData { completion, .. }
                | AgentCommand::CancelFile { completion, .. } => {
                    let _ = completion.send(Err(AgentSendError::StaleGeneration));
                }
                _ => {}
            }
            return Ok(());
        }
        let features = features_from_capabilities(&self.peer_capabilities);
        match command {
            AgentCommand::SyncAudioVolume { volume, .. } => {
                if !features.audio_volume_sync {
                    return Ok(());
                }
                let payload = encode_audio_volume_sync(&volume)?;
                self.queue(agent_message::AUDIO_VOLUME_SYNC, payload)?;
            }
            AgentCommand::OfferClipboard {
                selection, types, ..
            } => {
                if !features.clipboard_by_demand {
                    return Ok(());
                }
                self.desired_clipboard_offers[selection as usize] = Some(types.clone());
                let serial = if features.clipboard_grab_serial {
                    let serial = self.outgoing_clipboard_serial[selection as usize];
                    self.outgoing_clipboard_serial[selection as usize] = serial.wrapping_add(1);
                    Some(serial)
                } else {
                    None
                };
                let payload =
                    encode_clipboard_grab(selection, serial, &types, features.clipboard_selection)?;
                self.sent_offer_generation[selection as usize] = self.generation;
                self.queue(agent_message::CLIPBOARD_GRAB, payload)?;
            }
            AgentCommand::ReleaseClipboard { selection, .. } => {
                if !features.clipboard_by_demand {
                    return Ok(());
                }
                self.desired_clipboard_offers[selection as usize] = None;
                let payload = encode_clipboard_release(selection, features.clipboard_selection)?;
                self.sent_offer_generation[selection as usize] = self.generation;
                self.queue(agent_message::CLIPBOARD_RELEASE, payload)?;
            }
            AgentCommand::RequestClipboard {
                selection,
                clipboard_type,
                response,
                ..
            } => {
                if !features.clipboard_by_demand {
                    let _ = response.send(Err(AgentSendError::Unsupported));
                    return Ok(());
                }
                if self.pending_reads.contains_key(&selection) {
                    let _ = response.send(Err(AgentSendError::InvalidData));
                    return Ok(());
                }
                let payload = encode_clipboard_request(
                    selection,
                    clipboard_type,
                    features.clipboard_selection,
                )?;
                self.pending_reads.insert(
                    selection,
                    PendingClipboardRead {
                        generation: self.generation,
                        clipboard_type,
                        response,
                    },
                );
                self.queue(agent_message::CLIPBOARD_REQUEST, payload)?;
            }
            AgentCommand::ProvideClipboard {
                request_id, data, ..
            } => {
                if !features.clipboard_by_demand {
                    return Ok(());
                }
                let Some((generation, selection, clipboard_type)) =
                    self.pending_host_requests.remove(&request_id)
                else {
                    return Ok(());
                };
                if generation != self.generation || data.len() > MAX_CLIPBOARD_BYTES {
                    return Ok(());
                }
                validate_clipboard_payload(clipboard_type, &data)?;
                let payload = encode_clipboard_data(
                    selection,
                    clipboard_type,
                    &data,
                    features.clipboard_selection,
                )?;
                self.queue(agent_message::CLIPBOARD, payload)?;
            }
            AgentCommand::StartFile {
                metadata, response, ..
            } => {
                if features.file_transfer_disabled
                    || self.transfers.len() >= MAX_ACTIVE_FILE_TRANSFERS
                {
                    let _ = response.send(Err(if features.file_transfer_disabled {
                        AgentSendError::Unsupported
                    } else {
                        AgentSendError::ResourceLimit
                    }));
                    return Ok(());
                }
                let transfer_id = self.allocate_transfer_id()?;
                let payload = match encode_file_transfer_start(
                    transfer_id,
                    &metadata.file_name,
                    metadata.size,
                ) {
                    Ok(payload) => payload,
                    Err(_) => {
                        let _ = response.send(Err(AgentSendError::InvalidData));
                        return Ok(());
                    }
                };
                let (state, state_receiver) = watch::channel(FileTransferState::WaitingForGuest);
                self.transfers.insert(
                    transfer_id,
                    TransferRuntime {
                        generation: self.generation,
                        total_bytes: metadata.size,
                        accepted_bytes: 0,
                        phase: TransferPhase::WaitingForGuest,
                        state,
                    },
                );
                self.queue(agent_message::FILE_TRANSFER_START, payload)?;
                let _ = response.send(Ok(FileTransferRegistration {
                    transfer_id,
                    state: state_receiver,
                }));
            }
            AgentCommand::FileData {
                transfer_id,
                data,
                completion,
                ..
            } => {
                let Some(transfer) = self.transfers.get_mut(&transfer_id) else {
                    let _ = completion.send(Err(AgentSendError::InvalidData));
                    return Ok(());
                };
                if transfer.generation != self.generation
                    || transfer.phase != TransferPhase::Sending
                {
                    let _ = completion.send(Err(AgentSendError::InvalidData));
                    return Ok(());
                }
                let data_bytes =
                    u64::try_from(data.len()).map_err(|_| resource_error("Agent file chunk"))?;
                if data.is_empty() && transfer.total_bytes != 0
                    || !data.is_empty() && transfer.total_bytes == 0
                {
                    let _ = completion.send(Err(AgentSendError::InvalidData));
                    return Ok(());
                }
                let accepted_bytes = transfer
                    .accepted_bytes
                    .checked_add(data_bytes)
                    .filter(|total| *total <= transfer.total_bytes);
                let Some(accepted_bytes) = accepted_bytes else {
                    let _ = completion.send(Err(AgentSendError::ResourceLimit));
                    return Ok(());
                };
                let payload = match encode_file_transfer_data(transfer_id, &data) {
                    Ok(payload) => payload,
                    Err(_) => {
                        let _ = completion.send(Err(AgentSendError::ResourceLimit));
                        return Ok(());
                    }
                };
                transfer.accepted_bytes = accepted_bytes;
                if accepted_bytes == transfer.total_bytes {
                    transfer.phase = TransferPhase::AwaitingCompletion;
                    transfer
                        .state
                        .send_replace(FileTransferState::AwaitingCompletion { accepted_bytes });
                } else {
                    transfer
                        .state
                        .send_replace(FileTransferState::Sending { accepted_bytes });
                }
                self.queue_with_completion(agent_message::FILE_TRANSFER_DATA, payload, completion)?;
            }
            AgentCommand::CancelFile {
                transfer_id,
                completion,
                ..
            } => {
                let Some(transfer) = self.transfers.remove(&transfer_id) else {
                    let _ = completion.send(Err(AgentSendError::InvalidData));
                    return Ok(());
                };
                transfer.state.send_replace(FileTransferState::Cancelled);
                let payload =
                    encode_file_transfer_status(transfer_id, AgentFileTransferStatus::Cancelled)?
                        .to_vec();
                self.queue_with_completion(
                    agent_message::FILE_TRANSFER_STATUS,
                    payload,
                    completion,
                )?;
            }
        }
        Ok(())
    }

    pub async fn handle_server_message<S>(
        &mut self,
        message_type: u16,
        body: &[u8],
        channel: &mut Channel<S>,
    ) -> Result<bool, ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        match message_type {
            oxide_spice_protocol::main_server::AGENT_CONNECTED => {
                if !body.is_empty() {
                    return Err(protocol_error("Main Agent Connected body"));
                }
                self.connected = true;
                self.begin_generation(channel).await?;
            }
            oxide_spice_protocol::main_server::AGENT_CONNECTED_TOKENS => {
                self.outbound_tokens = u64::from(oxide_spice_protocol::decode_agent_u32(
                    body,
                    "Main Agent Connected tokens",
                )?);
                self.connected = true;
                self.begin_generation(channel).await?;
            }
            oxide_spice_protocol::main_server::AGENT_DISCONNECTED => {
                let reason =
                    oxide_spice_protocol::decode_agent_u32(body, "Main Agent disconnect reason")?;
                self.disconnect(Some(reason));
            }
            oxide_spice_protocol::main_server::AGENT_TOKEN => {
                let tokens = u64::from(oxide_spice_protocol::decode_agent_u32(
                    body,
                    "Main Agent tokens",
                )?);
                self.outbound_tokens = self
                    .outbound_tokens
                    .checked_add(tokens)
                    .ok_or_else(|| resource_error("Agent outbound tokens"))?;
            }
            oxide_spice_protocol::main_server::AGENT_DATA => {
                self.handle_fragment(body)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn handle_fragment(&mut self, body: &[u8]) -> Result<(), ClientError> {
        if !self.connected || self.inbound_tokens == 0 {
            return Err(protocol_error("Agent data without inbound token"));
        }
        self.inbound_tokens -= 1;
        let mut messages = std::mem::take(&mut self.decoded_messages);
        messages.clear();
        self.decoder.push_fragment_into(body, &mut messages)?;
        let credit = inbound_credit(self.sinks.credit_returns.clone(), self.generation);
        let mut retained_credit = false;
        for message in messages.drain(..) {
            retained_credit |= self.dispatch_agent_message(message, credit.clone())?;
        }
        self.decoded_messages = messages;
        if !retained_credit {
            drop(credit);
        }
        Ok(())
    }

    fn dispatch_agent_message(
        &mut self,
        message: AgentMessage,
        credit: Arc<InboundAgentCredit>,
    ) -> Result<bool, ClientError> {
        if message.protocol != AGENT_PROTOCOL {
            return Ok(false);
        }
        let selection_supported = self
            .peer_capabilities
            .contains(agent_capability::CLIPBOARD_SELECTION);
        match message.message_type {
            agent_message::ANNOUNCE_CAPABILITIES => {
                let capabilities = AgentCapabilities::decode(&message.payload)?;
                self.peer_capabilities = capabilities.capabilities;
                self.ready = true;
                self.max_clipboard_pending = self
                    .peer_capabilities
                    .contains(agent_capability::MAX_CLIPBOARD);
                if capabilities.request_reply {
                    self.announce_pending = true;
                }
                self.sinks.state.send_replace(AgentState::Ready {
                    connection_generation: self.connection_generation,
                    agent_generation: self.generation,
                    features: features_from_capabilities(&self.peer_capabilities),
                });
            }
            agent_message::CLIPBOARD_GRAB => {
                let serial_supported = self
                    .peer_capabilities
                    .contains(agent_capability::CLIPBOARD_GRAB_SERIAL);
                let grab =
                    decode_clipboard_grab(&message.payload, selection_supported, serial_supported)?;
                if let Some(serial) = grab.serial {
                    let expected = &mut self.incoming_clipboard_serial[grab.selection as usize];
                    if serial != *expected {
                        return Ok(false);
                    }
                    *expected = expected.wrapping_add(1);
                }
                self.offer_revision = self.offer_revision.wrapping_add(1);
                let offer = ClipboardOffer {
                    connection_generation: self.connection_generation,
                    agent_generation: self.generation,
                    revision: self.offer_revision,
                    selection: grab.selection,
                    types: grab.types.into(),
                };
                let mut offers = self.sinks.offers.borrow().clone();
                offers.replace(offer);
                self.sinks.offers.send_replace(offers);
            }
            agent_message::CLIPBOARD_RELEASE => {
                let selection = decode_clipboard_release(&message.payload, selection_supported)?;
                let mut offers = self.sinks.offers.borrow().clone();
                offers.clear(selection);
                self.sinks.offers.send_replace(offers);
            }
            agent_message::CLIPBOARD_REQUEST => {
                let request = decode_clipboard_request(&message.payload, selection_supported)?;
                let clipboard_type = AgentClipboardType::try_from(request.clipboard_type)?;
                if clipboard_type == AgentClipboardType::None {
                    return Err(protocol_error("Agent clipboard request type"));
                }
                let request_id = self.next_request_id;
                self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
                self.pending_host_requests.insert(
                    request_id,
                    (self.generation, request.selection, clipboard_type),
                );
                let event = AgentEvent::ClipboardRequested(ClipboardRequest {
                    request_id,
                    connection_generation: self.connection_generation,
                    agent_generation: self.generation,
                    selection: request.selection,
                    clipboard_type,
                    _credit: Some(credit),
                });
                match self.sinks.events.try_send(event) {
                    Ok(()) => return Ok(true),
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        return Err(resource_error("Agent event queue"));
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        self.pending_host_requests.remove(&request_id);
                    }
                }
            }
            agent_message::CLIPBOARD => {
                let data = decode_clipboard_data(&message.payload, selection_supported)?;
                let clipboard_type = AgentClipboardType::try_from(data.clipboard_type)?;
                if data.data.len() > MAX_CLIPBOARD_BYTES {
                    return Err(resource_error("Agent clipboard data"));
                }
                validate_clipboard_payload(clipboard_type, data.data)?;
                if let Some(pending) = self.pending_reads.remove(&data.selection)
                    && pending.generation == self.generation
                    && pending.clipboard_type == clipboard_type
                {
                    let _ = pending.response.send(Ok(Arc::from(data.data)));
                }
            }
            agent_message::FILE_TRANSFER_STATUS => {
                let status = decode_file_transfer_status(&message.payload)?;
                let Some(transfer) = self.transfers.get_mut(&status.transfer_id) else {
                    return Ok(false);
                };
                if transfer.generation != self.generation {
                    return Ok(false);
                }
                let terminal_state = match status.status {
                    AgentFileTransferStatus::CanSendData => {
                        if transfer.phase != TransferPhase::WaitingForGuest {
                            return Err(protocol_error("duplicate Agent file acceptance"));
                        }
                        transfer.phase = TransferPhase::Sending;
                        transfer.state.send_replace(FileTransferState::Sending {
                            accepted_bytes: transfer.accepted_bytes,
                        });
                        None
                    }
                    AgentFileTransferStatus::Success => {
                        if transfer.phase != TransferPhase::AwaitingCompletion
                            || transfer.accepted_bytes != transfer.total_bytes
                        {
                            return Err(protocol_error("early Agent file success"));
                        }
                        Some(FileTransferState::Completed)
                    }
                    AgentFileTransferStatus::Cancelled => Some(FileTransferState::Cancelled),
                    AgentFileTransferStatus::RemoteError
                    | AgentFileTransferStatus::NotEnoughSpace
                    | AgentFileTransferStatus::SessionLocked
                    | AgentFileTransferStatus::AgentNotConnected
                    | AgentFileTransferStatus::Disabled => {
                        let failure = decode_file_transfer_failure(status.status, status.detail)?;
                        Some(FileTransferState::Failed { failure })
                    }
                };
                if let Some(terminal_state) = terminal_state {
                    transfer.state.send_replace(terminal_state);
                    self.transfers.remove(&status.transfer_id);
                }
            }
            agent_message::AUDIO_VOLUME_SYNC => {
                let volume = decode_audio_volume_sync(&message.payload)?;
                self.sinks
                    .audio_volume
                    .send_replace(Some(AgentAudioVolumeState {
                        connection_generation: self.connection_generation,
                        agent_generation: self.generation,
                        is_playback: volume.is_playback,
                        muted: volume.muted,
                        volumes: volume.volumes.into(),
                    }));
            }
            agent_message::GRAPHICS_DEVICE_INFO => {
                let info = decode_graphics_device_info(&message.payload)?;
                self.sinks
                    .graphics_devices
                    .send_replace(Some(AgentGraphicsDeviceState {
                        connection_generation: self.connection_generation,
                        agent_generation: self.generation,
                        displays: info.displays.into(),
                    }));
            }
            _ => {}
        }
        Ok(false)
    }

    async fn begin_generation<S>(&mut self, channel: &mut Channel<S>) -> Result<(), ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.fail_pending_reads(AgentSendError::StaleGeneration);
        self.fail_transfers();
        if let Some(completion) = self.outbound_completion.take() {
            let _ = completion.send(Err(AgentSendError::StaleGeneration));
        }
        self.generation = self.generation.wrapping_add(1).max(1);
        self.ready = false;
        self.decoder.reset();
        self.peer_capabilities = CapabilitySet::new();
        self.outbound = None;
        self.announce_pending = true;
        self.max_clipboard_pending = false;
        self.inbound_tokens = AGENT_RECEIVE_WINDOW;
        self.outgoing_clipboard_serial = [0; 3];
        self.incoming_clipboard_serial = [0; 3];
        self.sent_offer_generation = [0; 3];
        self.pending_host_requests.clear();
        self.sinks.credit_returns.reset(self.generation);
        self.sinks.offers.send_replace(ClipboardOffers::default());
        self.sinks.audio_volume.send_replace(None);
        self.sinks.graphics_devices.send_replace(None);
        channel
            .write_message(
                main_client::AGENT_START,
                &encode_agent_tokens(AGENT_RECEIVE_WINDOW),
            )
            .await?;
        self.sinks.state.send_replace(AgentState::Negotiating {
            connection_generation: self.connection_generation,
            agent_generation: self.generation,
        });
        Ok(())
    }

    fn disconnect(&mut self, reason: Option<u32>) {
        self.connected = false;
        self.ready = false;
        self.inbound_tokens = 0;
        self.decoder.reset();
        self.outbound = None;
        if let Some(completion) = self.outbound_completion.take() {
            let _ = completion.send(Err(AgentSendError::StaleGeneration));
        }
        self.announce_pending = false;
        self.max_clipboard_pending = false;
        self.peer_capabilities = CapabilitySet::new();
        self.fail_pending_reads(AgentSendError::StaleGeneration);
        self.fail_transfers();
        self.pending_host_requests.clear();
        self.sinks.offers.send_replace(ClipboardOffers::default());
        self.sinks.audio_volume.send_replace(None);
        self.sinks.graphics_devices.send_replace(None);
        self.sinks.state.send_replace(AgentState::Disconnected {
            connection_generation: self.connection_generation,
            agent_generation: self.generation,
            reason,
        });
    }

    fn fail_pending_reads(&mut self, error: AgentSendError) {
        for (_, pending) in self.pending_reads.drain() {
            let _ = pending.response.send(Err(error));
        }
    }

    fn fail_transfers(&mut self) {
        for (_, transfer) in self.transfers.drain() {
            transfer
                .state
                .send_replace(FileTransferState::AgentDisconnected);
        }
    }

    fn queue(&mut self, message_type: u32, payload: Vec<u8>) -> Result<(), ClientError> {
        if self.outbound.is_some() {
            return Err(ClientError::Internal("overlapping outbound Agent messages"));
        }
        self.outbound = Some(OutboundAgentMessage::new(
            message_type,
            0,
            Arc::from(payload),
            oxide_spice_protocol::DEFAULT_MAX_AGENT_MESSAGE_BYTES,
        )?);
        Ok(())
    }

    fn queue_with_completion(
        &mut self,
        message_type: u32,
        payload: Vec<u8>,
        completion: oneshot::Sender<Result<(), AgentSendError>>,
    ) -> Result<(), ClientError> {
        self.queue(message_type, payload)?;
        self.outbound_completion = Some(completion);
        Ok(())
    }

    fn allocate_transfer_id(&mut self) -> Result<u32, ClientError> {
        for _ in 0..=MAX_ACTIVE_FILE_TRANSFERS {
            let candidate = self.next_transfer_id.max(1);
            self.next_transfer_id = candidate.wrapping_add(1).max(1);
            if !self.transfers.contains_key(&candidate) {
                return Ok(candidate);
            }
        }
        Err(resource_error("Agent file transfer identifiers"))
    }
}

fn local_agent_capabilities() -> Result<CapabilitySet, ClientError> {
    CapabilitySet::from_bits([
        agent_capability::MONITORS_CONFIG,
        agent_capability::REPLY,
        agent_capability::CLIPBOARD_BY_DEMAND,
        agent_capability::CLIPBOARD_SELECTION,
        agent_capability::MAX_CLIPBOARD,
        agent_capability::MONITORS_CONFIG_POSITION,
        agent_capability::SPARSE_MONITORS_CONFIG,
        agent_capability::AUDIO_VOLUME_SYNC,
        agent_capability::FILE_TRANSFER_DETAILED_ERRORS,
        agent_capability::GRAPHICS_DEVICE_INFO,
        agent_capability::CLIPBOARD_NO_RELEASE_ON_REGRAB,
        agent_capability::CLIPBOARD_GRAB_SERIAL,
        agent_capability::MONITORS_PHYSICAL_SIZE,
    ])
    .map_err(Into::into)
}

fn validate_clipboard_payload(
    clipboard_type: AgentClipboardType,
    data: &[u8],
) -> Result<(), ClientError> {
    match clipboard_type {
        AgentClipboardType::None => Err(protocol_error("Agent clipboard data type")),
        AgentClipboardType::Utf8Text => std::str::from_utf8(data)
            .map(|_| ())
            .map_err(|_| protocol_error("Agent clipboard UTF-8")),
        AgentClipboardType::FileList => oxide_spice_protocol::decode_clipboard_file_list(data)
            .map(|_| ())
            .map_err(Into::into),
        AgentClipboardType::ImagePng
        | AgentClipboardType::ImageBmp
        | AgentClipboardType::ImageTiff
        | AgentClipboardType::ImageJpeg => {
            if data.is_empty() {
                Err(protocol_error("Agent clipboard image"))
            } else {
                Ok(())
            }
        }
    }
}

fn protocol_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::InvalidValue,
        0,
        context,
    )
    .into()
}

fn resource_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::ResourceLimit,
        0,
        context,
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_fragment_credit_cannot_cross_agent_generations() {
        let returns = Arc::new(AgentCreditReturns::new());
        returns.reset(1);
        let stale_credit = inbound_credit(returns.clone(), 1);

        returns.reset(2);
        drop(stale_credit);
        assert_eq!(returns.take(2), 0);

        returns.return_now(2);
        assert_eq!(returns.take(2), 1);
        assert_eq!(returns.take(2), 0);
    }
}
