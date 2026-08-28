//! Bounded stdio protocol for an application-owned OxideSpice helper process.

use std::fmt;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::{HelperHello, HelperHelloAck, HelperMetadata};

const MAX_JSON_LINE_BYTES: usize = 1024 * 1024;
const MAX_BINARY_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;

/// A transient serialized secret that clears its allocation when ownership ends.
#[derive(Deserialize, Serialize)]
#[serde(transparent)]
pub struct HelperSecret(String);

impl HelperSecret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_inner(mut self) -> String {
        std::mem::take(&mut self.0)
    }
}

impl fmt::Debug for HelperSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HelperSecret([REDACTED])")
    }
}

impl Drop for HelperSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HelperEndpoint {
    Tcp { host: String, port: u16 },
    Unix { path: PathBuf },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HelperTransportSecurity {
    Plain,
    Tls {
        server_name: String,
        root_certificates_der: Vec<Vec<u8>>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HelperSasl {
    Gssapi {
        hostname: String,
        #[serde(default = "default_sasl_service")]
        service: String,
    },
    Password {
        hostname: String,
        #[serde(default = "default_sasl_service")]
        service: String,
        authentication_id: String,
        authorization_id: Option<String>,
        password: HelperSecret,
        #[serde(default)]
        allow_gssapi: bool,
    },
}

fn default_sasl_service() -> String {
    "spice".to_owned()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperConnectOptions {
    pub endpoint: HelperEndpoint,
    pub ticket: HelperSecret,
    #[serde(default = "plain_transport_security")]
    pub transport_security: HelperTransportSecurity,
    pub sasl: Option<HelperSasl>,
}

fn plain_transport_security() -> HelperTransportSecurity {
    HelperTransportSecurity::Plain
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperUsbDeviceIdentity {
    pub bus_number: u8,
    pub device_address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
}

/// Runtime availability of a native device service on the current host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HelperNativeBackendStatus {
    Available,
    Unavailable { reason: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperPixelFormat {
    Rgba8,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperAudioDataMode {
    Raw,
    Celt051,
    Opus,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperPlaybackStateKind {
    AwaitingMode,
    Stopped,
    Started,
    Closed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperRecordStateKind {
    Stopped,
    StartRequested,
    Recording,
    Closed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperPortStateKind {
    AwaitingInit,
    Ready,
    Closed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperButtonState {
    Pressed,
    Released,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperMouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    Side,
    Extra,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperMouseMode {
    Server,
    Client,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperKeyState {
    Pressed,
    Released,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperClipboardSelection {
    Clipboard,
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperClipboardFormat {
    Utf8Text,
    ImagePng,
    ImageBmp,
    ImageTiff,
    ImageJpeg,
    FileList,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperMonitor {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub x: i32,
    pub y: i32,
    pub width_mm: Option<u16>,
    pub height_mm: Option<u16>,
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HelperRequest {
    Hello {
        hello: HelperHello,
    },
    Connect {
        options: HelperConnectOptions,
    },
    PointerPosition {
        x: u32,
        y: u32,
        buttons: u16,
        display_id: u8,
    },
    PointerMotion {
        dx: i32,
        dy: i32,
        buttons: u16,
    },
    MouseButton {
        button: HelperMouseButton,
        state: HelperButtonState,
        buttons: u16,
    },
    KeyCode {
        code: u32,
        state: HelperKeyState,
    },
    Scancodes {
        bytes: Vec<u8>,
    },
    Modifiers {
        bits: u16,
    },
    ClipboardOffer {
        selection: HelperClipboardSelection,
        formats: Vec<HelperClipboardFormat>,
    },
    ClipboardRelease {
        selection: HelperClipboardSelection,
    },
    ClipboardRequest {
        selection: HelperClipboardSelection,
        format: HelperClipboardFormat,
    },
    ClipboardProvide {
        request_id: u64,
        data: Vec<u8>,
    },
    FileTransferStart {
        transfer_id: u64,
        file_name: String,
        size: u64,
    },
    FileTransferData {
        transfer_id: u64,
        data: Vec<u8>,
    },
    FileTransferFinish {
        transfer_id: u64,
    },
    FileTransferCancel {
        transfer_id: u64,
    },
    PortWrite {
        channel_id: u8,
        data: Vec<u8>,
    },
    PortBreak {
        channel_id: u8,
    },
    MonitorLayout {
        monitors: Vec<HelperMonitor>,
    },
    SyncAgentAudioVolume {
        is_playback: bool,
        muted: bool,
        volumes: Vec<u16>,
    },
    RecordBegin {
        channel_id: u8,
    },
    RecordData {
        channel_id: u8,
        timestamp_ms: u32,
        pcm_s16le: Vec<u8>,
    },
    ListNativeDevices,
    StartWebDav {
        channel_id: u8,
        root: PathBuf,
        read_only: bool,
    },
    StartUsbRedirection {
        channel_id: u8,
        device: HelperUsbDeviceIdentity,
    },
    StartSmartcardRedirection {
        channel_id: u8,
        display_name: String,
    },
    Close,
}

impl fmt::Debug for HelperRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hello { hello } => formatter.debug_tuple("Hello").field(hello).finish(),
            Self::Connect { options } => formatter
                .debug_struct("Connect")
                .field("endpoint", &options.endpoint)
                .field("transport_security", &options.transport_security)
                .field("ticket", &options.ticket)
                .field("sasl", &options.sasl)
                .finish(),
            Self::ClipboardProvide { request_id, data } => formatter
                .debug_struct("ClipboardProvide")
                .field("request_id", request_id)
                .field("data", &format_args!("<{} bytes>", data.len()))
                .finish(),
            Self::FileTransferStart {
                transfer_id,
                file_name,
                size,
            } => formatter
                .debug_struct("FileTransferStart")
                .field("transfer_id", transfer_id)
                .field("file_name", file_name)
                .field("size", size)
                .finish(),
            Self::FileTransferData { transfer_id, data } => formatter
                .debug_struct("FileTransferData")
                .field("transfer_id", transfer_id)
                .field("data", &format_args!("<{} bytes>", data.len()))
                .finish(),
            Self::FileTransferFinish { transfer_id } => formatter
                .debug_struct("FileTransferFinish")
                .field("transfer_id", transfer_id)
                .finish(),
            Self::FileTransferCancel { transfer_id } => formatter
                .debug_struct("FileTransferCancel")
                .field("transfer_id", transfer_id)
                .finish(),
            Self::PortWrite { channel_id, data } => formatter
                .debug_struct("PortWrite")
                .field("channel_id", channel_id)
                .field("data", &format_args!("<{} bytes>", data.len()))
                .finish(),
            Self::PortBreak { channel_id } => formatter
                .debug_struct("PortBreak")
                .field("channel_id", channel_id)
                .finish(),
            Self::RecordData {
                channel_id,
                timestamp_ms,
                pcm_s16le,
            } => formatter
                .debug_struct("RecordData")
                .field("channel_id", channel_id)
                .field("timestamp_ms", timestamp_ms)
                .field("pcm_s16le", &format_args!("<{} bytes>", pcm_s16le.len()))
                .finish(),
            Self::PointerPosition { .. } => formatter.write_str("PointerPosition"),
            Self::PointerMotion { .. } => formatter.write_str("PointerMotion"),
            Self::MouseButton { .. } => formatter.write_str("MouseButton"),
            Self::KeyCode { .. } => formatter.write_str("KeyCode"),
            Self::Scancodes { bytes } => formatter
                .debug_tuple("Scancodes")
                .field(&format_args!("<{} bytes>", bytes.len()))
                .finish(),
            Self::Modifiers { .. } => formatter.write_str("Modifiers"),
            Self::ClipboardOffer { .. } => formatter.write_str("ClipboardOffer"),
            Self::ClipboardRelease { .. } => formatter.write_str("ClipboardRelease"),
            Self::ClipboardRequest { .. } => formatter.write_str("ClipboardRequest"),
            Self::MonitorLayout { monitors } => formatter
                .debug_struct("MonitorLayout")
                .field("monitor_count", &monitors.len())
                .finish(),
            Self::SyncAgentAudioVolume {
                is_playback,
                muted,
                volumes,
            } => formatter
                .debug_struct("SyncAgentAudioVolume")
                .field("is_playback", is_playback)
                .field("muted", muted)
                .field("channel_count", &volumes.len())
                .finish(),
            Self::RecordBegin { channel_id } => formatter
                .debug_struct("RecordBegin")
                .field("channel_id", channel_id)
                .finish(),
            Self::ListNativeDevices => formatter.write_str("ListNativeDevices"),
            Self::StartWebDav {
                channel_id,
                root,
                read_only,
            } => formatter
                .debug_struct("StartWebDav")
                .field("channel_id", channel_id)
                .field("root", root)
                .field("read_only", read_only)
                .finish(),
            Self::StartUsbRedirection { channel_id, device } => formatter
                .debug_struct("StartUsbRedirection")
                .field("channel_id", channel_id)
                .field("device", device)
                .finish(),
            Self::StartSmartcardRedirection {
                channel_id,
                display_name,
            } => formatter
                .debug_struct("StartSmartcardRedirection")
                .field("channel_id", channel_id)
                .field("display_name", display_name)
                .finish(),
            Self::Close => formatter.write_str("Close"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperStatus {
    Connecting,
    Connected,
    Closing,
    Disconnected,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperErrorCategory {
    Configuration,
    Network,
    Tls,
    Authentication,
    Protocol,
    Negotiation,
    Unsupported,
    ResourceLimit,
    RemoteDisconnect,
    Cancelled,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperChannelCapabilities {
    pub inputs: bool,
    pub raw_scancodes: bool,
    pub cursor: bool,
    pub agent: bool,
    pub playback_channel_ids: Vec<u8>,
    pub record_channel_ids: Vec<u8>,
    pub port_channel_ids: Vec<u8>,
    pub usbredir_channel_ids: Vec<u8>,
    pub smartcard_channel_ids: Vec<u8>,
    pub webdav_channel_ids: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperTopologyMonitor {
    pub id: u32,
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperAgentStateKind {
    Disconnected,
    Negotiating,
    Ready,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperFileTransferState {
    WaitingForGuest,
    Sending,
    AwaitingCompletion,
    Completed,
    Cancelled,
    Failed,
    AgentDisconnected,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HelperFileTransferFailure {
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperAgentFeatures {
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperGraphicsDevice {
    pub channel_id: u32,
    pub monitor_id: u32,
    pub device_display_id: u32,
    pub device_address: String,
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HelperEvent {
    HelloAck {
        acknowledgement: HelperHelloAck,
    },
    Status {
        status: HelperStatus,
        message: Option<String>,
    },
    Connected {
        session_id: u32,
        capabilities: HelperChannelCapabilities,
    },
    ServerIdentity {
        name: Option<String>,
        uuid: Option<[u8; 16]>,
    },
    MouseMode {
        mode: HelperMouseMode,
    },
    KeyboardModifiers {
        bits: u16,
    },
    Topology {
        connection_generation: u64,
        graphics_epoch: u64,
        display_channel_id: u8,
        maximum_allowed: u16,
        monitors: Vec<HelperTopologyMonitor>,
    },
    Frame {
        connection_generation: u64,
        graphics_epoch: u64,
        display_channel_id: u8,
        surface_id: u32,
        surface_width: u32,
        surface_height: u32,
        rect: HelperRect,
        full_refresh: bool,
        format: HelperPixelFormat,
        pixels: Vec<u8>,
    },
    Cursor {
        connection_generation: u64,
        cursor_epoch: u64,
        channel_id: u8,
        x: i32,
        y: i32,
        visible: bool,
        width: u16,
        height: u16,
        hot_spot_x: u16,
        hot_spot_y: u16,
        shape_id: Option<u64>,
        rgba: Vec<u8>,
    },
    ClipboardOffer {
        selection: HelperClipboardSelection,
        revision: u64,
        formats: Vec<HelperClipboardFormat>,
    },
    ClipboardRequest {
        request_id: u64,
        selection: HelperClipboardSelection,
        format: HelperClipboardFormat,
    },
    ClipboardData {
        selection: HelperClipboardSelection,
        format: HelperClipboardFormat,
        data: Vec<u8>,
    },
    PlaybackData {
        channel_id: u8,
        stream_generation: u64,
        sequence: u64,
        timestamp_ms: u32,
        channels: u32,
        sample_rate_hz: u32,
        discontinuity: bool,
        pcm_s16le: Vec<u8>,
    },
    PlaybackState {
        connection_generation: u64,
        channel_id: u8,
        stream_generation: Option<u64>,
        state: HelperPlaybackStateKind,
        mode_timestamp_ms: Option<u32>,
        start_timestamp_ms: Option<u32>,
        channels: Option<u32>,
        sample_rate_hz: Option<u32>,
    },
    PlaybackSettings {
        channel_id: u8,
        volumes: Vec<u16>,
        muted: bool,
        latency_ms: Option<u32>,
    },
    RecordState {
        connection_generation: u64,
        channel_id: u8,
        stream_generation: u64,
        state: HelperRecordStateKind,
        start_timestamp_ms: Option<u32>,
        mode: Option<HelperAudioDataMode>,
        channels: Option<u32>,
        sample_rate_hz: Option<u32>,
    },
    RecordSettings {
        channel_id: u8,
        volumes: Vec<u16>,
        muted: bool,
    },
    PortState {
        connection_generation: u64,
        channel_id: u8,
        state: HelperPortStateKind,
        name: Option<String>,
        opened: bool,
    },
    PortData {
        channel_id: u8,
        discontinuity: bool,
        data: Vec<u8>,
    },
    PortBreak {
        channel_id: u8,
    },
    NativeDevices {
        usb_devices: Vec<HelperUsbDeviceIdentity>,
        usb_status: HelperNativeBackendStatus,
        smartcard_readers: Vec<String>,
        smartcard_status: HelperNativeBackendStatus,
    },
    AgentState {
        connection_generation: u64,
        agent_generation: u64,
        state: HelperAgentStateKind,
        reason: Option<u32>,
        features: Option<HelperAgentFeatures>,
    },
    AgentAudioVolume {
        connection_generation: u64,
        agent_generation: u64,
        is_playback: bool,
        muted: bool,
        volumes: Vec<u16>,
    },
    AgentAudioVolumeReset,
    AgentGraphicsDevices {
        connection_generation: u64,
        agent_generation: u64,
        displays: Vec<HelperGraphicsDevice>,
    },
    AgentGraphicsDevicesReset,
    FileTransferState {
        transfer_id: u64,
        state: HelperFileTransferState,
        accepted_bytes: u64,
        failure: Option<HelperFileTransferFailure>,
    },
    Error {
        category: HelperErrorCategory,
        message: String,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum BinaryRequestHeader {
    ClipboardProvideBinary {
        request_id: u64,
        payload_len: usize,
    },
    RecordDataBinary {
        channel_id: u8,
        timestamp_ms: u32,
        payload_len: usize,
    },
    FileTransferDataBinary {
        transfer_id: u64,
        payload_len: usize,
    },
    PortWriteBinary {
        channel_id: u8,
        payload_len: usize,
    },
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum InitialRequest {
    Hello { hello: HelperHello },
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum BinaryEventHeader {
    FrameBinary {
        connection_generation: u64,
        graphics_epoch: u64,
        display_channel_id: u8,
        surface_id: u32,
        surface_width: u32,
        surface_height: u32,
        rect: HelperRect,
        full_refresh: bool,
        format: HelperPixelFormat,
        payload_len: usize,
    },
    CursorBinary {
        connection_generation: u64,
        cursor_epoch: u64,
        channel_id: u8,
        x: i32,
        y: i32,
        visible: bool,
        width: u16,
        height: u16,
        hot_spot_x: u16,
        hot_spot_y: u16,
        shape_id: Option<u64>,
        payload_len: usize,
    },
    ClipboardDataBinary {
        selection: HelperClipboardSelection,
        format: HelperClipboardFormat,
        payload_len: usize,
    },
    PlaybackDataBinary {
        channel_id: u8,
        stream_generation: u64,
        sequence: u64,
        timestamp_ms: u32,
        channels: u32,
        sample_rate_hz: u32,
        discontinuity: bool,
        payload_len: usize,
    },
    PortDataBinary {
        channel_id: u8,
        discontinuity: bool,
        payload_len: usize,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum HelperIpcError {
    #[error("helper IPC I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("helper IPC JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("helper IPC line exceeds {MAX_JSON_LINE_BYTES} bytes")]
    LineTooLarge,
    #[error("helper IPC line is not UTF-8")]
    InvalidUtf8,
    #[error("the first helper IPC request must be Hello")]
    ExpectedHello,
    #[error("helper IPC binary payload exceeds {MAX_BINARY_PAYLOAD_BYTES} bytes")]
    BinaryPayloadTooLarge,
    #[error("helper IPC binary payload does not match its metadata")]
    InvalidBinaryPayload,
    #[error("Agent file-transfer chunk exceeds the SPICE chunk bound")]
    FileTransferChunkTooLarge,
    #[error("Port payload exceeds the SPICE payload bound")]
    PortPayloadTooLarge,
}

/// Reads only the credential-free Hello shape used before the version gate opens.
pub fn read_initial_hello(
    reader: &mut impl BufRead,
) -> Result<Option<HelperHello>, HelperIpcError> {
    let Some(line) = read_bounded_line(reader)? else {
        return Ok(None);
    };
    let request =
        serde_json::from_str::<InitialRequest>(&line).map_err(|_| HelperIpcError::ExpectedHello)?;
    Ok(Some(match request {
        InitialRequest::Hello { hello } => hello,
    }))
}

pub fn read_request(reader: &mut impl BufRead) -> Result<Option<HelperRequest>, HelperIpcError> {
    let Some(line) = read_bounded_line(reader)? else {
        return Ok(None);
    };
    if let Ok(header) = serde_json::from_str::<BinaryRequestHeader>(&line) {
        let payload_len = match header {
            BinaryRequestHeader::ClipboardProvideBinary { payload_len, .. }
            | BinaryRequestHeader::RecordDataBinary { payload_len, .. }
            | BinaryRequestHeader::FileTransferDataBinary { payload_len, .. }
            | BinaryRequestHeader::PortWriteBinary { payload_len, .. } => payload_len,
        };
        if matches!(header, BinaryRequestHeader::FileTransferDataBinary { .. })
            && payload_len > oxide_spice_protocol::MAX_AGENT_FILE_CHUNK_BYTES
        {
            return Err(HelperIpcError::FileTransferChunkTooLarge);
        }
        if matches!(header, BinaryRequestHeader::PortWriteBinary { .. })
            && payload_len > oxide_spice_protocol::MAX_PORT_DATA_BYTES
        {
            return Err(HelperIpcError::PortPayloadTooLarge);
        }
        let payload = read_payload(reader, payload_len)?;
        return Ok(Some(match header {
            BinaryRequestHeader::ClipboardProvideBinary { request_id, .. } => {
                HelperRequest::ClipboardProvide {
                    request_id,
                    data: payload,
                }
            }
            BinaryRequestHeader::RecordDataBinary {
                channel_id,
                timestamp_ms,
                ..
            } => HelperRequest::RecordData {
                channel_id,
                timestamp_ms,
                pcm_s16le: payload,
            },
            BinaryRequestHeader::FileTransferDataBinary { transfer_id, .. } => {
                HelperRequest::FileTransferData {
                    transfer_id,
                    data: payload,
                }
            }
            BinaryRequestHeader::PortWriteBinary { channel_id, .. } => HelperRequest::PortWrite {
                channel_id,
                data: payload,
            },
        }));
    }
    Ok(Some(serde_json::from_str(&line)?))
}

pub fn write_request(
    writer: &mut impl Write,
    request: &HelperRequest,
) -> Result<(), HelperIpcError> {
    match request {
        HelperRequest::ClipboardProvide { request_id, data } => write_binary(
            writer,
            &BinaryRequestHeader::ClipboardProvideBinary {
                request_id: *request_id,
                payload_len: data.len(),
            },
            data,
        ),
        HelperRequest::RecordData {
            channel_id,
            timestamp_ms,
            pcm_s16le,
        } => write_binary(
            writer,
            &BinaryRequestHeader::RecordDataBinary {
                channel_id: *channel_id,
                timestamp_ms: *timestamp_ms,
                payload_len: pcm_s16le.len(),
            },
            pcm_s16le,
        ),
        HelperRequest::FileTransferData { transfer_id, data } => {
            if data.len() > oxide_spice_protocol::MAX_AGENT_FILE_CHUNK_BYTES {
                return Err(HelperIpcError::FileTransferChunkTooLarge);
            }
            write_binary(
                writer,
                &BinaryRequestHeader::FileTransferDataBinary {
                    transfer_id: *transfer_id,
                    payload_len: data.len(),
                },
                data,
            )
        }
        HelperRequest::PortWrite { channel_id, data } => {
            if data.len() > oxide_spice_protocol::MAX_PORT_DATA_BYTES {
                return Err(HelperIpcError::PortPayloadTooLarge);
            }
            write_binary(
                writer,
                &BinaryRequestHeader::PortWriteBinary {
                    channel_id: *channel_id,
                    payload_len: data.len(),
                },
                data,
            )
        }
        _ => write_json_line(writer, request),
    }
}

pub fn write_event(writer: &mut impl Write, event: &HelperEvent) -> Result<(), HelperIpcError> {
    match event {
        HelperEvent::Frame {
            connection_generation,
            graphics_epoch,
            display_channel_id,
            surface_id,
            surface_width,
            surface_height,
            rect,
            full_refresh,
            format,
            pixels,
        } => write_binary(
            writer,
            &BinaryEventHeader::FrameBinary {
                connection_generation: *connection_generation,
                graphics_epoch: *graphics_epoch,
                display_channel_id: *display_channel_id,
                surface_id: *surface_id,
                surface_width: *surface_width,
                surface_height: *surface_height,
                rect: *rect,
                full_refresh: *full_refresh,
                format: *format,
                payload_len: pixels.len(),
            },
            pixels,
        ),
        HelperEvent::Cursor {
            connection_generation,
            cursor_epoch,
            channel_id,
            x,
            y,
            visible,
            width,
            height,
            hot_spot_x,
            hot_spot_y,
            shape_id,
            rgba,
        } if !rgba.is_empty() => write_binary(
            writer,
            &BinaryEventHeader::CursorBinary {
                connection_generation: *connection_generation,
                cursor_epoch: *cursor_epoch,
                channel_id: *channel_id,
                x: *x,
                y: *y,
                visible: *visible,
                width: *width,
                height: *height,
                hot_spot_x: *hot_spot_x,
                hot_spot_y: *hot_spot_y,
                shape_id: *shape_id,
                payload_len: rgba.len(),
            },
            rgba,
        ),
        HelperEvent::ClipboardData {
            selection,
            format,
            data,
        } => write_binary(
            writer,
            &BinaryEventHeader::ClipboardDataBinary {
                selection: *selection,
                format: *format,
                payload_len: data.len(),
            },
            data,
        ),
        HelperEvent::PlaybackData {
            channel_id,
            stream_generation,
            sequence,
            timestamp_ms,
            channels,
            sample_rate_hz,
            discontinuity,
            pcm_s16le,
        } => write_binary(
            writer,
            &BinaryEventHeader::PlaybackDataBinary {
                channel_id: *channel_id,
                stream_generation: *stream_generation,
                sequence: *sequence,
                timestamp_ms: *timestamp_ms,
                channels: *channels,
                sample_rate_hz: *sample_rate_hz,
                discontinuity: *discontinuity,
                payload_len: pcm_s16le.len(),
            },
            pcm_s16le,
        ),
        HelperEvent::PortData {
            channel_id,
            discontinuity,
            data,
        } => write_binary(
            writer,
            &BinaryEventHeader::PortDataBinary {
                channel_id: *channel_id,
                discontinuity: *discontinuity,
                payload_len: data.len(),
            },
            data,
        ),
        _ => write_json_line(writer, event),
    }
}

pub fn read_event(reader: &mut impl BufRead) -> Result<Option<HelperEvent>, HelperIpcError> {
    let Some(line) = read_bounded_line(reader)? else {
        return Ok(None);
    };
    let Ok(header) = serde_json::from_str::<BinaryEventHeader>(&line) else {
        return Ok(Some(serde_json::from_str(&line)?));
    };
    let payload_len = match &header {
        BinaryEventHeader::FrameBinary { payload_len, .. }
        | BinaryEventHeader::CursorBinary { payload_len, .. }
        | BinaryEventHeader::ClipboardDataBinary { payload_len, .. }
        | BinaryEventHeader::PlaybackDataBinary { payload_len, .. }
        | BinaryEventHeader::PortDataBinary { payload_len, .. } => *payload_len,
    };
    let payload = read_payload(reader, payload_len)?;
    Ok(Some(match header {
        BinaryEventHeader::FrameBinary {
            connection_generation,
            graphics_epoch,
            display_channel_id,
            surface_id,
            surface_width,
            surface_height,
            rect,
            full_refresh,
            format,
            ..
        } => {
            let expected = checked_rgba_bytes(rect.width, rect.height)?;
            if payload.len() != expected {
                return Err(HelperIpcError::InvalidBinaryPayload);
            }
            HelperEvent::Frame {
                connection_generation,
                graphics_epoch,
                display_channel_id,
                surface_id,
                surface_width,
                surface_height,
                rect,
                full_refresh,
                format,
                pixels: payload,
            }
        }
        BinaryEventHeader::CursorBinary {
            connection_generation,
            cursor_epoch,
            channel_id,
            x,
            y,
            visible,
            width,
            height,
            hot_spot_x,
            hot_spot_y,
            shape_id,
            ..
        } => {
            let expected = checked_rgba_bytes(u32::from(width), u32::from(height))?;
            if payload.len() != expected {
                return Err(HelperIpcError::InvalidBinaryPayload);
            }
            HelperEvent::Cursor {
                connection_generation,
                cursor_epoch,
                channel_id,
                x,
                y,
                visible,
                width,
                height,
                hot_spot_x,
                hot_spot_y,
                shape_id,
                rgba: payload,
            }
        }
        BinaryEventHeader::ClipboardDataBinary {
            selection, format, ..
        } => HelperEvent::ClipboardData {
            selection,
            format,
            data: payload,
        },
        BinaryEventHeader::PlaybackDataBinary {
            channel_id,
            stream_generation,
            sequence,
            timestamp_ms,
            channels,
            sample_rate_hz,
            discontinuity,
            ..
        } => {
            let frame_bytes = usize::try_from(channels)
                .ok()
                .and_then(|channels| channels.checked_mul(2))
                .filter(|frame_bytes| *frame_bytes != 0)
                .ok_or(HelperIpcError::InvalidBinaryPayload)?;
            if !payload.len().is_multiple_of(frame_bytes) {
                return Err(HelperIpcError::InvalidBinaryPayload);
            }
            HelperEvent::PlaybackData {
                channel_id,
                stream_generation,
                sequence,
                timestamp_ms,
                channels,
                sample_rate_hz,
                discontinuity,
                pcm_s16le: payload,
            }
        }
        BinaryEventHeader::PortDataBinary {
            channel_id,
            discontinuity,
            ..
        } => {
            if payload.len() > oxide_spice_protocol::MAX_PORT_DATA_BYTES {
                return Err(HelperIpcError::PortPayloadTooLarge);
            }
            HelperEvent::PortData {
                channel_id,
                discontinuity,
                data: payload,
            }
        }
    }))
}

/// Writes one newline-terminated helper metadata document.
pub fn write_metadata(
    writer: &mut impl Write,
    metadata: &HelperMetadata,
) -> Result<(), HelperIpcError> {
    write_json_line(writer, metadata)
}

/// Reads one bounded helper metadata document.
pub fn read_metadata(reader: &mut impl BufRead) -> Result<Option<HelperMetadata>, HelperIpcError> {
    let Some(line) = read_bounded_line(reader)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&line)?))
}

fn read_bounded_line(
    reader: &mut impl BufRead,
) -> Result<Option<Zeroizing<String>>, HelperIpcError> {
    let mut bytes = Zeroizing::new(Vec::new());
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            break;
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(consumed) > MAX_JSON_LINE_BYTES {
            return Err(HelperIpcError::LineTooLarge);
        }
        bytes.extend_from_slice(&available[..consumed]);
        let complete = available.get(consumed.wrapping_sub(1)) == Some(&b'\n');
        reader.consume(consumed);
        if complete {
            break;
        }
    }
    while matches!(bytes.last(), Some(b'\r' | b'\n')) {
        bytes.pop();
    }
    String::from_utf8(std::mem::take(&mut *bytes))
        .map(|line| Some(Zeroizing::new(line)))
        .map_err(|_| HelperIpcError::InvalidUtf8)
}

fn read_payload(reader: &mut impl Read, payload_len: usize) -> Result<Vec<u8>, HelperIpcError> {
    validate_payload_len(payload_len)?;
    let mut payload = vec![0; payload_len];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

fn write_json_line(writer: &mut impl Write, value: &impl Serialize) -> Result<(), HelperIpcError> {
    let mut line = Zeroizing::new(serde_json::to_vec(value)?);
    if line.len() >= MAX_JSON_LINE_BYTES {
        return Err(HelperIpcError::LineTooLarge);
    }
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}

fn write_binary(
    writer: &mut impl Write,
    header: &impl Serialize,
    payload: &[u8],
) -> Result<(), HelperIpcError> {
    validate_payload_len(payload.len())?;
    let mut line = Zeroizing::new(serde_json::to_vec(header)?);
    if line.len() >= MAX_JSON_LINE_BYTES {
        return Err(HelperIpcError::LineTooLarge);
    }
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn validate_payload_len(payload_len: usize) -> Result<(), HelperIpcError> {
    if payload_len > MAX_BINARY_PAYLOAD_BYTES {
        Err(HelperIpcError::BinaryPayloadTooLarge)
    } else {
        Ok(())
    }
}

fn checked_rgba_bytes(width: u32, height: u32) -> Result<usize, HelperIpcError> {
    usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(usize::try_from(height).ok()?))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(HelperIpcError::InvalidBinaryPayload)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn binary_clipboard_request_round_trips_without_json_expansion() {
        let request = HelperRequest::ClipboardProvide {
            request_id: 7,
            data: vec![0, 1, 2, 255],
        };
        let mut wire = Vec::new();
        write_request(&mut wire, &request).expect("encode binary request");
        assert!(!wire.windows(9).any(|window| window == b"[0,1,2,2"));
        let decoded = read_request(&mut Cursor::new(wire))
            .expect("decode binary request")
            .expect("request exists");
        let HelperRequest::ClipboardProvide { request_id, data } = decoded else {
            panic!("unexpected request type");
        };
        assert_eq!(request_id, 7);
        assert_eq!(data, [0, 1, 2, 255]);
    }

    #[test]
    fn connect_debug_redacts_ticket_and_sasl_password() {
        let request = HelperRequest::Connect {
            options: HelperConnectOptions {
                endpoint: HelperEndpoint::Tcp {
                    host: "example.test".to_owned(),
                    port: 5900,
                },
                ticket: HelperSecret::new("ticket-secret"),
                transport_security: HelperTransportSecurity::Plain,
                sasl: Some(HelperSasl::Password {
                    hostname: "example.test".to_owned(),
                    service: "spice".to_owned(),
                    authentication_id: "user".to_owned(),
                    authorization_id: None,
                    password: HelperSecret::new("sasl-secret"),
                    allow_gssapi: false,
                }),
            },
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("ticket-secret"));
        assert!(!debug.contains("sasl-secret"));
    }

    #[test]
    fn binary_frame_event_round_trips_with_exact_region_size() {
        let event = HelperEvent::Frame {
            connection_generation: 1,
            graphics_epoch: 2,
            display_channel_id: 0,
            surface_id: 7,
            surface_width: 4,
            surface_height: 3,
            rect: HelperRect {
                x: 1,
                y: 1,
                width: 2,
                height: 1,
            },
            full_refresh: false,
            format: HelperPixelFormat::Rgba8,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let mut wire = Vec::new();
        write_event(&mut wire, &event).expect("encode frame event");
        let decoded = read_event(&mut Cursor::new(wire))
            .expect("decode frame event")
            .expect("event exists");
        let HelperEvent::Frame { rect, pixels, .. } = decoded else {
            panic!("unexpected event type");
        };
        assert_eq!(rect.width, 2);
        assert_eq!(pixels.len(), 8);
    }

    #[test]
    fn file_transfer_binary_chunk_enforces_protocol_bound() {
        let request = HelperRequest::FileTransferData {
            transfer_id: 9,
            data: vec![0; oxide_spice_protocol::MAX_AGENT_FILE_CHUNK_BYTES + 1],
        };
        let error = write_request(&mut Vec::new(), &request).expect_err("reject oversized chunk");
        assert!(matches!(error, HelperIpcError::FileTransferChunkTooLarge));
    }
}
