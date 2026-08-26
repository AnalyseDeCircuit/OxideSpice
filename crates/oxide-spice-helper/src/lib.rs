//! Host-side integrations kept outside the reusable SPICE client stack.

mod event_writer;
pub mod ipc;
mod runtime;
#[cfg(feature = "smartcard")]
mod smartcard;
mod stdio;
#[cfg(feature = "usbredir")]
mod usbredir;
#[cfg(feature = "webdav")]
mod webdav;

pub use runtime::HelperRuntimeError;
#[cfg(feature = "smartcard")]
pub use smartcard::{
    PcscReader, SmartcardRedirectionError, list_pcsc_readers, run_smartcard_redirection,
};
pub use stdio::{HelperProcessError, run_stdio};
#[cfg(feature = "usbredir")]
pub use usbredir::{UsbDeviceIdentity, UsbRedirectionError, list_usb_devices, run_usb_redirection};
#[cfg(feature = "webdav")]
pub use webdav::{WebDavConfig, WebDavError, run_webdav};
