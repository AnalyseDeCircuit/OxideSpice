//! Host-side integrations kept outside the reusable SPICE client stack.

mod event_writer;
pub mod ipc;
mod runtime;
mod smartcard;
mod stdio;
mod usbredir;
mod webdav;

pub use runtime::HelperRuntimeError;
pub use smartcard::{
    PcscReader, SmartcardRedirectionError, list_pcsc_readers, run_smartcard_redirection,
};
pub use stdio::{HelperProcessError, run_stdio};
pub use usbredir::{UsbDeviceIdentity, UsbRedirectionError, list_usb_devices, run_usb_redirection};
pub use webdav::{WebDavConfig, WebDavError, run_webdav};
