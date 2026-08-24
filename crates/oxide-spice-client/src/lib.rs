//! Asynchronous SPICE client ownership and channel I/O.

mod agent;
mod auth;
mod channel;
mod cursor;
mod display;
mod error;
mod inputs;
mod playback;
mod port;
mod record;
mod sasl;
mod session;
mod smartcard;
#[cfg(unix)]
mod unix_stream;
mod usbredir;

pub use cursor::{CursorEvents, CursorShape, CursorState};
pub use display::{
    DisplayTopology, DisplayTopologyEvents, FrameEvent, FrameSnapshot, PixelFormat, SurfaceHandle,
};
#[cfg(unix)]
pub use display::{DmaBufPlane, DmaBufScanout, GlFrame, GlFrameEvents};
pub use error::{ClientError, ErrorCategory};
pub use inputs::{InputSendError, InputsHandle, PointerPosition};
pub use oxide_spice_protocol::{
    AgentClipboardSelection, KeyboardModifiers, MonitorHead, MouseButton, MouseButtons, MouseMode,
    SMARTCARD_UNDEFINED_READER_ID, SmartcardMessageType,
};
pub use playback::{
    PlaybackAudioSettings, PlaybackChannel, PlaybackFormat, PlaybackPackets, PlaybackPcmPacket,
    PlaybackState,
};
pub use port::{PortChannel, PortInbound, PortSendError, PortState};
pub use record::{RecordAudioSettings, RecordChannel, RecordSendError, RecordState};
pub use sasl::{SaslCredentials, SaslOptions};
pub use session::{
    ConnectOptions, ConnectionEndpoint, ServerIdentity, Session, SessionState, TicketSecret,
    TransportSecurity,
};
#[cfg(feature = "tls-ring")]
pub use session::{MigrationTlsConfiguration, MigrationTlsPolicy};
pub use smartcard::{SmartcardChannel, SmartcardInbound, SmartcardSendError, SmartcardState};
pub use usbredir::{UsbRedirChannel, UsbRedirInbound, UsbRedirSendError, UsbRedirState};

pub use agent::{
    AgentAudioVolumeState, AgentEvent, AgentEvents, AgentFeatures, AgentGraphicsDeviceState,
    AgentHandle, AgentSendError, AgentState, ClipboardOffer, ClipboardRequest,
    FileTransferMetadata, FileTransferState, GuestMonitor, GuestMonitorLayout,
    OutgoingFileTransfer,
};
/// Rustls types used to build caller-owned TLS identity policy for the `tls-ring` feature.
#[cfg(feature = "tls-ring")]
pub use tokio_rustls::rustls as tls;
