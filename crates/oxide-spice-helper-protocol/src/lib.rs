//! Versioned, bounded IPC shared by the OxideSpice helper and its host process.

mod handshake;
mod wire;

pub use handshake::{
    FULL_HELPER_CAPABILITIES, HELPER_IPC_PROTOCOL_VERSION, HelperBuildInfo, HelperCapability,
    HelperHandshakeRejection, HelperHello, HelperHelloAck, HelperHelloAckError, HelperMetadata,
    ServerHandshake,
};
pub use wire::*;
