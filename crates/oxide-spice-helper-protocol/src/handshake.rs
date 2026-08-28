//! Version and compiled-capability negotiation performed before credentials are accepted.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// First published IPC protocol revision.
pub const HELPER_IPC_PROTOCOL_VERSION: u32 = 1;

/// Maximum number of capabilities accepted in one Hello request.
const MAX_REQUIRED_CAPABILITIES: usize = 64;

/// A helper behavior or native backend that a host may require before sending credentials.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperCapability {
    CoreSession,
    Tls,
    SaslPassword,
    SaslGssapi,
    DisplayCanvas,
    CompositePixman,
    AudioRaw,
    AudioOpus,
    VideoMjpeg,
    VideoVp8,
    VideoVp9,
    VideoH264,
    VideoH265,
    Clipboard,
    FileTransfer,
    WebDav,
    UsbRedir,
    Smartcard,
    MultiDisplay,
    Playback,
    Record,
    Port,
}

/// Capabilities required for an official full helper artifact.
pub const FULL_HELPER_CAPABILITIES: &[HelperCapability] = &[
    HelperCapability::CoreSession,
    HelperCapability::Tls,
    HelperCapability::SaslPassword,
    HelperCapability::SaslGssapi,
    HelperCapability::DisplayCanvas,
    HelperCapability::CompositePixman,
    HelperCapability::AudioRaw,
    HelperCapability::AudioOpus,
    HelperCapability::VideoMjpeg,
    HelperCapability::VideoVp8,
    HelperCapability::VideoVp9,
    HelperCapability::VideoH264,
    HelperCapability::VideoH265,
    HelperCapability::Clipboard,
    HelperCapability::FileTransfer,
    HelperCapability::WebDav,
    HelperCapability::UsbRedir,
    HelperCapability::Smartcard,
    HelperCapability::MultiDisplay,
    HelperCapability::Playback,
    HelperCapability::Record,
    HelperCapability::Port,
];

/// Host declaration that must be the first IPC request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperHello {
    pub protocol_version: u32,
    pub required_capabilities: Vec<HelperCapability>,
}

impl HelperHello {
    pub fn current(required_capabilities: Vec<HelperCapability>) -> Self {
        Self {
            protocol_version: HELPER_IPC_PROTOCOL_VERSION,
            required_capabilities,
        }
    }
}

/// Build identity supplied by the helper executable to the handshake validator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelperBuildInfo {
    pub helper_version: String,
    pub target: String,
    pub capabilities: Vec<HelperCapability>,
}

/// Machine-readable identity embedded in every packaged helper directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperMetadata {
    pub helper_version: String,
    pub ipc_protocol_version: u32,
    pub target: String,
    pub capabilities: Vec<HelperCapability>,
    pub minimum_system_version: String,
    pub dynamic_libraries: Vec<String>,
}

impl HelperMetadata {
    pub fn from_build(
        build: HelperBuildInfo,
        minimum_system_version: String,
        mut dynamic_libraries: Vec<String>,
    ) -> Self {
        dynamic_libraries.sort_unstable();
        dynamic_libraries.dedup();
        Self {
            helper_version: build.helper_version,
            ipc_protocol_version: HELPER_IPC_PROTOCOL_VERSION,
            target: build.target,
            capabilities: build.capabilities,
            minimum_system_version,
            dynamic_libraries,
        }
    }
}

/// A deterministic reason why the helper refused to enter credential-bearing IPC state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HelperHandshakeRejection {
    UnsupportedProtocolVersion { requested: u32, supported: u32 },
    InvalidCapabilityList,
    MissingCapabilities { capabilities: Vec<HelperCapability> },
}

/// Helper response written before the request reader accepts Connect or any secret-bearing body.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperHelloAck {
    pub protocol_version: u32,
    pub helper_version: String,
    pub target: String,
    pub capabilities: Vec<HelperCapability>,
    pub compatible: bool,
    pub rejection: Option<HelperHandshakeRejection>,
}

/// Host-side validation failure for a received Hello acknowledgement.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HelperHelloAckError {
    #[error("helper rejected the requested IPC contract: {0:?}")]
    Rejected(HelperHandshakeRejection),
    #[error("helper returned an inconsistent Hello acknowledgement")]
    InconsistentAcknowledgement,
    #[error("helper acknowledged IPC version {actual}, expected {expected}")]
    ProtocolVersionMismatch { expected: u32, actual: u32 },
    #[error("helper acknowledgement contains an invalid capability list")]
    InvalidCapabilityList,
    #[error("helper acknowledgement is missing required capabilities: {0:?}")]
    MissingCapabilities(Vec<HelperCapability>),
}

impl HelperHelloAck {
    pub fn require_compatible(&self) -> Result<(), HelperHelloAckError> {
        match (&self.rejection, self.compatible) {
            (None, true) => Ok(()),
            (Some(rejection), false) => Err(HelperHelloAckError::Rejected(rejection.clone())),
            _ => Err(HelperHelloAckError::InconsistentAcknowledgement),
        }
    }

