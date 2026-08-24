//! Checked SPICE Agent stream framing and payload types.

use std::sync::Arc;

use crate::wire::{Reader, checked_array_bytes};
use crate::{CapabilitySet, DecodeError, DecodeErrorKind, MAX_CAPABILITY_WORDS};

pub const AGENT_PROTOCOL: u32 = 1;
pub const AGENT_MESSAGE_HEADER_BYTES: usize = 20;
pub const MAX_AGENT_FRAGMENT_BYTES: usize = 2048;
pub const DEFAULT_MAX_AGENT_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_AGENT_MONITORS: usize = 256;
pub const MAX_AGENT_CLIPBOARD_TYPES: usize = 64;
pub const MAX_AGENT_CLIPBOARD_FILE_PATHS: usize = 1024;
pub const MAX_AGENT_FILE_NAME_BYTES: usize = 255;
pub const MAX_AGENT_FILE_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_AGENT_AUDIO_CHANNELS: usize = 32;
pub const MAX_AGENT_DISPLAY_DEVICES: usize = 256;
pub const MAX_AGENT_DEVICE_ADDRESS_BYTES: usize = 1024;

/// Agent message identifiers defined by `vd_agent.h`.
pub mod agent_message {
    pub const MOUSE_STATE: u32 = 1;
    pub const MONITORS_CONFIG: u32 = 2;
    pub const REPLY: u32 = 3;
    pub const CLIPBOARD: u32 = 4;
    pub const DISPLAY_CONFIG: u32 = 5;
    pub const ANNOUNCE_CAPABILITIES: u32 = 6;
    pub const CLIPBOARD_GRAB: u32 = 7;
    pub const CLIPBOARD_REQUEST: u32 = 8;
    pub const CLIPBOARD_RELEASE: u32 = 9;
    pub const FILE_TRANSFER_START: u32 = 10;
    pub const FILE_TRANSFER_STATUS: u32 = 11;
    pub const FILE_TRANSFER_DATA: u32 = 12;
    pub const CLIENT_DISCONNECTED: u32 = 13;
    pub const MAX_CLIPBOARD: u32 = 14;
    pub const AUDIO_VOLUME_SYNC: u32 = 15;
    pub const GRAPHICS_DEVICE_INFO: u32 = 16;
}

/// Agent capability bit indices.
pub mod agent_capability {
    pub const MOUSE_STATE: u32 = 0;
    pub const MONITORS_CONFIG: u32 = 1;
    pub const REPLY: u32 = 2;
    pub const CLIPBOARD: u32 = 3;
    pub const DISPLAY_CONFIG: u32 = 4;
    pub const CLIPBOARD_BY_DEMAND: u32 = 5;
    pub const CLIPBOARD_SELECTION: u32 = 6;
    pub const SPARSE_MONITORS_CONFIG: u32 = 7;
    pub const GUEST_LINE_END_LF: u32 = 8;
    pub const GUEST_LINE_END_CRLF: u32 = 9;
    pub const MAX_CLIPBOARD: u32 = 10;
    pub const AUDIO_VOLUME_SYNC: u32 = 11;
    pub const MONITORS_CONFIG_POSITION: u32 = 12;
    pub const FILE_TRANSFER_DISABLED: u32 = 13;
    pub const FILE_TRANSFER_DETAILED_ERRORS: u32 = 14;
    pub const GRAPHICS_DEVICE_INFO: u32 = 15;
    pub const CLIPBOARD_NO_RELEASE_ON_REGRAB: u32 = 16;
    pub const CLIPBOARD_GRAB_SERIAL: u32 = 17;
    pub const MONITORS_PHYSICAL_SIZE: u32 = 18;
}

/// Clipboard payload type identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum AgentClipboardType {
    None = 0,
    Utf8Text = 1,
    ImagePng = 2,
    ImageBmp = 3,
    ImageTiff = 4,
    ImageJpeg = 5,
    FileList = 6,
}

/// File-list operation encoded as the first NUL-terminated clipboard item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentClipboardFileAction {
    Copy,
    Cut,
}

/// WebDAV-relative absolute paths carried by the Agent file-list clipboard type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentClipboardFileList {
    pub action: AgentClipboardFileAction,
    pub paths: Vec<String>,
}

impl TryFrom<u32> for AgentClipboardType {
    type Error = DecodeError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Utf8Text),
            2 => Ok(Self::ImagePng),
            3 => Ok(Self::ImageBmp),
            4 => Ok(Self::ImageTiff),
            5 => Ok(Self::ImageJpeg),
            6 => Ok(Self::FileList),
            _ => Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                0,
                "agent clipboard type",
            )),
        }
    }
}

/// Clipboard selection identifiers negotiated by the selection capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AgentClipboardSelection {
    Clipboard = 0,
    Primary = 1,
    Secondary = 2,
}

/// File transfer status values returned by the guest Agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AgentFileTransferStatus {
    CanSendData = 0,
    Cancelled = 1,
    RemoteError = 2,
    Success = 3,
    NotEnoughSpace = 4,
    SessionLocked = 5,
    AgentNotConnected = 6,
    Disabled = 7,
}

impl TryFrom<u32> for AgentFileTransferStatus {
    type Error = DecodeError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::CanSendData),
            1 => Ok(Self::Cancelled),
            2 => Ok(Self::RemoteError),
            3 => Ok(Self::Success),
            4 => Ok(Self::NotEnoughSpace),
            5 => Ok(Self::SessionLocked),
            6 => Ok(Self::AgentNotConnected),
            7 => Ok(Self::Disabled),
            _ => Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                4,
                "agent file transfer status",
            )),
        }
    }
}

