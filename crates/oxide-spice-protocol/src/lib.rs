//! Checked byte-level types for SPICE protocol version 2.x.
//!
//! This crate has no network runtime and performs no host pointer casts. Every wire offset is
//! resolved against the current bounded message body.

mod agent;
mod capability;
mod channel;
mod cursor;
mod display;
mod error;
mod inputs;
mod link;
mod main_channel;
mod playback;
mod port;
mod record;
mod smartcard;
mod spicevmc;
mod usbredir;
mod wire;

pub use agent::{
    AGENT_MESSAGE_HEADER_BYTES, AGENT_PROTOCOL, AgentAudioVolumeSync, AgentCapabilities,
    AgentClipboardData, AgentClipboardFileAction, AgentClipboardFileList, AgentClipboardGrab,
    AgentClipboardRequest, AgentClipboardSelection, AgentClipboardType, AgentDisplayDevice,
    AgentFileTransferFailure, AgentFileTransferStatus, AgentFileTransferStatusMessage,
    AgentGraphicsDeviceInfo, AgentMessage, AgentMonitorConfig, AgentStreamDecoder,
    DEFAULT_MAX_AGENT_MESSAGE_BYTES, MAX_AGENT_AUDIO_CHANNELS, MAX_AGENT_CLIPBOARD_FILE_PATHS,
    MAX_AGENT_CLIPBOARD_TYPES, MAX_AGENT_DEVICE_ADDRESS_BYTES, MAX_AGENT_DISPLAY_DEVICES,
    MAX_AGENT_FILE_CHUNK_BYTES, MAX_AGENT_FILE_NAME_BYTES, MAX_AGENT_FRAGMENT_BYTES,
    MAX_AGENT_MONITORS, OutboundAgentMessage, agent_capability, agent_message,
    decode_audio_volume_sync, decode_clipboard_data, decode_clipboard_file_list,
    decode_clipboard_grab, decode_clipboard_release, decode_clipboard_request,
    decode_file_transfer_failure, decode_file_transfer_status, decode_graphics_device_info,
    encode_audio_volume_sync, encode_clipboard_data, encode_clipboard_file_list,
    encode_clipboard_grab, encode_clipboard_release, encode_clipboard_request,
    encode_file_transfer_data, encode_file_transfer_start, encode_file_transfer_status,
    encode_monitors_config,
};
pub use capability::{CapabilitySet, MAX_CAPABILITY_WORDS};
pub use channel::{
    ChannelType, ChannelWait, DataHeader, Framing, MAX_CHANNEL_WAITS, WaitForChannels,
    common_client, common_server,
};
pub use cursor::{
    CursorHeader, CursorImage, CursorInit, CursorPosition, CursorSet, CursorType, cursor_server,
    decode_cursor_cache_id, decode_cursor_position, decode_cursor_trail,
};
pub use display::{
    BitmapFormat, BitmapPalette, BitmapUpdate, CompositeClip, CompositeEmbeddedImage,
    CompositeImage, CompositeSurface, CompositeTransform, CompressedImageUpdate, CopyBits,
    DisplayInit, DrawComposite, DrawCopyImageType, EmbeddedBitmap, EmbeddedCompressedImage,
    EmbeddedImage, EmbeddedJpeg, GlDraw, GlScanout2Unix, GlScanoutPlane, GlScanoutUnix,
    ImageCompression, JpegUpdate, MAX_COMPOSITE_CLIP_RECTS, MAX_GL_SCANOUT_PLANES,
    MAX_MONITOR_HEADS, MonitorHead, MonitorsConfig, Point16, Rect, SolidFill, StreamClip,
    StreamClipUpdate, StreamCreate, StreamData, StreamReport, StreamReportActivation,
    SurfaceCreate, SurfaceFormat, VideoCodec, display_client, display_server,
    encode_preferred_video_codecs,
};
pub use error::{DecodeError, DecodeErrorKind};
pub use inputs::{
    INPUT_MOTION_ACK_BUNCH, KeyboardModifiers, MouseButton, MouseButtons, encode_key_code,
    encode_mouse_button, encode_mouse_motion, encode_mouse_position, inputs_capability,
    inputs_client, inputs_server,
};
pub use link::{
    AUTH_MECHANISM_SASL, AUTH_MECHANISM_SPICE, LINK_HEADER_SIZE, LINK_MESSAGE_FIXED_SIZE,
    LINK_REPLY_FIXED_SIZE, LinkError, LinkHeader, LinkMessage, LinkReply, MAX_LINK_BODY_SIZE,
    SPICE_MAGIC, SPICE_TICKET_PUBLIC_KEY_SIZE, SPICE_VERSION_MAJOR, SPICE_VERSION_MINOR,
    common_capability, display_capability, main_capability, playback_capability, record_capability,
    spicevmc_capability,
};
pub use main_channel::{
    ChannelId, ChannelsList, MAX_MIGRATION_CERT_SUBJECT_BYTES, MAX_MIGRATION_HOST_BYTES, MainInit,
    MigrationBegin, MigrationDestination, MouseMode, MouseModeState, decode_agent_u32,
    decode_main_name, decode_main_uuid, encode_agent_tokens, encode_mouse_mode_request,
    main_client, main_server,
};
pub use playback::{
    AudioDataMode, AudioSampleFormat, MAX_PLAYBACK_CHANNELS, MAX_PLAYBACK_PACKET_BYTES,
    MAX_PLAYBACK_SAMPLE_RATE_HZ, PlaybackMode, PlaybackPacket, PlaybackStart, decode_audio_mute,
    decode_audio_volume, decode_playback_latency, playback_server,
};
pub use port::{
    MAX_PORT_DATA_BYTES, MAX_PORT_NAME_BYTES, PortEvent, PortInit, decode_port_data,
    decode_port_event, encode_port_event, port_client, port_server,
};
pub use record::{
    MAX_RECORD_PACKET_BYTES, RecordStart, encode_record_mode, encode_record_packet,
    encode_record_start_mark, record_client, record_server,
};
pub use smartcard::{
    MAX_SMARTCARD_DATA_BYTES, SMARTCARD_UNDEFINED_READER_ID, SmartcardMessage,
    SmartcardMessageType, encode_smartcard_message, smartcard_client, smartcard_server,
};
pub use spicevmc::{
    SPICEVMC_COMPRESSION_LZ4, SpiceVmcCompressedData, encode_spicevmc_compressed_data,
};
pub use usbredir::{
    MAX_USBREDIR_CAPABILITY_WORDS, MAX_USBREDIR_PACKET_BYTES, USBREDIR_HEADER32_BYTES,
    USBREDIR_HEADER64_BYTES, USBREDIR_PACKET_HELLO, USBREDIR_VERSION_BYTES, UsbRedirCapabilities,
    UsbRedirHello, UsbRedirPacket, UsbRedirStreamDecoder, encode_usbredir_hello,
    encode_usbredir_packet, usbredir_capability,
};
