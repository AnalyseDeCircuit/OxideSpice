//! Host-side integrations kept outside the reusable SPICE client stack.

mod build_info;
mod event_writer;
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
    PcscReader, SmartcardRedirectionError, check_pcsc_client_library, list_pcsc_readers,
    run_smartcard_redirection,
};
pub use stdio::{HelperProcessError, run_stdio};
#[cfg(feature = "usbredir")]
pub use usbredir::{UsbDeviceIdentity, UsbRedirectionError, list_usb_devices, run_usb_redirection};
#[cfg(feature = "webdav")]
pub use webdav::{WebDavConfig, WebDavError, run_webdav};

/// Returns the metadata written into a packaged helper directory.
pub fn build_metadata() -> oxide_spice_helper_protocol::HelperMetadata {
    build_info::helper_metadata()
}