/// Borrowed status with optional capability-dependent detail bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentFileTransferStatusMessage<'a> {
    pub transfer_id: u32,
    pub status: AgentFileTransferStatus,
    pub detail: &'a [u8],
}

/// Capability-dependent failure information attached to a file-transfer status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFileTransferFailure {
    RemoteError {
        error_domain: Option<u8>,
        error_code: Option<u32>,
    },
    NotEnoughSpace {
        available_bytes: Option<u64>,
    },
    SessionLocked,
    AgentNotConnected,
    Disabled,
}

/// One bounded playback or recording volume update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAudioVolumeSync {
    pub is_playback: bool,
    pub muted: bool,
    pub volumes: Vec<u16>,
}

/// One display device entry reported by the guest graphics Agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDisplayDevice {
    pub channel_id: u32,
    pub monitor_id: u32,
    pub device_display_id: u32,
    pub device_address: String,
}

/// Bounded graphics device information reported by the guest Agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentGraphicsDeviceInfo {
    pub displays: Vec<AgentDisplayDevice>,
}

impl TryFrom<u8> for AgentClipboardSelection {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Clipboard),
            1 => Ok(Self::Primary),
            2 => Ok(Self::Secondary),
            _ => Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "agent clipboard selection",
            )),
        }
    }
}

/// One complete message reconstructed from the Main AgentData byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessage {
    pub protocol: u32,
    pub message_type: u32,
    pub opaque: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
struct PartialAgentMessage {
    protocol: u32,
    message_type: u32,
    opaque: u64,
    declared_size: usize,
    payload: Vec<u8>,
}

/// Incremental, allocation-bounded decoder for fragmented AgentData bodies.
#[derive(Debug)]
pub struct AgentStreamDecoder {
    maximum_message_bytes: usize,
    header: [u8; AGENT_MESSAGE_HEADER_BYTES],
    header_bytes: usize,
    current: Option<PartialAgentMessage>,
}

impl AgentStreamDecoder {
    pub fn new(maximum_message_bytes: usize) -> Result<Self, DecodeError> {
        if maximum_message_bytes == 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                0,
                "agent message bound",
            ));
        }
        Ok(Self {
            maximum_message_bytes,
            header: [0; AGENT_MESSAGE_HEADER_BYTES],
            header_bytes: 0,
            current: None,
        })
    }

    /// Consumes one token-sized AgentData body and appends complete stream messages to `output`.
    pub fn push_fragment_into(
        &mut self,
        mut fragment: &[u8],
        output: &mut Vec<AgentMessage>,
    ) -> Result<(), DecodeError> {
        if fragment.is_empty() || fragment.len() > MAX_AGENT_FRAGMENT_BYTES {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                fragment.len(),
                "agent fragment bytes",
            ));
        }
        while !fragment.is_empty() {
            if self.current.is_none() {
                let needed = AGENT_MESSAGE_HEADER_BYTES - self.header_bytes;
                let copied = needed.min(fragment.len());
                self.header[self.header_bytes..self.header_bytes + copied]
                    .copy_from_slice(&fragment[..copied]);
                self.header_bytes += copied;
                fragment = &fragment[copied..];
                if self.header_bytes != AGENT_MESSAGE_HEADER_BYTES {
                    continue;
                }
                let mut header = Reader::new(&self.header);
                let protocol = header.u32("agent protocol")?;
                let message_type = header.u32("agent message type")?;
                let opaque = header.u64("agent opaque")?;
                let declared_size =
                    usize::try_from(header.u32("agent payload size")?).map_err(|_| {
                        DecodeError::new(DecodeErrorKind::Overflow, 16, "agent payload size")
                    })?;
                if declared_size > self.maximum_message_bytes {
                    return Err(DecodeError::new(
                        DecodeErrorKind::ResourceLimit,
                        16,
                        "agent payload size",
                    ));
                }
                self.current = Some(PartialAgentMessage {
                    protocol,
                    message_type,
                    opaque,
                    declared_size,
                    payload: Vec::with_capacity(declared_size),
                });
                self.header_bytes = 0;
            }

            let current = self.current.as_mut().expect("agent header initialized");
            let remaining = current.declared_size - current.payload.len();
            let copied = remaining.min(fragment.len());
            current.payload.extend_from_slice(&fragment[..copied]);
            fragment = &fragment[copied..];
            if current.payload.len() == current.declared_size {
                let current = self.current.take().expect("complete agent message");
                output.push(AgentMessage {
                    protocol: current.protocol,
                    message_type: current.message_type,
                    opaque: current.opaque,
                    payload: current.payload,
                });
            }
        }
        Ok(())
    }

    /// Discards an incomplete message when the Agent connection generation changes.
    pub fn reset(&mut self) {
        self.header_bytes = 0;
        self.current = None;
    }
}

/// Token-by-token encoder state for one outbound Agent stream message.
#[derive(Debug)]
pub struct OutboundAgentMessage {
    header: [u8; AGENT_MESSAGE_HEADER_BYTES],
    payload: Arc<[u8]>,
    stream_offset: usize,
}