    /// Validates a received acknowledgement against the exact Hello sent by the host.
    pub fn validate_for(&self, hello: &HelperHello) -> Result<(), HelperHelloAckError> {
        self.require_compatible()?;
        if self.protocol_version != hello.protocol_version {
            return Err(HelperHelloAckError::ProtocolVersionMismatch {
                expected: hello.protocol_version,
                actual: self.protocol_version,
            });
        }
        if self.helper_version.is_empty()
            || self.target.is_empty()
            || self.capabilities.len() > MAX_REQUIRED_CAPABILITIES
        {
            return Err(HelperHelloAckError::InconsistentAcknowledgement);
        }
        let compiled: BTreeSet<_> = self.capabilities.iter().copied().collect();
        if compiled.len() != self.capabilities.len() {
            return Err(HelperHelloAckError::InvalidCapabilityList);
        }
        let required: BTreeSet<_> = hello.required_capabilities.iter().copied().collect();
        if hello.required_capabilities.len() > MAX_REQUIRED_CAPABILITIES
            || required.len() != hello.required_capabilities.len()
        {
            return Err(HelperHelloAckError::InvalidCapabilityList);
        }
        let missing: Vec<_> = required.difference(&compiled).copied().collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(HelperHelloAckError::MissingCapabilities(missing))
        }
    }
}

/// Stateless server-side negotiation over immutable build information.
#[derive(Clone, Debug)]
pub struct ServerHandshake {
    build: HelperBuildInfo,
}

impl ServerHandshake {
    pub fn new(mut build: HelperBuildInfo) -> Self {
        build.capabilities.sort_unstable();
        build.capabilities.dedup();
        Self { build }
    }

    pub fn negotiate(&self, hello: HelperHello) -> HelperHelloAck {
        let rejection = self.rejection_for(&hello);
        HelperHelloAck {
            protocol_version: HELPER_IPC_PROTOCOL_VERSION,
            helper_version: self.build.helper_version.clone(),
            target: self.build.target.clone(),
            capabilities: self.build.capabilities.clone(),
            compatible: rejection.is_none(),
            rejection,
        }
    }

    fn rejection_for(&self, hello: &HelperHello) -> Option<HelperHandshakeRejection> {
        if hello.protocol_version != HELPER_IPC_PROTOCOL_VERSION {
            return Some(HelperHandshakeRejection::UnsupportedProtocolVersion {
                requested: hello.protocol_version,
                supported: HELPER_IPC_PROTOCOL_VERSION,
            });
        }
        if hello.required_capabilities.len() > MAX_REQUIRED_CAPABILITIES {
            return Some(HelperHandshakeRejection::InvalidCapabilityList);
        }
        let required: BTreeSet<_> = hello.required_capabilities.iter().copied().collect();
        if required.len() != hello.required_capabilities.len() {
            return Some(HelperHandshakeRejection::InvalidCapabilityList);
        }
        let compiled: BTreeSet<_> = self.build.capabilities.iter().copied().collect();
        let missing: Vec<_> = required.difference(&compiled).copied().collect();
        (!missing.is_empty()).then_some(HelperHandshakeRejection::MissingCapabilities {
            capabilities: missing,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_build() -> HelperBuildInfo {
        HelperBuildInfo {
            helper_version: "0.1.0".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            capabilities: FULL_HELPER_CAPABILITIES.to_vec(),
        }
    }

    #[test]
    fn full_build_accepts_the_complete_delivery_contract() {
        let acknowledgement = ServerHandshake::new(full_build())
            .negotiate(HelperHello::current(FULL_HELPER_CAPABILITIES.to_vec()));
        assert!(acknowledgement.require_compatible().is_ok());
        assert_eq!(acknowledgement.capabilities, FULL_HELPER_CAPABILITIES);
    }

    #[test]
    fn incompatible_version_is_rejected_before_session_state() {
        let acknowledgement = ServerHandshake::new(full_build()).negotiate(HelperHello {
            protocol_version: HELPER_IPC_PROTOCOL_VERSION + 1,
            required_capabilities: Vec::new(),
        });
        assert!(matches!(
            acknowledgement.require_compatible(),
            Err(HelperHelloAckError::Rejected(
                HelperHandshakeRejection::UnsupportedProtocolVersion { .. }
            ))
        ));
    }

    #[test]
    fn missing_or_duplicate_capabilities_are_rejected() {
        let mut build = full_build();
        build
            .capabilities
            .retain(|capability| *capability != HelperCapability::UsbRedir);
        let acknowledgement = ServerHandshake::new(build)
            .negotiate(HelperHello::current(vec![HelperCapability::UsbRedir]));
        assert_eq!(
            acknowledgement.rejection,
            Some(HelperHandshakeRejection::MissingCapabilities {
                capabilities: vec![HelperCapability::UsbRedir],
            })
        );

        let duplicate = ServerHandshake::new(full_build()).negotiate(HelperHello::current(vec![
            HelperCapability::Tls,
            HelperCapability::Tls,
        ]));
        assert_eq!(
            duplicate.rejection,
            Some(HelperHandshakeRejection::InvalidCapabilityList)
        );
    }

    #[test]
    fn inconsistent_acknowledgement_is_rejected_without_panicking() {
        let mut acknowledgement =
            ServerHandshake::new(full_build()).negotiate(HelperHello::current(Vec::new()));
        acknowledgement.compatible = false;
        assert_eq!(
            acknowledgement.require_compatible(),
            Err(HelperHelloAckError::InconsistentAcknowledgement)
        );
    }

    #[test]
    fn host_validation_checks_version_identity_and_required_capabilities() {
        let hello = HelperHello::current(vec![HelperCapability::Tls]);
        let mut acknowledgement = ServerHandshake::new(full_build()).negotiate(hello.clone());
        assert!(acknowledgement.validate_for(&hello).is_ok());

        acknowledgement
            .capabilities
            .retain(|capability| *capability != HelperCapability::Tls);
        assert_eq!(
            acknowledgement.validate_for(&hello),
            Err(HelperHelloAckError::MissingCapabilities(vec![
                HelperCapability::Tls
            ]))
        );
    }
}
