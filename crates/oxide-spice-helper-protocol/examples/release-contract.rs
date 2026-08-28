//! Prints the release contract from the same constants used by helper IPC negotiation.

use oxide_spice_helper_protocol::{FULL_HELPER_CAPABILITIES, HELPER_IPC_PROTOCOL_VERSION};
use serde_json::json;

fn main() {
    let contract = json!({
        "helperVersion": env!("CARGO_PKG_VERSION"),
        "ipcProtocolVersion": HELPER_IPC_PROTOCOL_VERSION,
        "capabilities": FULL_HELPER_CAPABILITIES,
    });
    println!("{contract}");
}