impl OutboundAgentMessage {
    pub fn new(
        message_type: u32,
        opaque: u64,
        payload: Arc<[u8]>,
        maximum_message_bytes: usize,
    ) -> Result<Self, DecodeError> {
        if payload.len() > maximum_message_bytes {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                payload.len(),
                "outbound agent payload",
            ));
        }
        let payload_size = u32::try_from(payload.len()).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                payload.len(),
                "outbound agent payload",
            )
        })?;
        let mut header = [0; AGENT_MESSAGE_HEADER_BYTES];
        header[..4].copy_from_slice(&AGENT_PROTOCOL.to_le_bytes());
        header[4..8].copy_from_slice(&message_type.to_le_bytes());
        header[8..16].copy_from_slice(&opaque.to_le_bytes());
        header[16..20].copy_from_slice(&payload_size.to_le_bytes());
        Ok(Self {
            header,
            payload,
            stream_offset: 0,
        })
    }

    pub fn is_complete(&self) -> bool {
        self.stream_offset == AGENT_MESSAGE_HEADER_BYTES + self.payload.len()
    }

    /// Writes the next AgentData body without assembling a second complete message buffer.
    pub fn next_fragment(&mut self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        if self.is_complete() {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                self.stream_offset,
                "completed outbound agent message",
            ));
        }
        output.clear();
        output.reserve(MAX_AGENT_FRAGMENT_BYTES);
        while output.len() < MAX_AGENT_FRAGMENT_BYTES && !self.is_complete() {
            if self.stream_offset < AGENT_MESSAGE_HEADER_BYTES {
                let remaining_header = AGENT_MESSAGE_HEADER_BYTES - self.stream_offset;
                let copied = remaining_header.min(MAX_AGENT_FRAGMENT_BYTES - output.len());
                output.extend_from_slice(
                    &self.header[self.stream_offset..self.stream_offset + copied],
                );
                self.stream_offset += copied;
            } else {
                let payload_offset = self.stream_offset - AGENT_MESSAGE_HEADER_BYTES;
                let copied = (self.payload.len() - payload_offset)
                    .min(MAX_AGENT_FRAGMENT_BYTES - output.len());
                output.extend_from_slice(&self.payload[payload_offset..payload_offset + copied]);
                self.stream_offset += copied;
            }
        }
        Ok(())
    }
}

/// Capabilities announced inside one Agent protocol message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub request_reply: bool,
    pub capabilities: CapabilitySet,
}

impl AgentCapabilities {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        if payload.len() < 4 || !payload.len().is_multiple_of(4) {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                payload.len(),
                "agent capability payload",
            ));
        }
        let word_count = payload.len() / 4 - 1;
        if word_count > MAX_CAPABILITY_WORDS {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                4,
                "agent capability words",
            ));
        }
        let mut reader = Reader::new(payload);
        let request = reader.u32("agent capability request")?;
        if request > 1 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "agent capability request",
            ));
        }
        let mut words = Vec::with_capacity(word_count);
        for _ in 0..word_count {
            words.push(reader.u32("agent capability word")?);
        }
        Ok(Self {
            request_reply: request != 0,
            capabilities: CapabilitySet::from_words(words),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(4 + self.capabilities.words().len() * 4);
        output.extend_from_slice(&u32::from(self.request_reply).to_le_bytes());
        for word in self.capabilities.words() {
            output.extend_from_slice(&word.to_le_bytes());
        }
        output
    }
}

/// A parsed Clipboard Grab with negotiated optional fields removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentClipboardGrab {
    pub selection: AgentClipboardSelection,
    pub serial: Option<u32>,
    pub types: Vec<u32>,
}

/// A parsed Clipboard Request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentClipboardRequest {
    pub selection: AgentClipboardSelection,
    pub clipboard_type: u32,
}

/// Borrowed Clipboard data after negotiated optional fields are decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentClipboardData<'a> {
    pub selection: AgentClipboardSelection,
    pub clipboard_type: u32,
    pub data: &'a [u8],
}

fn decode_selection(
    payload: &[u8],
    selection_supported: bool,
) -> Result<(AgentClipboardSelection, &[u8]), DecodeError> {
    if !selection_supported {
        return Ok((AgentClipboardSelection::Clipboard, payload));
    }
    let prefix = payload.get(..4).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Truncated,
            payload.len(),
            "agent clipboard selection",
        )
    })?;
    Ok((AgentClipboardSelection::try_from(prefix[0])?, &payload[4..]))
}

fn encode_selection(selection: AgentClipboardSelection, selection_supported: bool) -> Vec<u8> {
    if selection_supported {
        vec![selection as u8, 0, 0, 0]
    } else {
        Vec::new()
    }
}

pub fn decode_clipboard_grab(
    payload: &[u8],
    selection_supported: bool,
    serial_supported: bool,
) -> Result<AgentClipboardGrab, DecodeError> {
    let (selection, payload) = decode_selection(payload, selection_supported)?;
    let mut reader = Reader::new(payload);
    let serial = if serial_supported {
        Some(reader.u32("agent clipboard serial")?)
    } else {
        None
    };
    if !reader.remaining().is_multiple_of(4) {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            reader.offset(),
            "agent clipboard types",
        ));
    }
    let type_count = reader.remaining() / 4;
    if type_count > MAX_AGENT_CLIPBOARD_TYPES {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            reader.offset(),
            "agent clipboard types",
        ));
    }
    let mut types = Vec::with_capacity(type_count);
    while reader.remaining() != 0 {
        types.push(reader.u32("agent clipboard type")?);
    }
    if types.is_empty() {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            reader.offset(),
            "agent clipboard types",
        ));
    }
    Ok(AgentClipboardGrab {
        selection,
        serial,
        types,
    })
}

pub fn encode_clipboard_grab(
    selection: AgentClipboardSelection,
    serial: Option<u32>,
    types: &[AgentClipboardType],
    selection_supported: bool,
) -> Result<Vec<u8>, DecodeError> {
    if !selection_supported && selection != AgentClipboardSelection::Clipboard {
        return Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            0,
            "agent clipboard selection",
        ));
    }
    if types.is_empty() {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "agent clipboard types",
        ));
    }
    if types.len() > MAX_AGENT_CLIPBOARD_TYPES {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            0,
            "agent clipboard types",
        ));
    }
    let mut output = encode_selection(selection, selection_supported);
    if let Some(serial) = serial {
        output.extend_from_slice(&serial.to_le_bytes());
    }
    for clipboard_type in types {
        output.extend_from_slice(&(*clipboard_type as u32).to_le_bytes());
    }
    Ok(output)
}

