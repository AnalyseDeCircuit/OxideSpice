//! Compile-time helper identity and capability reporting.

#[cfg(test)]
use oxide_spice_helper_protocol::FULL_HELPER_CAPABILITIES;
use oxide_spice_helper_protocol::{HelperBuildInfo, HelperCapability, HelperMetadata};

pub(crate) fn helper_build_info() -> HelperBuildInfo {
    let mut capabilities = vec![
        HelperCapability::CoreSession,
        HelperCapability::SaslPassword,
        HelperCapability::DisplayCanvas,
        HelperCapability::AudioRaw,
        HelperCapability::VideoMjpeg,
        HelperCapability::Clipboard,
        HelperCapability::FileTransfer,
        HelperCapability::MultiDisplay,
        HelperCapability::Playback,
        HelperCapability::Record,
        HelperCapability::Port,
    ];
    if cfg!(feature = "tls-ring") {
        capabilities.push(HelperCapability::Tls);
    }
    if cfg!(feature = "sasl-gssapi") {
        capabilities.push(HelperCapability::SaslGssapi);
    }
    if cfg!(feature = "composite-pixman") {
        capabilities.push(HelperCapability::CompositePixman);
    }
    if cfg!(feature = "audio-opus") {
        capabilities.push(HelperCapability::AudioOpus);
    }
    if cfg!(feature = "video-vpx") {
        capabilities.push(HelperCapability::VideoVp8);
        capabilities.push(HelperCapability::VideoVp9);
    }
    if cfg!(feature = "video-h264") {
        capabilities.push(HelperCapability::VideoH264);
    }
    if cfg!(feature = "video-h265") {
        capabilities.push(HelperCapability::VideoH265);
    }
    if cfg!(feature = "webdav") {
        capabilities.push(HelperCapability::WebDav);
    }
    if cfg!(feature = "usbredir") {
        capabilities.push(HelperCapability::UsbRedir);
    }
    if cfg!(feature = "smartcard") {
        capabilities.push(HelperCapability::Smartcard);
    }
    capabilities.sort_unstable();
    HelperBuildInfo {
        helper_version: env!("CARGO_PKG_VERSION").to_owned(),
        target: env!("OXIDE_SPICE_BUILD_TARGET").to_owned(),
        capabilities,
    }
}

pub(crate) fn helper_metadata() -> HelperMetadata {
    let dynamic_libraries = env!("OXIDE_SPICE_DYNAMIC_LIBRARIES")
        .split(',')
        .filter(|library| !library.is_empty())
        .map(str::to_owned)
        .collect();
    HelperMetadata::from_build(
        helper_build_info(),
        env!("OXIDE_SPICE_MINIMUM_SYSTEM_VERSION").to_owned(),
        dynamic_libraries,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_helper_build_contains_the_full_delivery_contract() {
        let build = helper_build_info();
        for required in FULL_HELPER_CAPABILITIES {
            assert!(
                build.capabilities.contains(required),
                "default helper build is missing {required:?}"
            );
        }
    }
}
