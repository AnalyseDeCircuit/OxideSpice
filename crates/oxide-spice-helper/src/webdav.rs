//! Host-owned WebDAV filesystem service over one SPICE WebDAV byte stream.

use std::convert::Infallible;
use std::path::PathBuf;

use dav_server::{DavHandler, DavMethodSet, fakels::FakeLs, localfs::LocalFs};
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use oxide_spice_client::{PortChannel, PortInbound, PortSendError, PortState};
use oxide_spice_protocol::{ChannelType, MAX_PORT_DATA_BYTES};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

/// Explicit host filesystem authority granted to one guest WebDAV channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebDavConfig {
    pub root: PathBuf,
    pub read_only: bool,
}

/// WebDAV service, filesystem, or SPICE transport failure.
#[derive(Debug, thiserror::Error)]
pub enum WebDavError {
    #[error("SPICE channel is not a WebDAV channel")]
    WrongChannelType,
    #[error("WebDAV root is not an accessible directory")]
    InvalidRoot,
    #[error("SPICE WebDAV transport failed: {0}")]
    Transport(#[from] PortSendError),
    #[error("WebDAV HTTP connection failed: {0}")]
    Http(String),
    #[error("WebDAV byte bridge failed: {0}")]
    Bridge(#[from] std::io::Error),
}

/// Serves a caller-authorized directory until the WebDAV port or HTTP connection closes.
pub async fn run_webdav(mut channel: PortChannel, config: WebDavConfig) -> Result<(), WebDavError> {
    if channel.channel_type() != ChannelType::WebDav {
        return Err(WebDavError::WrongChannelType);
    }
    let metadata = std::fs::metadata(&config.root).map_err(|_| WebDavError::InvalidRoot)?;
    if !metadata.is_dir() {
        return Err(WebDavError::InvalidRoot);
    }
    wait_until_open(&mut channel).await?;

    let methods = if config.read_only {
        DavMethodSet::WEBDAV_RO
    } else {
        DavMethodSet::WEBDAV_RW
    };
    let handler = DavHandler::builder()
        .filesystem(LocalFs::new(
            config.root,
            false,
            false,
            cfg!(target_os = "macos"),
        ))
        .locksystem(FakeLs::new())
        .methods(methods)
        .build_handler();

    let (http_stream, bridge_stream) = tokio::io::duplex(MAX_PORT_DATA_BYTES);
    let http = http1::Builder::new().serve_connection(
        TokioIo::new(http_stream),
        service_fn(move |request| {
            let handler = handler.clone();
            async move { Ok::<_, Infallible>(handler.handle(request).await) }
        }),
    );
    tokio::pin!(http);
    tokio::select! {
        result = bridge_port(channel, bridge_stream) => result,
        result = &mut http => result.map_err(|error| WebDavError::Http(error.to_string())),
    }
}

async fn wait_until_open(channel: &mut PortChannel) -> Result<(), WebDavError> {
    loop {
        match channel.state() {
            PortState::Ready { opened: true, .. } => return Ok(()),
            PortState::Closed { .. } => return Err(PortSendError::Closed.into()),
            PortState::AwaitingInit { .. } | PortState::Ready { opened: false, .. } => {
                channel.changed().await?;
            }
        }
    }
}

async fn bridge_port(
    mut channel: PortChannel,
    mut stream: DuplexStream,
) -> Result<(), WebDavError> {
    let mut outbound = vec![0; MAX_PORT_DATA_BYTES];
    loop {
        tokio::select! {
            inbound = channel.next() => match inbound? {
                PortInbound::Data { bytes, discontinuity: false } => {
                    stream.write_all(&bytes).await?;
                }
                PortInbound::Data { discontinuity: true, .. } | PortInbound::Break => {
                    stream.shutdown().await?;
                    return Ok(());
                }
            },
            read = stream.read(&mut outbound) => {
                let byte_count = read?;
                if byte_count == 0 {
                    return Ok(());
                }
                channel.write(&outbound[..byte_count]).await?;
            }
        }
    }
}