pub fn decode_clipboard_request(
    payload: &[u8],
    selection_supported: bool,
) -> Result<AgentClipboardRequest, DecodeError> {
    let (selection, payload) = decode_selection(payload, selection_supported)?;
    if payload.len() != 4 {
        return Err(DecodeError::new(
            if payload.len() < 4 {
                DecodeErrorKind::Truncated
            } else {
                DecodeErrorKind::InvalidValue
            },
            payload.len(),
            "agent clipboard request",
        ));
    }
    let mut reader = Reader::new(payload);
    Ok(AgentClipboardRequest {
        selection,
        clipboard_type: reader.u32("agent clipboard requested type")?,
    })
}

pub fn encode_clipboard_request(
    selection: AgentClipboardSelection,
    clipboard_type: AgentClipboardType,
    selection_supported: bool,
) -> Result<Vec<u8>, DecodeError> {
    if !selection_supported && selection != AgentClipboardSelection::Clipboard {
        return Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            0,
            "agent clipboard selection",
        ));
    }
    let mut output = encode_selection(selection, selection_supported);
    output.extend_from_slice(&(clipboard_type as u32).to_le_bytes());
    Ok(output)
}

pub fn decode_clipboard_data<'a>(
    payload: &'a [u8],
    selection_supported: bool,
) -> Result<AgentClipboardData<'a>, DecodeError> {
    let (selection, payload) = decode_selection(payload, selection_supported)?;
    let mut reader = Reader::new(payload);
    let clipboard_type = reader.u32("agent clipboard data type")?;
    Ok(AgentClipboardData {
        selection,
        clipboard_type,
        data: reader.take(reader.remaining(), "agent clipboard data")?,
    })
}

pub fn encode_clipboard_data(
    selection: AgentClipboardSelection,
    clipboard_type: AgentClipboardType,
    data: &[u8],
    selection_supported: bool,
) -> Result<Vec<u8>, DecodeError> {
    if !selection_supported && selection != AgentClipboardSelection::Clipboard {
        return Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            0,
            "agent clipboard selection",
        ));
    }
    let mut output = encode_selection(selection, selection_supported);
    output.extend_from_slice(&(clipboard_type as u32).to_le_bytes());
    output.extend_from_slice(data);
    Ok(output)
}

/// Decodes the Agent file-list payload as one action followed by NUL-terminated paths.
pub fn decode_clipboard_file_list(payload: &[u8]) -> Result<AgentClipboardFileList, DecodeError> {
    if payload.last() != Some(&0) {
        return Err(DecodeError::new(
            DecodeErrorKind::Truncated,
            payload.len(),
            "agent clipboard file list terminator",
        ));
    }
    let mut fields = payload[..payload.len() - 1].split(|byte| *byte == 0);
    let action = match fields.next() {
        Some(b"copy") => AgentClipboardFileAction::Copy,
        Some(b"cut") => AgentClipboardFileAction::Cut,
        _ => {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "agent clipboard file action",
            ));
        }
    };
    let mut paths = Vec::new();
    for path in fields {
        if paths.len() == MAX_AGENT_CLIPBOARD_FILE_PATHS {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                payload.len(),
                "agent clipboard file paths",
            ));
        }
        let path = std::str::from_utf8(path).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "agent clipboard file path UTF-8",
            )
        })?;
        if path.is_empty() || !path.starts_with('/') {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "agent clipboard absolute file path",
            ));
        }
        paths.push(path.to_owned());
    }
    if paths.is_empty() {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            payload.len(),
            "agent clipboard file paths",
        ));
    }
    Ok(AgentClipboardFileList { action, paths })
}

/// Encodes WebDAV paths without exposing host filesystem paths or platform separators.
pub fn encode_clipboard_file_list(
    file_list: &AgentClipboardFileList,
) -> Result<Vec<u8>, DecodeError> {
    if file_list.paths.is_empty() {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "agent clipboard file paths",
        ));
    }
    if file_list.paths.len() > MAX_AGENT_CLIPBOARD_FILE_PATHS {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            0,
            "agent clipboard file paths",
        ));
    }
    let action = match file_list.action {
        AgentClipboardFileAction::Copy => b"copy".as_slice(),
        AgentClipboardFileAction::Cut => b"cut".as_slice(),
    };
    let payload_bytes = file_list
        .paths
        .iter()
        .try_fold(action.len() + 1, |total, path| {
            if path.is_empty() || !path.starts_with('/') || path.as_bytes().contains(&0) {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidValue,
                    0,
                    "agent clipboard absolute file path",
                ));
            }
            total.checked_add(path.len() + 1).ok_or_else(|| {
                DecodeError::new(DecodeErrorKind::Overflow, 0, "agent clipboard file list")
            })
        })?;
    let mut output = Vec::with_capacity(payload_bytes);
    output.extend_from_slice(action);
    output.push(0);
    for path in &file_list.paths {
        output.extend_from_slice(path.as_bytes());
        output.push(0);
    }
    Ok(output)
}

pub fn decode_clipboard_release(
    payload: &[u8],
    selection_supported: bool,
) -> Result<AgentClipboardSelection, DecodeError> {
    let (selection, remaining) = decode_selection(payload, selection_supported)?;
    if !remaining.is_empty() {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            payload.len() - remaining.len(),
            "agent clipboard release",
        ));
    }
    Ok(selection)
}

pub fn encode_clipboard_release(
    selection: AgentClipboardSelection,
    selection_supported: bool,
) -> Result<Vec<u8>, DecodeError> {
    if !selection_supported && selection != AgentClipboardSelection::Clipboard {
        return Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            0,
            "agent clipboard selection",
        ));
    }
    Ok(encode_selection(selection, selection_supported))
}

/// One monitor requested through the guest Agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentMonitorConfig {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub x: i32,
    pub y: i32,
    pub width_mm: Option<u16>,
    pub height_mm: Option<u16>,
}

/// Encodes a bounded monitor array in the Agent's height-width-depth-x-y order.
pub fn encode_monitors_config(
    monitors: &[AgentMonitorConfig],
    use_positions: bool,
    sparse_supported: bool,
    physical_size_supported: bool,
) -> Result<Vec<u8>, DecodeError> {
    let monitor_count = u32::try_from(monitors.len()).map_err(|_| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            monitors.len(),
            "agent monitor count",
        )
    })?;
    let monitor_bytes =
        checked_array_bytes(monitor_count, 20, MAX_AGENT_MONITORS, 0, "agent monitors")?;
    for monitor in monitors {
        if monitor.depth == 0 || monitor.depth > 32 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "agent monitor depth",
            ));
        }
        if (monitor.width == 0 || monitor.height == 0) && !sparse_supported {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                0,
                "sparse agent monitor",
            ));
        }
        if !physical_size_supported && (monitor.width_mm.is_some() || monitor.height_mm.is_some()) {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                0,
                "agent monitor physical size",
            ));
        }
    }
    let physical_bytes = if physical_size_supported {
        monitors
            .len()
            .checked_mul(4)
            .ok_or_else(|| DecodeError::new(DecodeErrorKind::Overflow, 0, "agent monitor sizes"))?
    } else {
        0
    };
    let mut output = Vec::with_capacity(8 + monitor_bytes + physical_bytes);
    output.extend_from_slice(&monitor_count.to_le_bytes());
    let mut flags = 0_u32;
    if use_positions {
        flags |= 1 << 0;
    }
    if physical_size_supported {
        flags |= 1 << 1;
    }
    output.extend_from_slice(&flags.to_le_bytes());
    for monitor in monitors {
        output.extend_from_slice(&monitor.height.to_le_bytes());
        output.extend_from_slice(&monitor.width.to_le_bytes());
        output.extend_from_slice(&monitor.depth.to_le_bytes());
        output.extend_from_slice(&monitor.x.to_le_bytes());
        output.extend_from_slice(&monitor.y.to_le_bytes());
    }
    if physical_size_supported {
        for monitor in monitors {
            output.extend_from_slice(&monitor.height_mm.unwrap_or(0).to_le_bytes());
            output.extend_from_slice(&monitor.width_mm.unwrap_or(0).to_le_bytes());
        }
    }
    Ok(output)
}

/// Encodes the compatible key-file subset used to start one outgoing file transfer.
pub fn encode_file_transfer_start(
    transfer_id: u32,
    file_name: &str,
    file_size: u64,
) -> Result<Vec<u8>, DecodeError> {
    if transfer_id == 0
        || file_name.is_empty()
        || file_name.len() > MAX_AGENT_FILE_NAME_BYTES
        || file_name
            .bytes()
            .any(|byte| matches!(byte, 0 | b'/' | b'\\' | b'\r' | b'\n' | b'[' | b']' | b'='))
        || file_name == "."
        || file_name == ".."
    {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "agent file name",
        ));
    }
    let metadata = format!("[vdagent-file-xfer]\nname={file_name}\nsize={file_size}\n");
    let mut output = Vec::with_capacity(4 + metadata.len() + 1);
    output.extend_from_slice(&transfer_id.to_le_bytes());
    output.extend_from_slice(metadata.as_bytes());
    output.push(0);
    Ok(output)
}

/// Encodes one exact bounded data chunk; size is the current chunk length.
pub fn encode_file_transfer_data(transfer_id: u32, data: &[u8]) -> Result<Vec<u8>, DecodeError> {
    if transfer_id == 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "agent file transfer id",
        ));
    }
    if data.len() > MAX_AGENT_FILE_CHUNK_BYTES {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            data.len(),
            "agent file transfer data",
        ));
    }
    let data_size = u64::try_from(data.len()).map_err(|_| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            data.len(),
            "agent file transfer data",
        )
    })?;
    let mut output = Vec::with_capacity(12 + data.len());
    output.extend_from_slice(&transfer_id.to_le_bytes());
    output.extend_from_slice(&data_size.to_le_bytes());
    output.extend_from_slice(data);
    Ok(output)
}

pub fn decode_file_transfer_status(
    payload: &[u8],
) -> Result<AgentFileTransferStatusMessage<'_>, DecodeError> {
    let mut reader = Reader::new(payload);
    let transfer_id = reader.u32("agent file transfer id")?;
    if transfer_id == 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "agent file transfer id",
        ));
    }
    let status = AgentFileTransferStatus::try_from(reader.u32("agent file transfer status")?)?;
    let detail = reader.take(reader.remaining(), "agent file transfer detail")?;
    Ok(AgentFileTransferStatusMessage {
        transfer_id,
        status,
        detail,
    })
}

/// Decodes the optional detail union negotiated by the detailed-errors capability.
pub fn decode_file_transfer_failure(
    status: AgentFileTransferStatus,
    detail: &[u8],
) -> Result<AgentFileTransferFailure, DecodeError> {
    match status {
        AgentFileTransferStatus::RemoteError => {
            if detail.is_empty() {
                return Ok(AgentFileTransferFailure::RemoteError {
                    error_domain: None,
                    error_code: None,
                });
            }
            if detail.len() != 5 {
                return Err(DecodeError::new(
                    if detail.len() < 5 {
                        DecodeErrorKind::Truncated
                    } else {
                        DecodeErrorKind::InvalidValue
                    },
                    detail.len(),
                    "agent file transfer remote error",
                ));
            }
            let mut reader = Reader::new(detail);
            Ok(AgentFileTransferFailure::RemoteError {
                error_domain: Some(reader.u8("agent file transfer error domain")?),
                error_code: Some(reader.u32("agent file transfer error code")?),
            })
        }
        AgentFileTransferStatus::NotEnoughSpace => {
            if detail.is_empty() {
                return Ok(AgentFileTransferFailure::NotEnoughSpace {
                    available_bytes: None,
                });
            }
            if detail.len() != 8 {
                return Err(DecodeError::new(
                    if detail.len() < 8 {
                        DecodeErrorKind::Truncated
                    } else {
                        DecodeErrorKind::InvalidValue
                    },
                    detail.len(),
                    "agent file transfer free space",
                ));
            }
            let mut reader = Reader::new(detail);
            Ok(AgentFileTransferFailure::NotEnoughSpace {
                available_bytes: Some(reader.u64("agent file transfer free space")?),
            })
        }
        AgentFileTransferStatus::SessionLocked => {
            require_empty_detail(detail)?;
            Ok(AgentFileTransferFailure::SessionLocked)
        }
        AgentFileTransferStatus::AgentNotConnected => {
            require_empty_detail(detail)?;
            Ok(AgentFileTransferFailure::AgentNotConnected)
        }
        AgentFileTransferStatus::Disabled => {
            require_empty_detail(detail)?;
            Ok(AgentFileTransferFailure::Disabled)
        }
        _ => Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "agent non-failure file transfer status",
        )),
    }
}

fn require_empty_detail(detail: &[u8]) -> Result<(), DecodeError> {
    if detail.is_empty() {
        Ok(())
    } else {
        Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "agent file transfer status detail",
        ))
    }
}

/// Decodes one packed audio volume update without accepting trailing data.
pub fn decode_audio_volume_sync(payload: &[u8]) -> Result<AgentAudioVolumeSync, DecodeError> {
    let mut reader = Reader::new(payload);
    let playback = reader.u8("agent audio direction")?;
    let mute = reader.u8("agent audio mute")?;
    let channel_count = usize::from(reader.u8("agent audio channel count")?);
    if playback > 1 || mute > 1 || channel_count == 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "agent audio volume header",
        ));
    }
    if channel_count > MAX_AGENT_AUDIO_CHANNELS {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            2,
            "agent audio channels",
        ));
    }
    let expected_volume_bytes = channel_count
        .checked_mul(2)
        .ok_or_else(|| DecodeError::new(DecodeErrorKind::Overflow, 3, "agent audio volumes"))?;
    if reader.remaining() != expected_volume_bytes {
        return Err(DecodeError::new(
            if reader.remaining() < expected_volume_bytes {
                DecodeErrorKind::Truncated
            } else {
                DecodeErrorKind::InvalidValue
            },
            reader.offset(),
            "agent audio volumes",
        ));
    }
    let mut volumes = Vec::with_capacity(channel_count);
    for _ in 0..channel_count {
        volumes.push(reader.u16("agent audio volume")?);
    }
    Ok(AgentAudioVolumeSync {
        is_playback: playback != 0,
        muted: mute != 0,
        volumes,
    })
}

/// Encodes one packed audio volume update with a bounded channel array.
pub fn encode_audio_volume_sync(volume: &AgentAudioVolumeSync) -> Result<Vec<u8>, DecodeError> {
    if volume.volumes.is_empty() {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "agent audio channels",
        ));
    }
    if volume.volumes.len() > MAX_AGENT_AUDIO_CHANNELS {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            0,
            "agent audio channels",
        ));
    }
    let channel_count = u8::try_from(volume.volumes.len())
        .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 0, "agent audio channel count"))?;
    let mut output = Vec::with_capacity(3 + volume.volumes.len() * 2);
    output.push(u8::from(volume.is_playback));
    output.push(u8::from(volume.muted));
    output.push(channel_count);
    for channel_volume in &volume.volumes {
        output.extend_from_slice(&channel_volume.to_le_bytes());
    }
    Ok(output)
}

/// Decodes a bounded sequence of packed display-device records.
pub fn decode_graphics_device_info(payload: &[u8]) -> Result<AgentGraphicsDeviceInfo, DecodeError> {
    let mut reader = Reader::new(payload);
    let display_count =
        usize::try_from(reader.u32("agent display device count")?).map_err(|_| {
            DecodeError::new(DecodeErrorKind::Overflow, 0, "agent display device count")
        })?;
    if display_count > MAX_AGENT_DISPLAY_DEVICES {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            0,
            "agent display devices",
        ));
    }
    let mut displays = Vec::with_capacity(display_count);
    for _ in 0..display_count {
        let channel_id = reader.u32("agent display channel id")?;
        let monitor_id = reader.u32("agent display monitor id")?;
        let device_display_id = reader.u32("agent device display id")?;
        let address_bytes =
            usize::try_from(reader.u32("agent device address length")?).map_err(|_| {
                DecodeError::new(
                    DecodeErrorKind::Overflow,
                    reader.offset(),
                    "agent device address",
                )
            })?;
        if address_bytes == 0 || address_bytes > MAX_AGENT_DEVICE_ADDRESS_BYTES {
            return Err(DecodeError::new(
                if address_bytes == 0 {
                    DecodeErrorKind::InvalidValue
                } else {
                    DecodeErrorKind::ResourceLimit
                },
                reader.offset(),
                "agent device address",
            ));
        }
        let address = reader.take(address_bytes, "agent device address")?;
        if address.last() != Some(&0) || address[..address.len() - 1].contains(&0) {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                reader.offset() - address_bytes,
                "agent device address terminator",
            ));
        }
        let address = std::str::from_utf8(&address[..address.len() - 1]).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::InvalidValue,
                reader.offset() - address_bytes,
                "agent device address UTF-8",
            )
        })?;
        displays.push(AgentDisplayDevice {
            channel_id,
            monitor_id,
            device_display_id,
            device_address: address.to_owned(),
        });
    }
    if reader.remaining() != 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            reader.offset(),
            "agent graphics device trailing data",
        ));
    }
    Ok(AgentGraphicsDeviceInfo { displays })
}

pub fn encode_file_transfer_status(
    transfer_id: u32,
    status: AgentFileTransferStatus,
) -> Result<[u8; 8], DecodeError> {
    if transfer_id == 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "agent file transfer id",
        ));
    }
    let mut output = [0; 8];
    output[..4].copy_from_slice(&transfer_id.to_le_bytes());
    output[4..].copy_from_slice(&(status as u32).to_le_bytes());
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded_message(message_type: u32, payload: &[u8]) -> Vec<u8> {
        let mut encoder = OutboundAgentMessage::new(
            message_type,
            7,
            Arc::from(payload),
            DEFAULT_MAX_AGENT_MESSAGE_BYTES,
        )
        .expect("bounded agent message");
        let mut encoded = Vec::new();
        let mut fragment = Vec::new();
        while !encoder.is_complete() {
            encoder
                .next_fragment(&mut fragment)
                .expect("next agent fragment");
            encoded.extend_from_slice(&fragment);
        }
        encoded
    }

    #[test]
    fn stream_reassembly_handles_split_headers_bodies_and_adjacent_messages() {
        let first_payload = vec![0x5a; MAX_AGENT_FRAGMENT_BYTES + 17];
        let mut stream = encoded_message(agent_message::CLIPBOARD, &first_payload);
        stream.extend_from_slice(&encoded_message(agent_message::CLIPBOARD_RELEASE, &[]));
        let mut decoder =
            AgentStreamDecoder::new(DEFAULT_MAX_AGENT_MESSAGE_BYTES).expect("agent decoder");
        let mut messages = Vec::new();
        let mut completed = Vec::new();
        for fragment in stream.chunks(37) {
            completed.clear();
            decoder
                .push_fragment_into(fragment, &mut completed)
                .expect("fragmented agent stream");
            messages.append(&mut completed);
        }
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].protocol, AGENT_PROTOCOL);
        assert_eq!(messages[0].message_type, agent_message::CLIPBOARD);
        assert_eq!(messages[0].opaque, 7);
        assert_eq!(messages[0].payload, first_payload);
        assert_eq!(messages[1].message_type, agent_message::CLIPBOARD_RELEASE);
        assert!(messages[1].payload.is_empty());
    }

    #[test]
    fn declared_message_bound_precedes_payload_allocation() {
        let mut header = encoded_message(agent_message::CLIPBOARD, &[]);
        header[16..20].copy_from_slice(&1025_u32.to_le_bytes());
        let mut decoder = AgentStreamDecoder::new(1024).expect("agent decoder");
        let mut completed = Vec::new();
        let error = decoder
            .push_fragment_into(&header, &mut completed)
            .expect_err("oversized agent message");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);
    }

    #[test]
    fn negotiated_clipboard_fields_have_exact_layouts() {
        let grab = encode_clipboard_grab(
            AgentClipboardSelection::Primary,
            Some(9),
            &[AgentClipboardType::Utf8Text],
            true,
        )
        .expect("clipboard grab");
        let decoded = decode_clipboard_grab(&grab, true, true).expect("clipboard grab wire");
        assert_eq!(decoded.selection, AgentClipboardSelection::Primary);
        assert_eq!(decoded.serial, Some(9));
        assert_eq!(decoded.types, [AgentClipboardType::Utf8Text as u32]);

        let data = encode_clipboard_data(
            AgentClipboardSelection::Clipboard,
            AgentClipboardType::Utf8Text,
            b"hello",
            true,
        )
        .expect("clipboard data");
        let decoded = decode_clipboard_data(&data, true).expect("clipboard data wire");
        assert_eq!(decoded.clipboard_type, AgentClipboardType::Utf8Text as u32);
        assert_eq!(decoded.data, b"hello");

        let error = encode_clipboard_release(AgentClipboardSelection::Primary, false)
            .expect_err("selection requires negotiated support");
        assert_eq!(error.kind, DecodeErrorKind::Unsupported);

        let oversized_types = vec![0; (MAX_AGENT_CLIPBOARD_TYPES + 1) * 4];
        let error = decode_clipboard_grab(&oversized_types, false, false)
            .expect_err("clipboard type count bound");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);
    }

    #[test]
    fn clipboard_file_list_round_trips_action_and_webdav_paths() {
        let file_list = AgentClipboardFileList {
            action: AgentClipboardFileAction::Copy,
            paths: vec![
                "/share/report.txt".to_owned(),
                "/share/image.png".to_owned(),
            ],
        };
        let encoded = encode_clipboard_file_list(&file_list).expect("clipboard file list");
        assert_eq!(encoded, b"copy\0/share/report.txt\0/share/image.png\0");
        assert_eq!(
            decode_clipboard_file_list(&encoded).expect("clipboard file list wire"),
            file_list
        );

        let error = decode_clipboard_file_list(b"copy\0relative.txt\0")
            .expect_err("file list paths are rooted in the WebDAV share");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }

    #[test]
    fn monitor_layout_is_bounded_and_preserves_wire_field_order() {
        let monitor = AgentMonitorConfig {
            width: 1920,
            height: 1080,
            depth: 32,
            x: -1920,
            y: 20,
            width_mm: Some(520),
            height_mm: Some(290),
        };
        let encoded =
            encode_monitors_config(&[monitor], true, false, true).expect("agent monitor layout");
        assert_eq!(&encoded[..4], &1_u32.to_le_bytes());
        assert_eq!(&encoded[8..12], &1080_u32.to_le_bytes());
        assert_eq!(&encoded[12..16], &1920_u32.to_le_bytes());
        assert_eq!(&encoded[28..30], &290_u16.to_le_bytes());
        assert_eq!(&encoded[30..32], &520_u16.to_le_bytes());

        let monitors = vec![monitor; MAX_AGENT_MONITORS + 1];
        let error =
            encode_monitors_config(&monitors, true, false, true).expect_err("monitor count bound");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);

        let mut invalid = monitor;
        invalid.depth = 0;
        let error =
            encode_monitors_config(&[invalid], true, false, true).expect_err("zero monitor depth");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);

        invalid = monitor;
        invalid.width = 0;
        let error = encode_monitors_config(&[invalid], true, false, true)
            .expect_err("sparse monitor requires capability");
        assert_eq!(error.kind, DecodeErrorKind::Unsupported);

        let error = encode_monitors_config(&[monitor], true, false, false)
            .expect_err("physical size requires capability");
        assert_eq!(error.kind, DecodeErrorKind::Unsupported);
    }

    #[test]
    fn file_transfer_metadata_and_chunk_sizes_are_exact() {
        let start = encode_file_transfer_start(7, "report.txt", 123).expect("file start");
        assert_eq!(&start[..4], &7_u32.to_le_bytes());
        assert!(start[4..].ends_with(&[0]));
        assert!(
            std::str::from_utf8(&start[4..start.len() - 1])
                .expect("metadata UTF-8")
                .contains("size=123\n")
        );
        let error = encode_file_transfer_start(7, "../report.txt", 123)
            .expect_err("host path is not a guest basename");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);

        let data = encode_file_transfer_data(7, &[0x5a; 17]).expect("file data");
        assert_eq!(&data[..4], &7_u32.to_le_bytes());
        assert_eq!(&data[4..12], &17_u64.to_le_bytes());
        assert_eq!(&data[12..], &[0x5a; 17]);

        let status =
            encode_file_transfer_status(7, AgentFileTransferStatus::Success).expect("file status");
        let status = decode_file_transfer_status(&status).expect("file status wire");
        assert_eq!(status.transfer_id, 7);
        assert_eq!(status.status, AgentFileTransferStatus::Success);
        assert!(status.detail.is_empty());
    }

    #[test]
    fn audio_volume_sync_checks_channel_count_and_exact_size() {
        let volume = AgentAudioVolumeSync {
            is_playback: true,
            muted: false,
            volumes: vec![u16::MAX, u16::MAX / 2],
        };
        let encoded = encode_audio_volume_sync(&volume).expect("audio volume sync");
        assert_eq!(
            decode_audio_volume_sync(&encoded).expect("audio volume wire"),
            volume
        );

        let error = decode_audio_volume_sync(&[1, 0, 2, 0, 0])
            .expect_err("declared channel count must match payload");
        assert_eq!(error.kind, DecodeErrorKind::Truncated);

        let oversized = AgentAudioVolumeSync {
            is_playback: false,
            muted: false,
            volumes: vec![0; MAX_AGENT_AUDIO_CHANNELS + 1],
        };
        let error = encode_audio_volume_sync(&oversized).expect_err("audio channel bound");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);
    }

    #[test]
    fn graphics_device_info_checks_variable_record_boundaries() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&1_u32.to_le_bytes());
        encoded.extend_from_slice(&2_u32.to_le_bytes());
        encoded.extend_from_slice(&3_u32.to_le_bytes());
        encoded.extend_from_slice(&4_u32.to_le_bytes());
        let device_address = b"pci/0.1\0";
        encoded.extend_from_slice(
            &u32::try_from(device_address.len())
                .expect("device address length")
                .to_le_bytes(),
        );
        encoded.extend_from_slice(device_address);
        let info = decode_graphics_device_info(&encoded).expect("graphics device info");
        assert_eq!(info.displays[0].channel_id, 2);
        assert_eq!(info.displays[0].device_address, "pci/0.1");

        encoded.pop();
        let error = decode_graphics_device_info(&encoded)
            .expect_err("device address requires its declared terminator");
        assert_eq!(error.kind, DecodeErrorKind::Truncated);
    }

    #[test]
    fn detailed_file_errors_preserve_remote_diagnostics() {
        let mut free_space = Vec::new();
        free_space.extend_from_slice(&4096_u64.to_le_bytes());
        assert_eq!(
            decode_file_transfer_failure(AgentFileTransferStatus::NotEnoughSpace, &free_space,)
                .expect("free-space detail"),
            AgentFileTransferFailure::NotEnoughSpace {
                available_bytes: Some(4096),
            }
        );

        let mut remote_error = vec![0];
        remote_error.extend_from_slice(&27_u32.to_le_bytes());
        assert_eq!(
            decode_file_transfer_failure(AgentFileTransferStatus::RemoteError, &remote_error)
                .expect("remote error detail"),
            AgentFileTransferFailure::RemoteError {
                error_domain: Some(0),
                error_code: Some(27),
            }
        );
    }
}
