//! Display surface ownership and bounded frame notifications.
mod canvas;

use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oxide_spice_codecs::{
    DecodeLimits, DecodedGlzImage, DecodedImage, DecodedJpeg, DecodedPixels, DecodedVideoFrame,
    GlzErrorKind, LzImageType, SpiceVideoCodec, SpiceVideoDecoder, decode_glz_with_cancel,
    decode_jpeg_with_cancel, decode_lz_with_cancel, decode_lz4_with_cancel,
    decode_quic_with_cancel, inflate_zlib_exact_with_cancel,
};
use oxide_spice_protocol::{
    BitmapFormat, BitmapPalette, CompositeClip, CompositeImage, CompressedImageUpdate, CopyBits,
    DisplayBrush, DisplayInit, DisplayMask, DrawAlphaBlend, DrawCopy as ClassicDrawCopy,
    DrawCopyImageType, DrawFill as ClassicDrawFill, DrawMaskedDestination, DrawOpaque, DrawRop3,
    DrawStroke, DrawText, DrawTransparent, EmbeddedImage, ImageCompression, InvalidateList,
    MonitorHead, MonitorsConfig, Rect, StreamClip, StreamClipUpdate, StreamCreate, StreamData,
    StreamReport, StreamReportActivation, SurfaceCreate, SurfaceFormat, VideoCodec,
    WaitForChannels, common_server, display_capability, display_client, display_server,
    encode_preferred_video_codecs,
};
#[cfg(feature = "composite-pixman")]
use oxide_spice_protocol::{CompositeTransform, DrawComposite};
#[cfg(unix)]
use oxide_spice_protocol::{GlDraw, GlScanout2Unix, GlScanoutUnix};
#[cfg(feature = "composite-pixman")]
use pixman::{
    Box32 as PixmanBox32, Filter as PixmanFilter, Fixed as PixmanFixed, FormatCode as PixmanFormat,
    Image as PixmanImage, Operation as PixmanOperation, Region32 as PixmanRegion32,
    Repeat as PixmanRepeat, Transform as PixmanTransform,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{RwLock, Semaphore, mpsc, oneshot, watch, watch::error::RecvError};

use crate::ClientError;
use crate::channel::{
    Channel, ChannelIdentity, ControlDisposition, ControlState, IncomingEnvelope, ProgressRegistry,
    handle_channel_wait,
};

/// A bounded default for all live surface backing stores in one Display task.
const DEFAULT_MAX_SURFACE_BYTES: usize = 256 * 1024 * 1024;
/// Maximum number of full image decodes that may allocate concurrently in one session.
const DEFAULT_MAX_CONCURRENT_IMAGE_DECODES: usize = 1;
/// Retained cross-image GLZ dictionary bytes advertised to the server.
const DEFAULT_GLZ_DICTIONARY_BYTES: usize = 16 * 1024 * 1024;
const GLZ_DICTIONARY_ID: u8 = 1;
/// Surface Create bit that identifies a guest-visible primary surface.
const SURFACE_FLAG_PRIMARY: u32 = 1 << 0;
/// Maximum number of palettes retained by one Display channel.
const MAX_PALETTE_CACHE_ENTRIES: usize = 256;
/// Maximum decoded palette bytes retained by one Display channel.
const MAX_PALETTE_CACHE_BYTES: usize = 256 * 1024;
/// Maximum independently decoded video streams retained by one Display channel.
const MAX_ACTIVE_STREAMS: usize = 16;
/// Builds the preference order from codecs that are present in this binary.
fn preferred_video_codecs() -> Vec<VideoCodec> {
    let mut codecs = Vec::with_capacity(5);
    #[cfg(feature = "video-h264")]
    codecs.push(VideoCodec::H264);
    #[cfg(feature = "video-vpx")]
    codecs.push(VideoCodec::Vp9);
    #[cfg(feature = "video-h265")]
    codecs.push(VideoCodec::H265);
    #[cfg(feature = "video-vpx")]
    codecs.push(VideoCodec::Vp8);
    codecs.push(VideoCodec::Mjpeg);
    codecs
}

/// Host-facing pixel layout stored by the first Display implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Four bytes per pixel in red, green, blue, alpha order.
    Rgba8,
}

/// An explicit host snapshot taken outside the network task's message loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSnapshot {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub pixels: Vec<u8>,
}

/// Shared snapshot access to one surface while its Display task remains the only writer.
#[derive(Clone)]
pub struct SurfaceHandle {
    display_channel_id: u8,
    surface_id: u32,
    width: u32,
    height: u32,
    is_primary: bool,
    inner: Arc<RwLock<SurfaceData>>,
}

impl std::fmt::Debug for SurfaceHandle {
    /// Omits frame bytes from debug output while retaining useful identity metadata.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SurfaceHandle")
            .field("display_channel_id", &self.display_channel_id)
            .field("surface_id", &self.surface_id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("is_primary", &self.is_primary)
            .finish_non_exhaustive()
    }
}

impl SurfaceHandle {
    /// Identifies the independently linked Display channel that owns this surface.
    pub const fn display_channel_id(&self) -> u8 {
        self.display_channel_id
    }

    /// Returns the surface identity within its Display channel.
    pub const fn surface_id(&self) -> u32 {
        self.surface_id
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub const fn is_primary(&self) -> bool {
        self.is_primary
    }

    /// Copies the current full surface under an asynchronous, bounded-duration read lock.
    pub async fn snapshot(&self) -> Result<FrameSnapshot, ClientError> {
        let surface = self.inner.read().await;
        Ok(FrameSnapshot {
            width: self.width,
            height: self.height,
            format: PixelFormat::Rgba8,
            pixels: surface.pixel_bytes().to_vec(),
        })
    }

    /// Copies only a checked region so helper delivery does not require a full-frame copy.
    pub async fn snapshot_region(&self, region: Rect) -> Result<FrameSnapshot, ClientError> {
        let surface = self.inner.read().await;
        let bounds = surface.rect_bounds(region)?;
        let row_bytes = bounds
            .width
            .checked_mul(4)
            .ok_or_else(|| resource_limit_error("snapshot row bytes"))?;
        let output_bytes = row_bytes
            .checked_mul(bounds.height)
            .ok_or_else(|| resource_limit_error("snapshot region bytes"))?;
        let mut pixels = Vec::with_capacity(output_bytes);
        for row in bounds.y..bounds.y + bounds.height {
            let start = surface.pixel_offset(bounds.x, row)?;
            pixels.extend_from_slice(&surface.pixel_bytes()[start..start + row_bytes]);
        }
        Ok(FrameSnapshot {
            width: u32::try_from(bounds.width)
                .map_err(|_| resource_limit_error("snapshot width"))?,
            height: u32::try_from(bounds.height)
                .map_err(|_| resource_limit_error("snapshot height"))?,
            format: PixelFormat::Rgba8,
            pixels,
        })
    }
}

/// Coalescible notification that a shared surface contains newer pixels.
#[derive(Debug, Clone)]
pub struct FrameEvent {
    pub connection_generation: u64,
    pub graphics_epoch: u64,
    pub display_channel_id: u8,
    pub surface_id: u32,
    pub dirty: Rect,
    /// A latest-only notification may replace earlier dirty regions before consumption.
    pub full_refresh_required: bool,
    pub surface: SurfaceHandle,
}

/// One DMA-BUF plane whose descriptor remains owned until all frame references are released.
#[cfg(unix)]
pub struct DmaBufPlane {
    file_descriptor: Arc<OwnedFd>,
    pub offset: u32,
    pub stride: u32,
}

#[cfg(unix)]
impl std::fmt::Debug for DmaBufPlane {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DmaBufPlane")
            .field("offset", &self.offset)
            .field("stride", &self.stride)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl DmaBufPlane {
    pub fn file_descriptor(&self) -> BorrowedFd<'_> {
        self.file_descriptor.as_fd()
    }
}

/// Immutable DMA-BUF layout replacing the previous scanout on one Display channel.
#[cfg(unix)]
#[derive(Debug)]
pub struct DmaBufScanout {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub top_down: bool,
    pub planes: Arc<[DmaBufPlane]>,
}

/// One GL dirty region that must be acknowledged after the host finishes importing it.
#[cfg(unix)]
pub struct GlFrame {
    pub connection_generation: u64,
    pub display_channel_id: u8,
    pub dirty: GlDraw,
    pub scanout: Arc<DmaBufScanout>,
    completion: Option<oneshot::Sender<()>>,
}

#[cfg(unix)]
impl std::fmt::Debug for GlFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GlFrame")
            .field("connection_generation", &self.connection_generation)
            .field("display_channel_id", &self.display_channel_id)
            .field("dirty", &self.dirty)
            .field("scanout", &self.scanout)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl GlFrame {
    /// Signals that the host has consumed the dirty scanout region.
    pub fn complete(mut self) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(());
        }
    }
}

#[cfg(unix)]
impl Drop for GlFrame {
    /// Dropping a frame counts as a host-side drop and still releases the server pipeline.
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(());
        }
    }
}

/// Bounded receiver for GL frames across all Display channel ids.
#[cfg(unix)]
pub struct GlFrameEvents {
    receiver: mpsc::Receiver<GlFrame>,
}

#[cfg(unix)]
impl GlFrameEvents {
    pub async fn next(&mut self) -> Result<GlFrame, ClientError> {
        self.receiver
            .recv()
            .await
            .ok_or(ClientError::TaskTerminated)
    }
}

#[cfg(unix)]
pub(crate) fn gl_frame_events() -> (mpsc::Sender<GlFrame>, GlFrameEvents) {
    let (sender, receiver) = mpsc::channel(1);
    (sender, GlFrameEvents { receiver })
}

/// Complete monitor topology attached to one Display channel and graphics epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayTopology {
    pub connection_generation: u64,
    pub graphics_epoch: u64,
    pub display_channel_id: u8,
    pub maximum_allowed: u16,
    pub monitors: Arc<[MonitorHead]>,
}

/// Cloneable latest-only topology stream independent from frame consumption.
#[derive(Clone)]
pub struct DisplayTopologyEvents {
    receiver: watch::Receiver<Option<DisplayTopology>>,
}

impl std::fmt::Debug for DisplayTopologyEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DisplayTopologyEvents")
            .field("latest", &*self.receiver.borrow())
            .finish_non_exhaustive()
    }
}

impl DisplayTopologyEvents {
    /// Returns the latest topology after the server has sent Monitors Config.
    pub fn latest(&self) -> Option<DisplayTopology> {
        self.receiver.borrow().clone()
    }

    /// Waits for the next complete topology while intermediate replacements may be coalesced.
    pub async fn next(&mut self) -> Result<DisplayTopology, ClientError> {
        loop {
            self.receiver
                .changed()
                .await
                .map_err(|_| ClientError::TaskTerminated)?;
            if let Some(topology) = self.receiver.borrow_and_update().clone() {
                return Ok(topology);
            }
        }
    }
}

/// Creates the public topology stream and task-owned sender.
pub(crate) fn topology_events() -> (
    watch::Sender<Option<DisplayTopology>>,
    DisplayTopologyEvents,
) {
    let (sender, receiver) = watch::channel(None);
    (sender, DisplayTopologyEvents { receiver })
}

/// Receives latest-only frame notifications while the surface retains all applied updates.
pub(crate) type FrameReceiver = watch::Receiver<Option<FrameEvent>>;

/// Waits for the next surface change without allocating or copying frame pixels.
pub(crate) async fn next_frame(receiver: &mut FrameReceiver) -> Result<FrameEvent, ClientError> {
    loop {
        receiver
            .changed()
            .await
            .map_err(|_: RecvError| ClientError::TaskTerminated)?;
        if let Some(event) = receiver.borrow_and_update().clone() {
            return Ok(event);
        }
    }
}

/// Mutable backing storage owned by the Display socket task.
struct SurfaceData {
    width: u32,
    height: u32,
    format: SurfaceFormat,
    pixels: Vec<u32>,
    budget: Arc<SurfaceBudget>,
    allocated_bytes: usize,
}

struct StreamReportState {
    unique_id: u32,
    maximum_window_frames: u32,
    timeout: Duration,
    window_started: Instant,
    start_frame_multimedia_time: Option<u32>,
    end_frame_multimedia_time: u32,
    frame_count: u32,
}

struct StreamRuntime {
    create: StreamCreate,
    decoder: VideoDecoderWorker,
    report: Option<StreamReportState>,
}

struct VideoDecodeCommand {
    packet: Arc<[u8]>,
    expected_width: u32,
    expected_height: u32,
    response:
        oneshot::Sender<Result<Option<DecodedVideoFrame>, oxide_spice_codecs::VideoDecodeError>>,
}

struct VideoDecoderWorker {
    commands: mpsc::Sender<VideoDecodeCommand>,
    cancelled: Arc<AtomicBool>,
}

impl VideoDecoderWorker {
    async fn start(codec: SpiceVideoCodec, width: u32, height: u32) -> Result<Self, ClientError> {
        let (commands, mut command_receiver) = mpsc::channel::<VideoDecodeCommand>(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let (initialized, initialization) = oneshot::channel();
        std::thread::Builder::new()
            .name("oxide-spice-video".to_owned())
            .spawn(move || {
                let mut decoder = match SpiceVideoDecoder::new(codec, width, height) {
                    Ok(decoder) => {
                        let _ =
                            initialized.send(Ok::<(), oxide_spice_codecs::VideoDecodeError>(()));
                        decoder
                    }
                    Err(error) => {
                        let _ = initialized.send(Err(error));
                        return;
                    }
                };
                while let Some(command) = command_receiver.blocking_recv() {
                    if worker_cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let decode_cancelled = worker_cancelled.clone();
                    let result = decoder.decode(
                        &command.packet,
                        command.expected_width,
                        command.expected_height,
                        DecodeLimits::DISPLAY,
                        move || decode_cancelled.load(Ordering::Acquire),
                    );
                    let _ = command.response.send(result);
                }
            })?;
        initialization
            .await
            .map_err(|_| ClientError::Internal("video decoder initialization thread exited"))??;
        Ok(Self {
            commands,
            cancelled,
        })
    }

    async fn decode(
        &self,
        packet: Arc<[u8]>,
        expected_width: u32,
        expected_height: u32,
    ) -> Result<Option<DecodedVideoFrame>, ClientError> {
        let (response, decoded) = oneshot::channel();
        self.commands
            .send(VideoDecodeCommand {
                packet,
                expected_width,
                expected_height,
                response,
            })
            .await
            .map_err(|_| ClientError::TaskTerminated)?;
        decoded
            .await
            .map_err(|_| ClientError::TaskTerminated)?
            .map_err(Into::into)
    }
}

impl Drop for VideoDecoderWorker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

struct PaletteCache {
    entries: HashMap<u64, Arc<[[u8; 4]]>>,
    retained_bytes: usize,
}

impl PaletteCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            retained_bytes: 0,
        }
    }

    fn resolve(
        &mut self,
        palette: Option<BitmapPalette<'_>>,
    ) -> Result<Option<Arc<[[u8; 4]]>>, ClientError> {
        match palette {
            None => Ok(None),
            Some(BitmapPalette::Cached { unique_id }) => self
                .entries
                .get(&unique_id)
                .cloned()
                .map(Some)
                .ok_or_else(|| protocol_value_error("palette cache miss")),
            Some(BitmapPalette::Inline {
                unique_id,
                cache_me,
                entries_bgrx,
            }) => {
                let mut entries = Vec::with_capacity(entries_bgrx.len() / 4);
                for bgrx in entries_bgrx.chunks_exact(4) {
                    entries.push([bgrx[2], bgrx[1], bgrx[0], u8::MAX]);
                }
                let entries: Arc<[[u8; 4]]> = entries.into();
                if cache_me {
                    self.insert(unique_id, entries.clone())?;
                }
                Ok(Some(entries))
            }
        }
    }

    fn insert(&mut self, unique_id: u64, entries: Arc<[[u8; 4]]>) -> Result<(), ClientError> {
        let existing_bytes = self
            .entries
            .get(&unique_id)
            .map_or(0, |existing| existing.len() * 4);
        let entry_bytes = entries
            .len()
            .checked_mul(4)
            .ok_or_else(|| resource_limit_error("palette bytes"))?;
        let updated_bytes = self
            .retained_bytes
            .checked_sub(existing_bytes)
            .and_then(|bytes| bytes.checked_add(entry_bytes))
            .ok_or_else(|| resource_limit_error("palette cache bytes"))?;
        if !self.entries.contains_key(&unique_id) && self.entries.len() >= MAX_PALETTE_CACHE_ENTRIES
            || updated_bytes > MAX_PALETTE_CACHE_BYTES
        {
            return Err(resource_limit_error("palette cache capacity"));
        }
        self.entries.insert(unique_id, entries);
        self.retained_bytes = updated_bytes;
        Ok(())
    }

    fn invalidate(&mut self, unique_id: u64) {
        if let Some(entries) = self.entries.remove(&unique_id) {
            self.retained_bytes = self.retained_bytes.saturating_sub(entries.len() * 4);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.retained_bytes = 0;
    }
}

/// Tracks bytes until the final surface handle releases the backing store.
pub(crate) struct SurfaceBudget {
    used: AtomicUsize,
    maximum: usize,
}

/// Creates one session-wide budget shared by every linked Display channel.
pub(crate) fn surface_budget() -> Arc<SurfaceBudget> {
    Arc::new(SurfaceBudget {
        used: AtomicUsize::new(0),
        maximum: DEFAULT_MAX_SURFACE_BYTES,
    })
}

/// Creates the session-wide gate that bounds transient decoded image storage.
pub(crate) fn image_decode_slots() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_IMAGE_DECODES))
}

struct GlzWindowState {
    images: HashMap<u64, Arc<[u8]>>,
    oldest_by_image: HashMap<u64, u64>,
    retained_bytes: usize,
    oldest_image_id: u64,
    next_contiguous_image_id: u64,
    last_cleared_migration: u64,
}

/// Session-wide GLZ dictionary shared by every Display transport.
pub(crate) struct GlzWindow {
    state: Mutex<GlzWindowState>,
    changed: watch::Sender<u64>,
}

pub(crate) fn glz_window() -> Arc<GlzWindow> {
    let (changed, _) = watch::channel(0);
    Arc::new(GlzWindow {
        state: Mutex::new(GlzWindowState {
            images: HashMap::new(),
            oldest_by_image: HashMap::new(),
            retained_bytes: 0,
            oldest_image_id: 0,
            next_contiguous_image_id: 0,
            last_cleared_migration: 0,
        }),
        changed,
    })
}

impl GlzWindow {
    /// Drops dictionary entries that belong to a previous non-seamless server instance.
    fn clear_for_migration(&self, migration_activation: u64) -> Result<(), ClientError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ClientError::Internal("GLZ window lock poisoned"))?;
        if state.last_cleared_migration >= migration_activation {
            return Ok(());
        }
        state.images.clear();
        state.oldest_by_image.clear();
        state.retained_bytes = 0;
        state.oldest_image_id = 0;
        state.next_contiguous_image_id = 0;
        state.last_cleared_migration = migration_activation;
        drop(state);
        self.changed.send_modify(|generation| *generation += 1);
        Ok(())
    }

    fn resolve(&self, image_id: u64) -> Option<Arc<[u8]>> {
        self.state.lock().ok()?.images.get(&image_id).cloned()
    }

    fn insert(&self, image: &DecodedGlzImage) -> Result<(), ClientError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ClientError::Internal("GLZ window lock poisoned"))?;
        if state.images.contains_key(&image.image_id) {
            return Err(protocol_value_error("duplicate GLZ image id"));
        }
        let retained_bytes = state
            .retained_bytes
            .checked_add(image.pixels.len())
            .ok_or_else(|| resource_limit_error("GLZ dictionary bytes"))?;
        state.retained_bytes = retained_bytes;
        state.images.insert(image.image_id, image.pixels.clone());
        state
            .oldest_by_image
            .insert(image.image_id, image.oldest_retained_image_id);
        loop {
            let next_image_id = state.next_contiguous_image_id;
            let Some(oldest) = state.oldest_by_image.remove(&next_image_id) else {
                break;
            };
            state.oldest_image_id = state.oldest_image_id.max(oldest);
            state.next_contiguous_image_id += 1;
        }
        let oldest = state.oldest_image_id;
        let evicted: Vec<_> = state
            .images
            .keys()
            .copied()
            .filter(|image_id| *image_id < oldest)
            .collect();
        for image_id in evicted {
            if let Some(pixels) = state.images.remove(&image_id) {
                state.retained_bytes -= pixels.len();
            }
        }
        if state.retained_bytes > DEFAULT_GLZ_DICTIONARY_BYTES {
            return Err(resource_limit_error("GLZ dictionary bytes"));
        }
        drop(state);
        self.changed.send_modify(|generation| *generation += 1);
        Ok(())
    }

    async fn wait_for(
        &self,
        image_id: u64,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<(), ClientError> {
        let mut changed = self.changed.subscribe();
        loop {
            {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| ClientError::Internal("GLZ window lock poisoned"))?;
                if state.images.contains_key(&image_id) {
                    return Ok(());
                }
                if image_id < state.oldest_image_id {
                    return Err(protocol_value_error("evicted GLZ image reference"));
                }
            }
            tokio::select! {
                result = changed.changed() => {
                    result.map_err(|_| ClientError::TaskTerminated)?;
                }
                result = cancel.changed() => {
                    if result.is_err() || *cancel.borrow() {
                        return Err(ClientError::Cancelled);
                    }
                }
            }
        }
    }
}

impl SurfaceBudget {
    /// Reserves bytes atomically so retained host handles remain inside the live budget.
    fn reserve(&self, bytes: usize) -> Result<(), ClientError> {
        let mut current = self.used.load(Ordering::Acquire);
        loop {
            let updated = current
                .checked_add(bytes)
                .ok_or_else(|| resource_limit_error("surface byte budget"))?;
            if updated > self.maximum {
                return Err(resource_limit_error("surface byte budget"));
            }
            match self.used.compare_exchange_weak(
                current,
                updated,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }
}

impl SurfaceData {
    /// Exposes the aligned surface backing store as RGBA bytes for raster decoders.
    fn pixel_bytes(&self) -> &[u8] {
        bytemuck::cast_slice(&self.pixels)
    }

    /// Exposes the aligned surface backing store as mutable RGBA bytes.
    fn pixel_bytes_mut(&mut self) -> &mut [u8] {
        bytemuck::cast_slice_mut(&mut self.pixels)
    }

    /// Allocates a checked RGBA surface under the per-task byte budget.
    fn new(create: SurfaceCreate, budget: Arc<SurfaceBudget>) -> Result<Self, ClientError> {
        if create.width == 0 || create.height == 0 {
            return Err(protocol_value_error("zero-sized surface"));
        }
        let pixel_bytes = usize::try_from(create.width)
            .ok()
            .and_then(|width| width.checked_mul(usize::try_from(create.height).ok()?))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| resource_limit_error("surface dimensions"))?;
        budget.reserve(pixel_bytes)?;
        Ok(Self {
            width: create.width,
            height: create.height,
            format: create.format,
            pixels: vec![0; pixel_bytes / 4],
            budget,
            allocated_bytes: pixel_bytes,
        })
    }

    /// Scales one decoded stream frame into its destination while preserving pixels outside clip.
    fn blit_stream_frame(
        &mut self,
        destination: Rect,
        clip: &StreamClip,
        image: &DecodedVideoFrame,
    ) -> Result<(), ClientError> {
        let destination = self.rect_bounds(destination)?;
        let source_width = usize::try_from(image.width)
            .map_err(|_| resource_limit_error("stream source width"))?;
        let source_height = usize::try_from(image.height)
            .map_err(|_| resource_limit_error("stream source height"))?;
        if source_width == 0 || source_height == 0 {
            return Err(protocol_value_error("stream source dimensions"));
        }
        for destination_row in 0..destination.height {
            let surface_y = destination.y + destination_row;
            let source_y = destination_row
                .checked_mul(source_height)
                .ok_or_else(|| resource_limit_error("stream source row"))?
                / destination.height;
            for destination_column in 0..destination.width {
                let surface_x = destination.x + destination_column;
                if !stream_clip_contains(clip, surface_x, surface_y)? {
                    continue;
                }
                let source_x = destination_column
                    .checked_mul(source_width)
                    .ok_or_else(|| resource_limit_error("stream source column"))?
                    / destination.width;
                let source_offset = source_y
                    .checked_mul(source_width)
                    .and_then(|pixels| pixels.checked_add(source_x))
                    .and_then(|pixels| pixels.checked_mul(4))
                    .ok_or_else(|| resource_limit_error("stream source pixel"))?;
                let destination_offset = self.pixel_offset(surface_x, surface_y)?;
                let source_pixel = image
                    .rgba
                    .get(source_offset..source_offset + 4)
                    .ok_or_else(|| protocol_value_error("stream source pixel bounds"))?;
                write_surface_pixel(
                    self.format,
                    &mut self.pixel_bytes_mut()[destination_offset..destination_offset + 4],
                    source_pixel,
                );
            }
        }
        Ok(())
    }

    /// Moves an existing same-surface region while honoring the inline Display clip.
    fn copy_bits(&mut self, copy: &CopyBits) -> Result<(), ClientError> {
        let destination = self.rect_bounds(copy.destination)?;
        let source_rect = Rect {
            top: copy.source_y,
            left: copy.source_x,
            bottom: copy
                .source_y
                .checked_add(
                    i32::try_from(destination.height)
                        .map_err(|_| resource_limit_error("copy bits height"))?,
                )
                .ok_or_else(|| resource_limit_error("copy bits bottom"))?,
            right: copy
                .source_x
                .checked_add(
                    i32::try_from(destination.width)
                        .map_err(|_| resource_limit_error("copy bits width"))?,
                )
                .ok_or_else(|| resource_limit_error("copy bits right"))?,
        };
        let source = self.rect_bounds(source_rect)?;
        if !matches!(copy.clip, CompositeClip::None) {
            let surface_width = usize::try_from(self.width)
                .map_err(|_| resource_limit_error("copy bits surface width"))?;
            let pixel_count = source
                .width
                .checked_mul(source.height)
                .ok_or_else(|| resource_limit_error("copy bits source pixels"))?;
            let mut source_pixels = Vec::with_capacity(pixel_count);
            for row in 0..source.height {
                let start = (source.y + row)
                    .checked_mul(surface_width)
                    .and_then(|pixels| pixels.checked_add(source.x))
                    .ok_or_else(|| resource_limit_error("copy bits source row"))?;
                source_pixels.extend_from_slice(&self.pixels[start..start + source.width]);
            }
            for row in 0..destination.height {
                for column in 0..destination.width {
                    let x = destination.x + column;
                    let y = destination.y + row;
                    if canvas::clip_contains(&copy.clip, x, y)? {
                        self.pixels[y * surface_width + x] =
                            source_pixels[row * source.width + column];
                    }
                }
            }
            return Ok(());
        }
        if destination.y > source.y {
            for row in (0..destination.height).rev() {
                self.copy_surface_row(&source, &destination, row)?;
            }
        } else {
            for row in 0..destination.height {
                self.copy_surface_row(&source, &destination, row)?;
            }
        }
        Ok(())
    }

    /// Copies one overlap-safe row without allocating a temporary region.
    fn copy_surface_row(
        &mut self,
        source: &Bounds,
        destination: &Bounds,
        row: usize,
    ) -> Result<(), ClientError> {
        let source_start = self.pixel_offset(source.x, source.y + row)?;
        let destination_start = self.pixel_offset(destination.x, destination.y + row)?;
        let row_bytes = destination.width * 4;
        self.pixel_bytes_mut()
            .copy_within(source_start..source_start + row_bytes, destination_start);
        Ok(())
    }

    /// Converts a signed protocol rectangle into checked surface coordinates.
    fn rect_bounds(&self, rect: Rect) -> Result<Bounds, ClientError> {
        image_rect_bounds(rect, self.width, self.height)
    }

    /// Computes one RGBA byte offset with checked arithmetic.
    fn pixel_offset(&self, x: usize, y: usize) -> Result<usize, ClientError> {
        usize::try_from(self.width)
            .ok()
            .and_then(|width| y.checked_mul(width))
            .and_then(|pixels| pixels.checked_add(x))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| resource_limit_error("surface pixel offset"))
    }
}
/// Converts one validated direct-color wire pixel to the shared RGBA surface format.
fn direct_color_to_rgba(
    format: BitmapFormat,
    pixel: &[u8],
    surface_format: SurfaceFormat,
) -> Result<[u8; 4], ClientError> {
    Ok(match format {
        BitmapFormat::Indexed1Be | BitmapFormat::Indexed4Be | BitmapFormat::Indexed8 => {
            return Err(protocol_value_error("indexed pixel on direct-color path"));
        }
        BitmapFormat::Rgb16 => {
            let value = u16::from_le_bytes([pixel[0], pixel[1]]);
            let red = expand_five_bits(((value >> 10) & 0x1f) as u8);
            let green = expand_five_bits(((value >> 5) & 0x1f) as u8);
            let blue = expand_five_bits((value & 0x1f) as u8);
            [red, green, blue, u8::MAX]
        }
        BitmapFormat::Bgr24 => [pixel[2], pixel[1], pixel[0], u8::MAX],
        BitmapFormat::Xrgb32 => [pixel[2], pixel[1], pixel[0], u8::MAX],
        BitmapFormat::Rgba32 => [
            pixel[2],
            pixel[1],
            pixel[0],
            if matches!(surface_format, SurfaceFormat::Argb32 | SurfaceFormat::A8) {
                pixel[3]
            } else {
                u8::MAX
            },
        ],
        BitmapFormat::Alpha8 => [u8::MAX, u8::MAX, u8::MAX, pixel[0]],
    })
}

/// Normalizes one decoded RGBA pixel to the destination surface semantics.
fn write_surface_pixel(surface_format: SurfaceFormat, destination: &mut [u8], source: &[u8]) {
    match surface_format {
        SurfaceFormat::A8 => destination.copy_from_slice(&[u8::MAX, u8::MAX, u8::MAX, source[3]]),
        SurfaceFormat::Xrgb32 => {
            destination.copy_from_slice(&[source[0], source[1], source[2], u8::MAX])
        }
        SurfaceFormat::Argb32 => destination.copy_from_slice(source),
    }
}

enum PreparedCompositeImage {
    Surface(oxide_spice_protocol::CompositeSurface),
    Pixels(OwnedCompositePixels),
}

struct OwnedCompositePixels {
    format: SurfaceFormat,
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

/// Prepares the optional image owned by one classic pattern brush.
async fn prepare_classic_brush(
    body: &[u8],
    brush: DisplayBrush,
    palette_cache: &mut PaletteCache,
    image_decode_slots: &Arc<Semaphore>,
    glz_window: &Arc<GlzWindow>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Option<PreparedCompositeImage>, ClientError> {
    let DisplayBrush::Pattern { image, .. } = brush else {
        return Ok(None);
    };
    prepare_composite_image(
        body,
        image,
        palette_cache,
        image_decode_slots,
        glz_window,
        cancel,
    )
    .await
    .map(Some)
}

/// Prepares the optional A1 or surface image referenced by a classic QMask.
async fn prepare_classic_mask(
    body: &[u8],
    mask: DisplayMask,
    palette_cache: &mut PaletteCache,
    image_decode_slots: &Arc<Semaphore>,
    glz_window: &Arc<GlzWindow>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Option<PreparedCompositeImage>, ClientError> {
    let Some(image) = mask.image else {
        return Ok(None);
    };
    if matches!(
        image,
        CompositeImage::Embedded(descriptor)
            if descriptor.image_type != DrawCopyImageType::Bitmap
    ) {
        return Err(unsupported_wire("compressed Canvas mask image"));
    }
    prepare_composite_image(
        body,
        image,
        palette_cache,
        image_decode_slots,
        glz_window,
        cancel,
    )
    .await
    .map(Some)
}

/// Decodes one embedded Composite image under the same bounds and cancellation paths as Draw Copy.
async fn prepare_composite_image(
    body: &[u8],
    image: CompositeImage,
    palette_cache: &mut PaletteCache,
    image_decode_slots: &Arc<Semaphore>,
    glz_window: &Arc<GlzWindow>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<PreparedCompositeImage, ClientError> {
    let CompositeImage::Embedded(descriptor) = image else {
        let CompositeImage::Surface(surface) = image else {
            unreachable!("Composite image variants are exhaustive")
        };
        return Ok(PreparedCompositeImage::Surface(surface));
    };
    let embedded = EmbeddedImage::decode(
        body,
        descriptor,
        CompressedImageUpdate::DEFAULT_MAX_COMPRESSED_BYTES,
    )?;
    match embedded {
        EmbeddedImage::Bitmap(bitmap) => {
            let palette = palette_cache.resolve(bitmap.palette)?;
            let format = if bitmap.format == BitmapFormat::Alpha8 {
                SurfaceFormat::A8
            } else if bitmap.format == BitmapFormat::Rgba32 {
                SurfaceFormat::Argb32
            } else {
                SurfaceFormat::Xrgb32
            };
            let pixel_count = usize::try_from(bitmap.width)
                .ok()
                .and_then(|width| width.checked_mul(usize::try_from(bitmap.height).ok()?))
                .ok_or_else(|| resource_limit_error("Composite bitmap dimensions"))?;
            let mut rgba = vec![0; pixel_count * 4];
            let row_pixels = usize::try_from(bitmap.width)
                .map_err(|_| resource_limit_error("Composite bitmap width"))?;
            let image_height = usize::try_from(bitmap.height)
                .map_err(|_| resource_limit_error("Composite bitmap height"))?;
            let stride = usize::try_from(bitmap.stride)
                .map_err(|_| resource_limit_error("Composite bitmap stride"))?;
            for logical_y in 0..image_height {
                let storage_y = if bitmap.top_down {
                    logical_y
                } else {
                    image_height - logical_y - 1
                };
                let source_start = storage_y
                    .checked_mul(stride)
                    .ok_or_else(|| resource_limit_error("Composite bitmap row"))?;
                let source_row = bitmap
                    .pixel_bytes
                    .get(source_start..source_start + stride)
                    .ok_or_else(|| protocol_value_error("Composite bitmap row"))?;
                let destination_start = logical_y
                    .checked_mul(row_pixels)
                    .and_then(|pixels| pixels.checked_mul(4))
                    .ok_or_else(|| resource_limit_error("Composite bitmap output row"))?;
                let destination_row =
                    &mut rgba[destination_start..destination_start + row_pixels * 4];
                if let Some(bytes_per_pixel) = bitmap.format.bytes_per_pixel() {
                    for (source_pixel, destination_pixel) in source_row
                        .chunks_exact(bytes_per_pixel)
                        .take(row_pixels)
                        .zip(destination_row.chunks_exact_mut(4))
                    {
                        let pixel = direct_color_to_rgba(bitmap.format, source_pixel, format)?;
                        write_surface_pixel(format, destination_pixel, &pixel);
                    }
                } else {
                    let palette = palette
                        .as_deref()
                        .ok_or_else(|| protocol_value_error("Composite bitmap palette"))?;
                    for (column, destination_pixel) in
                        destination_row.chunks_exact_mut(4).enumerate()
                    {
                        let index = indexed_palette_index(bitmap.format, source_row, column)?;
                        let color = palette
                            .get(index)
                            .ok_or_else(|| protocol_value_error("Composite palette index"))?;
                        write_surface_pixel(format, destination_pixel, color);
                    }
                }
            }
            Ok(PreparedCompositeImage::Pixels(OwnedCompositePixels {
                format,
                width: bitmap.width,
                height: bitmap.height,
                pixels: rgba_bytes_into_words(rgba),
            }))
        }
        EmbeddedImage::Jpeg(jpeg) => {
            let decode_slot = acquire_image_decode_slot(image_decode_slots, cancel).await?;
            let jpeg_bytes = jpeg.jpeg_bytes.to_vec();
            let jpeg_cancel = cancel.clone();
            let width = jpeg.width;
            let height = jpeg.height;
            let mut decoded = tokio::task::spawn_blocking(move || {
                decode_jpeg_with_cancel(
                    &jpeg_bytes,
                    width,
                    height,
                    DecodeLimits::DISPLAY,
                    move || *jpeg_cancel.borrow(),
                )
            })
            .await
            .map_err(|_| ClientError::Internal("Composite JPEG decode task panicked"))??;
            if let Some(alpha_lz_bytes) = jpeg.alpha_lz_bytes {
                let alpha_bytes = alpha_lz_bytes.to_vec();
                let alpha_cancel = cancel.clone();
                let alpha = tokio::task::spawn_blocking(move || {
                    decode_lz_with_cancel(&alpha_bytes, None, DecodeLimits::DISPLAY, || {
                        *alpha_cancel.borrow()
                    })
                })
                .await
                .map_err(|_| ClientError::Internal("Composite alpha decode task panicked"))??;
                attach_jpeg_alpha(
                    &mut decoded,
                    alpha,
                    jpeg.alpha_top_down
                        .ok_or(ClientError::Internal("Composite alpha orientation"))?,
                )?;
            }
            drop(decode_slot);
            Ok(PreparedCompositeImage::Pixels(OwnedCompositePixels {
                format: if jpeg.alpha_top_down.is_some() {
                    SurfaceFormat::Argb32
                } else {
                    SurfaceFormat::Xrgb32
                },
                width,
                height,
                pixels: rgba_bytes_into_words(decoded.pixels),
            }))
        }
        EmbeddedImage::Compressed(compressed) => {
            let palette = palette_cache.resolve(compressed.palette)?;
            let mut decode_slot =
                Some(acquire_image_decode_slot(image_decode_slots, cancel).await?);
            let compressed_bytes = Arc::<[u8]>::from(compressed.compressed_bytes);
            let width = compressed.width;
            let height = compressed.height;
            let decoded = match compressed.image_type {
                DrawCopyImageType::LzPalette | DrawCopyImageType::LzRgb => {
                    let decode_cancel = cancel.clone();
                    let decode_palette = palette.clone();
                    let bytes = compressed_bytes.clone();
                    let image = tokio::task::spawn_blocking(move || {
                        decode_lz_with_cancel(
                            &bytes,
                            decode_palette.as_deref(),
                            DecodeLimits::DISPLAY,
                            || *decode_cancel.borrow(),
                        )
                    })
                    .await
                    .map_err(|_| ClientError::Internal("Composite LZ decode task panicked"))??;
                    composite_pixels_from_lz(image)?
                }
                DrawCopyImageType::Lz4 => {
                    let decode_cancel = cancel.clone();
                    let bytes = compressed_bytes.clone();
                    let image = tokio::task::spawn_blocking(move || {
                        decode_lz4_with_cancel(&bytes, width, height, DecodeLimits::DISPLAY, || {
                            *decode_cancel.borrow()
                        })
                    })
                    .await
                    .map_err(|_| ClientError::Internal("Composite LZ4 decode task panicked"))??;
                    composite_pixels_from_oriented_rgba(
                        image.width,
                        image.height,
                        image.top_down,
                        image.pixels.to_vec(),
                        SurfaceFormat::Xrgb32,
                    )?
                }
                DrawCopyImageType::Quic => {
                    let decode_cancel = cancel.clone();
                    let bytes = compressed_bytes.clone();
                    let image = tokio::task::spawn_blocking(move || {
                        decode_quic_with_cancel(
                            &bytes,
                            width,
                            height,
                            DecodeLimits::DISPLAY,
                            || *decode_cancel.borrow(),
                        )
                    })
                    .await
                    .map_err(|_| ClientError::Internal("Composite QUIC decode task panicked"))??;
                    let rgba = image.pixels.into_iter().flatten().collect();
                    OwnedCompositePixels {
                        format: SurfaceFormat::Argb32,
                        width: image.width,
                        height: image.height,
                        pixels: rgba_bytes_into_words(rgba),
                    }
                }
                DrawCopyImageType::GlzRgb | DrawCopyImageType::ZlibGlzRgb => {
                    let glz_bytes = if let Some(output_bytes) = compressed.uncompressed_bytes {
                        let inflate_cancel = cancel.clone();
                        let bytes = compressed_bytes.clone();
                        tokio::task::spawn_blocking(move || {
                            inflate_zlib_exact_with_cancel(
                                &bytes,
                                output_bytes,
                                CompressedImageUpdate::DEFAULT_MAX_COMPRESSED_BYTES,
                                || *inflate_cancel.borrow(),
                            )
                        })
                        .await
                        .map_err(|_| ClientError::Internal("Composite zlib decode task panicked"))??
                        .into()
                    } else {
                        compressed_bytes.clone()
                    };
                    let image = loop {
                        let decode_window = glz_window.clone();
                        let decode_cancel = cancel.clone();
                        let bytes = glz_bytes.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            decode_glz_with_cancel(
                                &bytes,
                                DecodeLimits::DISPLAY,
                                |image_id| decode_window.resolve(image_id),
                                || *decode_cancel.borrow(),
                            )
                        })
                        .await
                        .map_err(|_| ClientError::Internal("Composite GLZ decode task panicked"))?;
                        match result {
                            Ok(image) => break image,
                            Err(error) if error.kind == GlzErrorKind::MissingReference => {
                                let image_id = error.missing_image_id.ok_or(
                                    ClientError::Internal("Composite GLZ reference identity"),
                                )?;
                                drop(decode_slot.take());
                                glz_window.wait_for(image_id, cancel).await?;
                                decode_slot = Some(
                                    acquire_image_decode_slot(image_decode_slots, cancel).await?,
                                );
                            }
                            Err(error) => return Err(error.into()),
                        }
                    };
                    glz_window.insert(&image)?;
                    composite_pixels_from_oriented_rgba(
                        image.width,
                        image.height,
                        image.top_down,
                        image.pixels.to_vec(),
                        SurfaceFormat::Xrgb32,
                    )?
                }
                DrawCopyImageType::Bitmap
                | DrawCopyImageType::Jpeg
                | DrawCopyImageType::JpegAlpha => {
                    return Err(ClientError::Internal(
                        "Composite compressed image dispatch mismatch",
                    ));
                }
            };
            drop(decode_slot);
            Ok(PreparedCompositeImage::Pixels(decoded))
        }
    }
}

async fn acquire_image_decode_slot(
    image_decode_slots: &Arc<Semaphore>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<tokio::sync::OwnedSemaphorePermit, ClientError> {
    tokio::select! {
        changed = cancel.changed() => {
            if changed.is_err() || *cancel.borrow() {
                Err(ClientError::Cancelled)
            } else {
                image_decode_slots.clone().acquire_owned().await
                    .map_err(|_| ClientError::Internal("image decode gate closed"))
            }
        }
        slot = image_decode_slots.clone().acquire_owned() => {
            slot.map_err(|_| ClientError::Internal("image decode gate closed"))
        }
    }
}

fn composite_pixels_from_lz(image: DecodedImage) -> Result<OwnedCompositePixels, ClientError> {
    match image.pixels {
        DecodedPixels::Rgba(rgba) => Ok(OwnedCompositePixels {
            format: SurfaceFormat::Argb32,
            width: image.width,
            height: image.height,
            pixels: rgba_bytes_into_words(rgba),
        }),
        DecodedPixels::Alpha8(alpha) => {
            let mut rgba = Vec::with_capacity(alpha.len() * 4);
            for value in alpha {
                rgba.extend_from_slice(&[u8::MAX, u8::MAX, u8::MAX, value]);
            }
            Ok(OwnedCompositePixels {
                format: SurfaceFormat::A8,
                width: image.width,
                height: image.height,
                pixels: rgba_bytes_into_words(rgba),
            })
        }
    }
}

fn composite_pixels_from_oriented_rgba(
    width: u32,
    height: u32,
    top_down: bool,
    rgba: Vec<u8>,
    format: SurfaceFormat,
) -> Result<OwnedCompositePixels, ClientError> {
    if top_down {
        return Ok(OwnedCompositePixels {
            format,
            width,
            height,
            pixels: rgba_bytes_into_words(rgba),
        });
    }
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| resource_limit_error("Composite decoded row"))?;
    let image_height =
        usize::try_from(height).map_err(|_| resource_limit_error("Composite decoded height"))?;
    let mut top_down_rgba = vec![0; rgba.len()];
    for destination_y in 0..image_height {
        let source_y = image_height - destination_y - 1;
        top_down_rgba[destination_y * row_bytes..(destination_y + 1) * row_bytes]
            .copy_from_slice(&rgba[source_y * row_bytes..(source_y + 1) * row_bytes]);
    }
    Ok(OwnedCompositePixels {
        format,
        width,
        height,
        pixels: rgba_bytes_into_words(top_down_rgba),
    })
}

fn rgba_bytes_into_words(bytes: Vec<u8>) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|pixel| u32::from_ne_bytes(pixel.try_into().expect("four-byte RGBA pixel")))
        .collect()
}

/// Applies one Composite operation while copying only inputs that alias the destination.
#[cfg(feature = "composite-pixman")]
async fn composite_surface_inputs(
    surfaces: &HashMap<u32, SurfaceHandle>,
    composite: &DrawComposite,
) -> Result<SurfaceHandle, ClientError> {
    let CompositeImage::Surface(source_descriptor) = composite.source else {
        return Err(unsupported_wire("Composite embedded source image"));
    };
    let mask_descriptor = match composite.mask {
        Some(CompositeImage::Surface(mask)) => Some(mask),
        Some(CompositeImage::Embedded(_)) => {
            return Err(unsupported_wire("Composite embedded mask image"));
        }
        None => None,
    };
    let destination_handle = surface_for_update(surfaces, composite.destination_surface_id)?;
    let source_handle = surface_for_update(surfaces, source_descriptor.surface_id)?;
    let mask_handle = mask_descriptor
        .as_ref()
        .map(|mask| surface_for_update(surfaces, mask.surface_id))
        .transpose()?;

    let source_aliases_destination =
        source_descriptor.surface_id == composite.destination_surface_id;
    let mask_aliases_source = mask_descriptor
        .as_ref()
        .is_some_and(|mask| mask.surface_id == source_descriptor.surface_id);
    let mask_aliases_destination = mask_descriptor
        .as_ref()
        .is_some_and(|mask| mask.surface_id == composite.destination_surface_id);

    let mut source_copy = if source_aliases_destination {
        Some(destination_handle.inner.read().await.pixels.clone())
    } else {
        None
    };
    let mut mask_copy = if mask_aliases_destination && !mask_aliases_source {
        Some(destination_handle.inner.read().await.pixels.clone())
    } else {
        None
    };
    let mut source_guard = if source_aliases_destination {
        None
    } else {
        Some(source_handle.inner.write().await)
    };
    let mut mask_guard =
        if mask_handle.is_some() && !mask_aliases_source && !mask_aliases_destination {
            Some(
                mask_handle
                    .as_ref()
                    .expect("validated separate mask handle")
                    .inner
                    .write()
                    .await,
            )
        } else {
            None
        };

    let source_format;
    let source_width;
    let source_height;
    let source_pixels = if let Some(copy) = source_copy.as_mut() {
        let destination = destination_handle.inner.read().await;
        source_format = destination.format;
        source_width = destination.width;
        source_height = destination.height;
        drop(destination);
        copy.as_mut_slice()
    } else {
        let source = source_guard
            .as_mut()
            .expect("non-aliased source guard exists");
        source_format = source.format;
        source_width = source.width;
        source_height = source.height;
        source.pixels.as_mut_slice()
    };

    let mut destination = destination_handle.inner.write().await;
    if source_descriptor.width != source_width || source_descriptor.height != source_height {
        return Err(protocol_value_error(
            "Composite source descriptor dimensions",
        ));
    }

    let separate_mask_metadata = if let Some(mask_descriptor) = mask_descriptor.as_ref() {
        if mask_aliases_source {
            if mask_descriptor.width != source_width || mask_descriptor.height != source_height {
                return Err(protocol_value_error("Composite mask descriptor dimensions"));
            }
            None
        } else if let Some(copy) = mask_copy.as_mut() {
            if mask_descriptor.width != destination.width
                || mask_descriptor.height != destination.height
            {
                return Err(protocol_value_error("Composite mask descriptor dimensions"));
            }
            Some((
                destination.format,
                destination.width,
                destination.height,
                copy.as_mut_slice(),
            ))
        } else {
            let mask = mask_guard.as_mut().expect("non-aliased mask guard exists");
            if mask_descriptor.width != mask.width || mask_descriptor.height != mask.height {
                return Err(protocol_value_error("Composite mask descriptor dimensions"));
            }
            Some((
                mask.format,
                mask.width,
                mask.height,
                mask.pixels.as_mut_slice(),
            ))
        }
    } else {
        None
    };

    render_composite(
        &mut destination,
        composite,
        (source_format, source_width, source_height, source_pixels),
        separate_mask_metadata,
        mask_aliases_source,
    )?;
    drop(destination);
    Ok(destination_handle)
}

#[cfg(feature = "composite-pixman")]
async fn composite_prepared(
    surfaces: &HashMap<u32, SurfaceHandle>,
    composite: &DrawComposite,
    mut source: PreparedCompositeImage,
    mut mask: Option<PreparedCompositeImage>,
) -> Result<SurfaceHandle, ClientError> {
    if matches!(source, PreparedCompositeImage::Surface(_))
        && mask
            .as_ref()
            .is_none_or(|image| matches!(image, PreparedCompositeImage::Surface(_)))
    {
        return composite_surface_inputs(surfaces, composite).await;
    }
    let destination_handle = surface_for_update(surfaces, composite.destination_surface_id)?;
    match (&mut source, mask.as_mut()) {
        (PreparedCompositeImage::Pixels(source), Some(PreparedCompositeImage::Pixels(mask))) => {
            let mut destination = destination_handle.inner.write().await;
            render_composite(
                &mut destination,
                composite,
                (
                    source.format,
                    source.width,
                    source.height,
                    &mut source.pixels,
                ),
                Some((mask.format, mask.width, mask.height, &mut mask.pixels)),
                false,
            )?;
        }
        (PreparedCompositeImage::Pixels(source), Some(PreparedCompositeImage::Surface(mask))) => {
            if mask.surface_id == composite.destination_surface_id {
                let mut mask_pixels = destination_handle.inner.read().await.pixels.clone();
                let mut destination = destination_handle.inner.write().await;
                let mask_format = destination.format;
                let mask_width = destination.width;
                let mask_height = destination.height;
                render_composite(
                    &mut destination,
                    composite,
                    (
                        source.format,
                        source.width,
                        source.height,
                        &mut source.pixels,
                    ),
                    Some((mask_format, mask_width, mask_height, &mut mask_pixels)),
                    false,
                )?;
            } else {
                let mask_handle = surface_for_update(surfaces, mask.surface_id)?;
                let mut mask_guard = mask_handle.inner.write().await;
                if mask.width != mask_guard.width || mask.height != mask_guard.height {
                    return Err(protocol_value_error("Composite mask descriptor dimensions"));
                }
                let mask_format = mask_guard.format;
                let mask_width = mask_guard.width;
                let mask_height = mask_guard.height;
                let mut destination = destination_handle.inner.write().await;
                render_composite(
                    &mut destination,
                    composite,
                    (
                        source.format,
                        source.width,
                        source.height,
                        &mut source.pixels,
                    ),
                    Some((mask_format, mask_width, mask_height, &mut mask_guard.pixels)),
                    false,
                )?;
            }
        }
        (PreparedCompositeImage::Pixels(source), None) => {
            let mut destination = destination_handle.inner.write().await;
            render_composite(
                &mut destination,
                composite,
                (
                    source.format,
                    source.width,
                    source.height,
                    &mut source.pixels,
                ),
                None,
                false,
            )?;
        }
        (PreparedCompositeImage::Surface(source), Some(PreparedCompositeImage::Pixels(mask))) => {
            if source.surface_id == composite.destination_surface_id {
                let mut source_pixels = destination_handle.inner.read().await.pixels.clone();
                let mut destination = destination_handle.inner.write().await;
                if source.width != destination.width || source.height != destination.height {
                    return Err(protocol_value_error(
                        "Composite source descriptor dimensions",
                    ));
                }
                let source_format = destination.format;
                let source_width = destination.width;
                let source_height = destination.height;
                render_composite(
                    &mut destination,
                    composite,
                    (
                        source_format,
                        source_width,
                        source_height,
                        &mut source_pixels,
                    ),
                    Some((mask.format, mask.width, mask.height, &mut mask.pixels)),
                    false,
                )?;
            } else {
                let source_handle = surface_for_update(surfaces, source.surface_id)?;
                let mut source_guard = source_handle.inner.write().await;
                if source.width != source_guard.width || source.height != source_guard.height {
                    return Err(protocol_value_error(
                        "Composite source descriptor dimensions",
                    ));
                }
                let source_format = source_guard.format;
                let source_width = source_guard.width;
                let source_height = source_guard.height;
                let mut destination = destination_handle.inner.write().await;
                render_composite(
                    &mut destination,
                    composite,
                    (
                        source_format,
                        source_width,
                        source_height,
                        &mut source_guard.pixels,
                    ),
                    Some((mask.format, mask.width, mask.height, &mut mask.pixels)),
                    false,
                )?;
            }
        }
        (PreparedCompositeImage::Surface(_), None)
        | (PreparedCompositeImage::Surface(_), Some(PreparedCompositeImage::Surface(_))) => {
            unreachable!("surface-only inputs returned above")
        }
    }
    Ok(destination_handle)
}

#[cfg(feature = "composite-pixman")]
type CompositeSurfacePixels<'a> = (SurfaceFormat, u32, u32, &'a mut [u32]);

/// Maps SPICE Composite semantics onto pixman without copying independent surfaces.
#[cfg(feature = "composite-pixman")]
fn render_composite(
    destination: &mut SurfaceData,
    composite: &DrawComposite,
    source: CompositeSurfacePixels<'_>,
    mask: Option<CompositeSurfacePixels<'_>>,
    mask_uses_source: bool,
) -> Result<(), ClientError> {
    let destination_bounds = destination.rect_bounds(composite.destination)?;
    let destination_width = usize::try_from(destination.width)
        .map_err(|_| resource_limit_error("Composite destination width"))?;
    let destination_height = usize::try_from(destination.height)
        .map_err(|_| resource_limit_error("Composite destination height"))?;
    let (source_format, source_width, source_height, source_pixels) = source;
    let source_width = usize::try_from(source_width)
        .map_err(|_| resource_limit_error("Composite source width"))?;
    let source_height = usize::try_from(source_height)
        .map_err(|_| resource_limit_error("Composite source height"))?;
    let mut source_image = PixmanImage::from_slice_mut(
        pixman_surface_format(source_format, composite.source_opaque),
        source_width,
        source_height,
        source_pixels,
        source_width
            .checked_mul(4)
            .ok_or_else(|| resource_limit_error("Composite source stride"))?,
        false,
    )
    .map_err(|_| resource_limit_error("Composite source image"))?;
    configure_composite_image(
        &mut source_image,
        composite.source_filter,
        composite.source_repeat,
        composite.source_transform,
        false,
    )?;

    let mask_image = if let Some((format, width, height, pixels)) = mask {
        let width =
            usize::try_from(width).map_err(|_| resource_limit_error("Composite mask width"))?;
        let height =
            usize::try_from(height).map_err(|_| resource_limit_error("Composite mask height"))?;
        let mut image = PixmanImage::from_slice_mut(
            pixman_surface_format(format, composite.mask_opaque),
            width,
            height,
            pixels,
            width
                .checked_mul(4)
                .ok_or_else(|| resource_limit_error("Composite mask stride"))?,
            false,
        )
        .map_err(|_| resource_limit_error("Composite mask image"))?;
        configure_composite_image(
            &mut image,
            composite.mask_filter,
            composite.mask_repeat,
            composite.mask_transform,
            composite.component_alpha,
        )?;
        Some(image)
    } else {
        None
    };
    if mask_uses_source {
        source_image.set_component_alpha(composite.component_alpha);
    }

    let destination_format = destination.format;
    let destination_stride = destination_width
        .checked_mul(4)
        .ok_or_else(|| resource_limit_error("Composite destination stride"))?;
    let mut destination_image = PixmanImage::from_slice_mut(
        pixman_surface_format(destination_format, composite.destination_opaque),
        destination_width,
        destination_height,
        destination.pixels.as_mut_slice(),
        destination_stride,
        false,
    )
    .map_err(|_| resource_limit_error("Composite destination image"))?;

    let clip_region = match &composite.clip {
        CompositeClip::None => None,
        CompositeClip::Rectangles(rectangles) => {
            let boxes: Vec<PixmanBox32> = rectangles
                .iter()
                .map(|rectangle| PixmanBox32 {
                    x1: rectangle.left,
                    y1: rectangle.top,
                    x2: rectangle.right,
                    y2: rectangle.bottom,
                })
                .collect();
            Some(PixmanRegion32::init_rects(&boxes))
        }
    };
    if let Some(region) = clip_region.as_ref() {
        destination_image
            .set_clip_region32(Some(region))
            .map_err(|_| unsupported_wire("Composite clip region"))?;
    }

    let operation = pixman_operation(composite.operation)?;
    let source_origin = (
        i32::from(composite.source_origin.x),
        i32::from(composite.source_origin.y),
    );
    let mask_origin = (
        i32::from(composite.mask_origin.x),
        i32::from(composite.mask_origin.y),
    );
    let destination_origin = (composite.destination.left, composite.destination.top);
    let size = (
        i32::try_from(destination_bounds.width)
            .map_err(|_| resource_limit_error("Composite width"))?,
        i32::try_from(destination_bounds.height)
            .map_err(|_| resource_limit_error("Composite height"))?,
    );
    let mask_reference = if mask_uses_source {
        Some(&*source_image)
    } else {
        mask_image.as_ref().map(|image| &**image)
    };
    destination_image.composite32(
        operation,
        &source_image,
        mask_reference,
        source_origin,
        mask_origin,
        destination_origin,
        size,
    );
    drop(destination_image);
    normalize_surface_region(destination, destination_bounds);
    Ok(())
}

#[cfg(feature = "composite-pixman")]
fn configure_composite_image(
    image: &mut PixmanImage<'_, '_>,
    filter: u8,
    repeat: u8,
    transform: Option<CompositeTransform>,
    component_alpha: bool,
) -> Result<(), ClientError> {
    image
        .set_filter(pixman_filter(filter)?, &[])
        .map_err(|_| unsupported_wire("Composite filter"))?;
    image.set_repeat(pixman_repeat(repeat)?);
    image.set_component_alpha(component_alpha);
    if let Some(transform) = transform {
        image
            .set_transform(pixman_transform(transform))
            .map_err(|_| protocol_value_error("Composite transform"))?;
    }
    Ok(())
}

#[cfg(feature = "composite-pixman")]
fn pixman_transform(transform: CompositeTransform) -> PixmanTransform {
    PixmanTransform::new([
        [
            PixmanFixed::from_raw(transform.xx),
            PixmanFixed::from_raw(transform.xy),
            PixmanFixed::from_raw(transform.x0),
        ],
        [
            PixmanFixed::from_raw(transform.yx),
            PixmanFixed::from_raw(transform.yy),
            PixmanFixed::from_raw(transform.y0),
        ],
        [PixmanFixed::ZERO, PixmanFixed::ZERO, PixmanFixed::ONE],
    ])
}

#[cfg(target_endian = "little")]
#[cfg(feature = "composite-pixman")]
fn pixman_surface_format(_format: SurfaceFormat, opaque: bool) -> PixmanFormat {
    if opaque {
        PixmanFormat::X8B8G8R8
    } else {
        PixmanFormat::A8B8G8R8
    }
}

#[cfg(target_endian = "big")]
#[cfg(feature = "composite-pixman")]
fn pixman_surface_format(_format: SurfaceFormat, opaque: bool) -> PixmanFormat {
    if opaque {
        PixmanFormat::R8G8B8X8
    } else {
        PixmanFormat::R8G8B8A8
    }
}

#[cfg(feature = "composite-pixman")]
fn pixman_filter(filter: u8) -> Result<PixmanFilter, ClientError> {
    match filter {
        0 => Ok(PixmanFilter::Fast),
        1 => Ok(PixmanFilter::Good),
        2 => Ok(PixmanFilter::Best),
        3 => Ok(PixmanFilter::Nearest),
        4 => Ok(PixmanFilter::Bilinear),
        5 => Ok(PixmanFilter::Convolution),
        6 => Ok(PixmanFilter::SeparableConvolution),
        _ => Err(unsupported_wire("Composite filter value")),
    }
}

#[cfg(feature = "composite-pixman")]
fn pixman_repeat(repeat: u8) -> Result<PixmanRepeat, ClientError> {
    match repeat {
        0 => Ok(PixmanRepeat::None),
        1 => Ok(PixmanRepeat::Normal),
        2 => Ok(PixmanRepeat::Pad),
        3 => Ok(PixmanRepeat::Reflect),
        _ => Err(protocol_value_error("Composite repeat value")),
    }
}

#[cfg(feature = "composite-pixman")]
fn pixman_operation(operation: u8) -> Result<PixmanOperation, ClientError> {
    let operation = match operation {
        0x00 => PixmanOperation::Clear,
        0x01 => PixmanOperation::Src,
        0x02 => PixmanOperation::Dst,
        0x03 => PixmanOperation::Over,
        0x04 => PixmanOperation::OverReverse,
        0x05 => PixmanOperation::In,
        0x06 => PixmanOperation::InReverse,
        0x07 => PixmanOperation::Out,
        0x08 => PixmanOperation::OutReverse,
        0x09 => PixmanOperation::Atop,
        0x0a => PixmanOperation::AtopReverse,
        0x0b => PixmanOperation::Xor,
        0x0c => PixmanOperation::Add,
        0x0d => PixmanOperation::Saturate,
        0x10 => PixmanOperation::DisjointClear,
        0x11 => PixmanOperation::DisjointSrc,
        0x12 => PixmanOperation::DisjointDst,
        0x13 => PixmanOperation::DisjointOver,
        0x14 => PixmanOperation::DisjointOverReverse,
        0x15 => PixmanOperation::DisjointIn,
        0x16 => PixmanOperation::DisjointInReverse,
        0x17 => PixmanOperation::DisjointOut,
        0x18 => PixmanOperation::DisjointOutReverse,
        0x19 => PixmanOperation::DisjointAtop,
        0x1a => PixmanOperation::DisjointAtopReverse,
        0x1b => PixmanOperation::DisjointXor,
        0x20 => PixmanOperation::ConjointClear,
        0x21 => PixmanOperation::ConjointSrc,
        0x22 => PixmanOperation::ConjointDst,
        0x23 => PixmanOperation::ConjointOver,
        0x24 => PixmanOperation::ConjointOverReverse,
        0x25 => PixmanOperation::ConjointIn,
        0x26 => PixmanOperation::ConjointInReverse,
        0x27 => PixmanOperation::ConjointOut,
        0x28 => PixmanOperation::ConjointOutReverse,
        0x29 => PixmanOperation::ConjointAtop,
        0x2a => PixmanOperation::ConjointAtopReverse,
        0x2b => PixmanOperation::ConjointXor,
        0x30 => PixmanOperation::Multiply,
        0x31 => PixmanOperation::Screen,
        0x32 => PixmanOperation::Overlay,
        0x33 => PixmanOperation::Darken,
        0x34 => PixmanOperation::Lighten,
        0x35 => PixmanOperation::ColorDodge,
        0x36 => PixmanOperation::ColorBurn,
        0x37 => PixmanOperation::HardLight,
        0x38 => PixmanOperation::SoftLight,
        0x39 => PixmanOperation::Difference,
        0x3a => PixmanOperation::Exclustion,
        0x3b => PixmanOperation::HslHue,
        0x3c => PixmanOperation::HslSaturation,
        0x3d => PixmanOperation::HslColor,
        0x3e => PixmanOperation::HslLuminosity,
        _ => return Err(unsupported_wire("Composite operation")),
    };
    Ok(operation)
}

#[cfg(feature = "composite-pixman")]
fn normalize_surface_region(surface: &mut SurfaceData, bounds: Bounds) {
    if surface.format == SurfaceFormat::Argb32 {
        return;
    }
    let surface_format = surface.format;
    for row in bounds.y..bounds.y + bounds.height {
        let row_start = (row * usize::try_from(surface.width).expect("surface width fits usize")
            + bounds.x)
            * 4;
        let row_end = row_start + bounds.width * 4;
        for pixel in surface.pixel_bytes_mut()[row_start..row_end].chunks_exact_mut(4) {
            match surface_format {
                SurfaceFormat::A8 => {
                    pixel[0] = u8::MAX;
                    pixel[1] = u8::MAX;
                    pixel[2] = u8::MAX;
                }
                SurfaceFormat::Xrgb32 => pixel[3] = u8::MAX,
                SurfaceFormat::Argb32 => unreachable!("returned above"),
            }
        }
    }
}

fn indexed_palette_index(
    format: BitmapFormat,
    source_row: &[u8],
    x: usize,
) -> Result<usize, ClientError> {
    match format {
        BitmapFormat::Indexed1Be => source_row
            .get(x / 8)
            .map(|byte| usize::from((byte >> (7 - x % 8)) & 1))
            .ok_or_else(|| protocol_value_error("1-bit bitmap row")),
        BitmapFormat::Indexed4Be => source_row
            .get(x / 2)
            .map(|byte| usize::from(if x & 1 == 0 { byte >> 4 } else { byte & 0x0f }))
            .ok_or_else(|| protocol_value_error("4-bit bitmap row")),
        BitmapFormat::Indexed8 => source_row
            .get(x)
            .map(|index| usize::from(*index))
            .ok_or_else(|| protocol_value_error("8-bit bitmap row")),
        _ => Err(protocol_value_error("direct-color pixel on indexed path")),
    }
}

const fn expand_five_bits(value: u8) -> u8 {
    (value << 3) | (value >> 2)
}

impl Drop for SurfaceData {
    /// Releases budget only when no task, event, or host handle can access the pixels.
    fn drop(&mut self) {
        self.budget
            .used
            .fetch_sub(self.allocated_bytes, Ordering::AcqRel);
    }
}

/// Checked unsigned rectangle used only after validating signed wire coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bounds {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

/// Immutable ownership and delivery paths for one Display task.
pub(crate) struct DisplayTaskContext {
    pub connection_generation: u64,
    pub display_channel_id: u8,
    pub frame_sender: watch::Sender<Option<FrameEvent>>,
    pub topology_sender: watch::Sender<Option<DisplayTopology>>,
    pub surface_budget: Arc<SurfaceBudget>,
    pub image_decode_slots: Arc<Semaphore>,
    pub glz_window: Arc<GlzWindow>,
    pub progress: ProgressRegistry,
    #[cfg(unix)]
    pub gl_frame_sender: mpsc::Sender<GlFrame>,
}

/// Sends the client initialization required by every newly linked Display transport.
pub(crate) async fn initialize_display_channel<S>(
    channel: &mut Channel<S>,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let display_init = DisplayInit {
        pixmap_cache_id: 0,
        pixmap_cache_size: 0,
        glz_dictionary_id: GLZ_DICTIONARY_ID,
        glz_dictionary_window_size: i32::try_from(DEFAULT_GLZ_DICTIONARY_BYTES / 4)
            .expect("GLZ dictionary size fits protocol field"),
    };
    channel
        .write_message(display_client::INIT, &display_init.encode())
        .await?;
    if channel.peer_supports(display_capability::PREFERRED_COMPRESSION) {
        channel
            .write_message(
                display_client::PREFERRED_COMPRESSION,
                &ImageCompression::Lz.encode(),
            )
            .await?;
    }
    if channel.peer_supports(display_capability::PREFERRED_VIDEO_CODEC) {
        channel
            .write_message(
                display_client::PREFERRED_VIDEO_CODEC,
                &encode_preferred_video_codecs(&preferred_video_codecs())?,
            )
            .await?;
    }
    Ok(())
}

/// Runs the linked Display channel until cancellation or a terminal protocol result.
pub(crate) async fn run_display<S>(
    mut channel: Channel<S>,
    mut cancel: watch::Receiver<bool>,
    context: DisplayTaskContext,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let DisplayTaskContext {
        connection_generation,
        display_channel_id,
        frame_sender,
        topology_sender,
        surface_budget,
        image_decode_slots,
        glz_window,
        progress,
        #[cfg(unix)]
        gl_frame_sender,
    } = context;
    initialize_display_channel(&mut channel).await?;

    let mut control = ControlState::new();
    let mut surfaces: HashMap<u32, SurfaceHandle> = HashMap::new();
    let mut streams: HashMap<u32, StreamRuntime> = HashMap::new();
    let mut palette_cache = PaletteCache::new();
    let mut graphics_epoch = 1_u64;
    #[cfg(unix)]
    let mut gl_scanout: Option<Arc<DmaBufScanout>> = None;
    let identity = ChannelIdentity {
        channel_type: oxide_spice_protocol::ChannelType::Display,
        channel_id: display_channel_id,
    };
    let mut message_body = Vec::new();
    let mut observed_migration_activation = channel.migration_activation_count();
    loop {
        let header = tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    // A peer that closed first has already achieved the same transport cleanup.
                    let _ = channel.shutdown().await;
                    return Ok(());
                }
                continue;
            }
            message = channel.read_message(&mut message_body) => message?,
        };
        let envelope = IncomingEnvelope::decode(header, &message_body)?;
        let counts_for_ack = envelope.counts_for_ack();
        let serial = channel.received_serial();
        if let Some(seamless) =
            channel.observe_migration_activation(&mut observed_migration_activation)
            && !seamless
        {
            surfaces.clear();
            streams.clear();
            palette_cache.clear();
            glz_window.clear_for_migration(observed_migration_activation)?;
            graphics_epoch = graphics_epoch
                .checked_add(1)
                .ok_or_else(|| resource_limit_error("graphics epoch"))?;
            frame_sender.send_replace(None);
            topology_sender.send_replace(None);
            #[cfg(unix)]
            {
                gl_scanout = None;
            }
        }
        for message in envelope.messages() {
            if control.handle_without_ack(&mut channel, &message).await?
                == ControlDisposition::Consumed
            {
                continue;
            }
            if handle_channel_wait(&progress, identity, serial, &mut cancel, &message).await? {
                continue;
            }
            match message.header.message_type {
                display_server::MODE => {
                    if message.body.len() != 12 {
                        return Err(protocol_value_error("display mode body"));
                    }
                }
                display_server::MARK => {
                    if !message.body.is_empty() {
                        return Err(protocol_value_error("empty Display control body"));
                    }
                }
                display_server::INVALIDATE_LIST => {
                    let _ = InvalidateList::decode(message.body)?;
                }
                display_server::INVALIDATE_ALL_PIXMAPS => {
                    let waits = WaitForChannels::decode(message.body)?;
                    progress
                        .wait_for(identity, serial, &waits, &mut cancel)
                        .await?;
                }
                display_server::INVALIDATE_PALETTE => {
                    if message.body.len() != 8 {
                        return Err(protocol_value_error("palette invalidation body"));
                    }
                    let unique_id = u64::from_le_bytes(
                        message.body.try_into().expect("validated palette id body"),
                    );
                    palette_cache.invalidate(unique_id);
                }
                display_server::INVALIDATE_ALL_PALETTES => {
                    if !message.body.is_empty() {
                        return Err(protocol_value_error("palette invalidation body"));
                    }
                    palette_cache.clear();
                }
                display_server::RESET => {
                    if !message.body.is_empty() {
                        return Err(protocol_value_error("Display Reset body"));
                    }
                    surfaces.clear();
                    streams.clear();
                    frame_sender.send_replace(None);
                    topology_sender.send_replace(None);
                    palette_cache.clear();
                    graphics_epoch = graphics_epoch
                        .checked_add(1)
                        .ok_or_else(|| resource_limit_error("graphics epoch"))?;
                    #[cfg(unix)]
                    {
                        gl_scanout = None;
                    }
                }
                display_server::SURFACE_CREATE => {
                    let create = SurfaceCreate::decode(message.body)?;
                    if surfaces.contains_key(&create.surface_id) {
                        return Err(protocol_value_error("duplicate surface id"));
                    }
                    let surface = SurfaceData::new(create, surface_budget.clone())?;
                    surfaces.insert(
                        create.surface_id,
                        SurfaceHandle {
                            display_channel_id,
                            surface_id: create.surface_id,
                            width: create.width,
                            height: create.height,
                            is_primary: create.flags & SURFACE_FLAG_PRIMARY != 0,
                            inner: Arc::new(RwLock::new(surface)),
                        },
                    );
                }
                display_server::SURFACE_DESTROY => {
                    if message.body.len() != 4 {
                        return Err(protocol_value_error("surface destroy body"));
                    }
                    let surface_id = u32::from_le_bytes(
                        message.body[..4].try_into().expect("validated fixed body"),
                    );
                    if surfaces
                        .remove(&surface_id)
                        .is_some_and(|surface| surface.is_primary)
                    {
                        frame_sender.send_replace(None);
                    }
                    streams.retain(|_, stream| stream.create.surface_id != surface_id);
                }
                display_server::MONITORS_CONFIG => {
                    let config = MonitorsConfig::decode(message.body)?;
                    for monitor in &config.heads {
                        let surface = surfaces.get(&monitor.surface_id).ok_or_else(|| {
                            protocol_value_error("monitor references unknown surface")
                        })?;
                        let right = monitor
                            .x
                            .checked_add(monitor.width)
                            .ok_or_else(|| protocol_value_error("monitor right edge"))?;
                        let bottom = monitor
                            .y
                            .checked_add(monitor.height)
                            .ok_or_else(|| protocol_value_error("monitor bottom edge"))?;
                        if right > surface.width || bottom > surface.height {
                            return Err(protocol_value_error("monitor exceeds surface bounds"));
                        }
                    }
                    topology_sender.send_replace(Some(DisplayTopology {
                        connection_generation,
                        graphics_epoch,
                        display_channel_id,
                        maximum_allowed: config.maximum_allowed,
                        monitors: config.heads.into(),
                    }));
                }
                display_server::STREAM_CREATE => {
                    let create = StreamCreate::decode(message.body)?;
                    if streams.contains_key(&create.stream_id) {
                        return Err(protocol_value_error("duplicate Display stream id"));
                    }
                    if streams.len() == MAX_ACTIVE_STREAMS {
                        return Err(resource_limit_error("active Display streams"));
                    }
                    let surface = surface_for_update(&surfaces, create.surface_id)?;
                    surface.inner.read().await.rect_bounds(create.destination)?;
                    let decoder_codec = match create.codec {
                        VideoCodec::Mjpeg => SpiceVideoCodec::Mjpeg,
                        VideoCodec::Vp8 => SpiceVideoCodec::Vp8,
                        VideoCodec::H264 => SpiceVideoCodec::H264,
                        VideoCodec::Vp9 => SpiceVideoCodec::Vp9,
                        VideoCodec::H265 => SpiceVideoCodec::H265,
                    };
                    let decoder = VideoDecoderWorker::start(
                        decoder_codec,
                        create.stream_width,
                        create.stream_height,
                    )
                    .await?;
                    streams.insert(
                        create.stream_id,
                        StreamRuntime {
                            create,
                            decoder,
                            report: None,
                        },
                    );
                }
                display_server::STREAM_CLIP => {
                    let update = StreamClipUpdate::decode(message.body)?;
                    let stream = streams
                        .get_mut(&update.stream_id)
                        .ok_or_else(|| protocol_value_error("unknown Display stream clip id"))?;
                    stream.create.clip = update.clip;
                }
                display_server::STREAM_DESTROY => {
                    if message.body.len() != 4 {
                        return Err(protocol_value_error("Display stream destroy body"));
                    }
                    let stream_id =
                        u32::from_le_bytes(message.body.try_into().expect("validated stream id"));
                    if streams.remove(&stream_id).is_none() {
                        return Err(protocol_value_error("unknown Display stream destroy id"));
                    }
                }
                display_server::STREAM_DESTROY_ALL => {
                    if !message.body.is_empty() {
                        return Err(protocol_value_error("Display stream destroy-all body"));
                    }
                    streams.clear();
                }
                display_server::STREAM_ACTIVATE_REPORT => {
                    let activation = StreamReportActivation::decode(message.body)?;
                    let stream = streams
                        .get_mut(&activation.stream_id)
                        .ok_or_else(|| protocol_value_error("unknown Display stream report id"))?;
                    stream.report = Some(StreamReportState {
                        unique_id: activation.unique_id,
                        maximum_window_frames: activation.maximum_window_frames,
                        timeout: Duration::from_millis(u64::from(activation.timeout_ms)),
                        window_started: Instant::now(),
                        start_frame_multimedia_time: None,
                        end_frame_multimedia_time: 0,
                        frame_count: 0,
                    });
                }
                display_server::STREAM_DATA | display_server::STREAM_DATA_SIZED => {
                    let frame = StreamData::decode(
                        message.body,
                        message.header.message_type == display_server::STREAM_DATA_SIZED,
                    )?;
                    let create = streams
                        .get(&frame.stream_id)
                        .ok_or_else(|| protocol_value_error("unknown Display stream data id"))?
                        .create
                        .clone();
                    let frame_width = frame.width.unwrap_or(create.stream_width);
                    let frame_height = frame.height.unwrap_or(create.stream_height);
                    let destination = frame.destination.unwrap_or(create.destination);
                    let stream = streams
                        .get(&frame.stream_id)
                        .ok_or_else(|| protocol_value_error("unknown Display stream data id"))?;
                    let compressed = Arc::<[u8]>::from(frame.data);
                    let image = tokio::select! {
                        changed = cancel.changed() => {
                            if changed.is_err() || *cancel.borrow() {
                                let _ = channel.shutdown().await;
                                return Ok(());
                            }
                            continue;
                        }
                        decoded = stream.decoder.decode(compressed, frame_width, frame_height) => decoded,
                    }?;
                    if let Some(image) = image {
                        let surface = surface_for_update(&surfaces, create.surface_id)?;
                        surface.inner.write().await.blit_stream_frame(
                            destination,
                            &create.clip,
                            &image,
                        )?;
                        notify_surface(
                            &frame_sender,
                            connection_generation,
                            graphics_epoch,
                            create.surface_id,
                            destination,
                            surface,
                        );
                    }

                    let report = streams
                        .get_mut(&frame.stream_id)
                        .and_then(|stream| stream.report.as_mut())
                        .and_then(|report| {
                            report
                                .start_frame_multimedia_time
                                .get_or_insert(frame.multimedia_time);
                            report.end_frame_multimedia_time = frame.multimedia_time;
                            report.frame_count = report.frame_count.saturating_add(1);
                            if report.frame_count < report.maximum_window_frames
                                && report.window_started.elapsed() < report.timeout
                            {
                                return None;
                            }
                            let completed = StreamReport {
                                stream_id: frame.stream_id,
                                unique_id: report.unique_id,
                                start_frame_multimedia_time: report
                                    .start_frame_multimedia_time
                                    .unwrap_or(frame.multimedia_time),
                                end_frame_multimedia_time: report.end_frame_multimedia_time,
                                frame_count: report.frame_count,
                                dropped_frame_count: 0,
                                last_frame_delay_ms: 0,
                                audio_delay_ms: u32::MAX,
                            };
                            report.window_started = Instant::now();
                            report.start_frame_multimedia_time = None;
                            report.frame_count = 0;
                            Some(completed)
                        });
                    if let Some(report) = report {
                        channel
                            .write_message(display_client::STREAM_REPORT, &report.encode())
                            .await?;
                    }
                }
                display_server::DRAW_FILL => {
                    let command = ClassicDrawFill::decode(message.body)?;
                    let pattern = prepare_classic_brush(
                        message.body,
                        command.brush,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let mask = prepare_classic_mask(
                        message.body,
                        command.mask,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let surface =
                        canvas::render_fill(&surfaces, &command, pattern.as_ref(), mask.as_ref())
                            .await?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        command.base.surface_id,
                        command.base.destination,
                        surface,
                    );
                }
                display_server::DRAW_OPAQUE => {
                    let command = DrawOpaque::decode(message.body)?;
                    let source = prepare_composite_image(
                        message.body,
                        command.source.image,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let pattern = prepare_classic_brush(
                        message.body,
                        command.brush,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let mask = prepare_classic_mask(
                        message.body,
                        command.mask,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let surface = canvas::render_opaque(
                        &surfaces,
                        &command,
                        &source,
                        pattern.as_ref(),
                        mask.as_ref(),
                    )
                    .await?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        command.base.surface_id,
                        command.base.destination,
                        surface,
                    );
                }
                display_server::DRAW_COPY | display_server::DRAW_BLEND => {
                    let command = ClassicDrawCopy::decode(message.body)?;
                    let source = prepare_composite_image(
                        message.body,
                        command.source.image,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let mask = prepare_classic_mask(
                        message.body,
                        command.mask,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let surface =
                        canvas::render_copy(&surfaces, &command, &source, mask.as_ref()).await?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        command.base.surface_id,
                        command.base.destination,
                        surface,
                    );
                }
                display_server::DRAW_BLACKNESS
                | display_server::DRAW_WHITENESS
                | display_server::DRAW_INVERS => {
                    let command = DrawMaskedDestination::decode(message.body)?;
                    let mask = prepare_classic_mask(
                        message.body,
                        command.mask,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let operation = match message.header.message_type {
                        display_server::DRAW_BLACKNESS => {
                            canvas::MaskedDestinationOperation::Blackness
                        }
                        display_server::DRAW_WHITENESS => {
                            canvas::MaskedDestinationOperation::Whiteness
                        }
                        display_server::DRAW_INVERS => canvas::MaskedDestinationOperation::Invert,
                        _ => unreachable!("classic destination operation was matched above"),
                    };
                    let surface = canvas::render_masked_destination(
                        &surfaces,
                        &command,
                        mask.as_ref(),
                        operation,
                    )
                    .await?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        command.base.surface_id,
                        command.base.destination,
                        surface,
                    );
                }
                display_server::DRAW_ROP3 => {
                    let command = DrawRop3::decode(message.body)?;
                    let source = prepare_composite_image(
                        message.body,
                        command.source.image,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let pattern = prepare_classic_brush(
                        message.body,
                        command.brush,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let mask = prepare_classic_mask(
                        message.body,
                        command.mask,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let surface = canvas::render_rop3(
                        &surfaces,
                        &command,
                        &source,
                        pattern.as_ref(),
                        mask.as_ref(),
                    )
                    .await?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        command.base.surface_id,
                        command.base.destination,
                        surface,
                    );
                }
                display_server::DRAW_STROKE => {
                    let command = DrawStroke::decode(message.body)?;
                    let pattern = prepare_classic_brush(
                        message.body,
                        command.brush,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let surface =
                        canvas::render_stroke(&surfaces, &command, pattern.as_ref()).await?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        command.base.surface_id,
                        command.base.destination,
                        surface,
                    );
                }
                display_server::DRAW_TEXT => {
                    let command = DrawText::decode(message.body)?;
                    let foreground_pattern = prepare_classic_brush(
                        message.body,
                        command.foreground_brush,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let background_pattern = prepare_classic_brush(
                        message.body,
                        command.background_brush,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let surface = canvas::render_text(
                        &surfaces,
                        &command,
                        foreground_pattern.as_ref(),
                        background_pattern.as_ref(),
                    )
                    .await?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        command.base.surface_id,
                        command.base.destination,
                        surface,
                    );
                }
                display_server::DRAW_TRANSPARENT => {
                    let command = DrawTransparent::decode(message.body)?;
                    let source = prepare_composite_image(
                        message.body,
                        command.source_image,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let surface = canvas::render_transparent(&surfaces, &command, &source).await?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        command.base.surface_id,
                        command.base.destination,
                        surface,
                    );
                }
                display_server::DRAW_ALPHA_BLEND => {
                    let command = DrawAlphaBlend::decode(message.body)?;
                    let source = prepare_composite_image(
                        message.body,
                        command.source_image,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let surface = canvas::render_alpha_blend(&surfaces, &command, &source).await?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        command.base.surface_id,
                        command.base.destination,
                        surface,
                    );
                }
                display_server::COPY_BITS => {
                    let copy = CopyBits::decode(message.body)?;
                    let surface = surface_for_update(&surfaces, copy.surface_id)?;
                    surface.inner.write().await.copy_bits(&copy)?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        copy.surface_id,
                        copy.destination,
                        surface,
                    );
                }
                #[cfg(feature = "composite-pixman")]
                display_server::DRAW_COMPOSITE => {
                    let composite = DrawComposite::decode(message.body)?;
                    let source = prepare_composite_image(
                        message.body,
                        composite.source,
                        &mut palette_cache,
                        &image_decode_slots,
                        &glz_window,
                        &mut cancel,
                    )
                    .await?;
                    let mask = match composite.mask {
                        Some(mask) => Some(
                            prepare_composite_image(
                                message.body,
                                mask,
                                &mut palette_cache,
                                &image_decode_slots,
                                &glz_window,
                                &mut cancel,
                            )
                            .await?,
                        ),
                        None => None,
                    };
                    let destination =
                        composite_prepared(&surfaces, &composite, source, mask).await?;
                    notify_surface(
                        &frame_sender,
                        connection_generation,
                        graphics_epoch,
                        composite.destination_surface_id,
                        composite.destination,
                        destination,
                    );
                }
                #[cfg(not(feature = "composite-pixman"))]
                display_server::DRAW_COMPOSITE => {
                    return Err(unsupported_wire("Draw Composite without composite-pixman"));
                }
                #[cfg(unix)]
                display_server::GL_SCANOUT_UNIX => {
                    let scanout = GlScanoutUnix::decode(message.body)?;
                    gl_scanout = if scanout.disabled() {
                        None
                    } else {
                        let file_descriptor = channel
                            .take_received_file_descriptor()?
                            .ok_or_else(|| protocol_value_error("missing GL scanout descriptor"))?;
                        Some(Arc::new(DmaBufScanout {
                            width: scanout.width,
                            height: scanout.height,
                            fourcc: scanout.fourcc,
                            modifier: 0,
                            top_down: scanout.top_down,
                            planes: vec![DmaBufPlane {
                                file_descriptor: Arc::new(file_descriptor),
                                offset: 0,
                                stride: scanout.stride,
                            }]
                            .into(),
                        }))
                    };
                }
                #[cfg(unix)]
                display_server::GL_SCANOUT2_UNIX => {
                    let scanout = GlScanout2Unix::decode(message.body)?;
                    if scanout.disabled() {
                        gl_scanout = None;
                    } else {
                        let mut planes = Vec::with_capacity(scanout.planes.len());
                        for plane in scanout.planes {
                            let file_descriptor =
                                channel.take_received_file_descriptor()?.ok_or_else(|| {
                                    protocol_value_error("missing GL scanout2 descriptor")
                                })?;
                            planes.push(DmaBufPlane {
                                file_descriptor: Arc::new(file_descriptor),
                                offset: plane.offset,
                                stride: plane.stride,
                            });
                        }
                        gl_scanout = Some(Arc::new(DmaBufScanout {
                            width: scanout.width,
                            height: scanout.height,
                            fourcc: scanout.fourcc,
                            modifier: scanout.modifier,
                            top_down: scanout.top_down,
                            planes: planes.into(),
                        }));
                    }
                }
                #[cfg(unix)]
                display_server::GL_DRAW => {
                    let dirty = GlDraw::decode(message.body)?;
                    let scanout = gl_scanout
                        .as_ref()
                        .cloned()
                        .ok_or_else(|| protocol_value_error("GL draw without scanout"))?;
                    if dirty
                        .x
                        .checked_add(dirty.width)
                        .is_none_or(|right| right > scanout.width)
                        || dirty
                            .y
                            .checked_add(dirty.height)
                            .is_none_or(|bottom| bottom > scanout.height)
                    {
                        return Err(protocol_value_error("GL draw outside scanout"));
                    }
                    let (completion, completed) = oneshot::channel();
                    let frame = GlFrame {
                        connection_generation,
                        display_channel_id,
                        dirty,
                        scanout,
                        completion: Some(completion),
                    };
                    tokio::select! {
                        changed = cancel.changed() => {
                            if changed.is_err() || *cancel.borrow() {
                                let _ = channel.shutdown().await;
                                return Ok(());
                            }
                        }
                        sent = gl_frame_sender.send(frame) => {
                            sent.map_err(|_| ClientError::TaskTerminated)?;
                        }
                    }
                    tokio::select! {
                        changed = cancel.changed() => {
                            if changed.is_err() || *cancel.borrow() {
                                let _ = channel.shutdown().await;
                                return Ok(());
                            }
                        }
                        result = completed => {
                            result.map_err(|_| ClientError::TaskTerminated)?;
                        }
                    }
                    channel
                        .write_message(display_client::GL_DRAW_DONE, &[])
                        .await?;
                }
                common_server::MIGRATE | common_server::MIGRATE_DATA => {
                    return Err(ClientError::UnsupportedMessage {
                        channel: "display",
                        message_type: message.header.message_type,
                    });
                }
                message_type => {
                    return Err(ClientError::UnsupportedMessage {
                        channel: "display",
                        message_type,
                    });
                }
            }
        }
        if counts_for_ack {
            control.acknowledge_envelope(&mut channel).await?;
        }
        progress.complete(identity, serial)?;
    }
}

/// Validates and merges the LZ `XXXA` plane carried by a JPEG_ALPHA image.
fn attach_jpeg_alpha(
    image: &mut DecodedJpeg,
    alpha: DecodedImage,
    declared_top_down: bool,
) -> Result<(), ClientError> {
    if alpha.image_type != LzImageType::XxxAlpha {
        return Err(protocol_value_error("JPEG alpha LZ image type"));
    }
    if alpha.width != image.width || alpha.height != image.height {
        return Err(protocol_value_error("JPEG alpha dimensions"));
    }
    if alpha.top_down != declared_top_down {
        return Err(protocol_value_error("JPEG alpha row orientation"));
    }
    let DecodedPixels::Alpha8(alpha_samples) = alpha.pixels else {
        return Err(protocol_value_error("JPEG alpha sample format"));
    };
    if alpha_samples.len().checked_mul(4) != Some(image.pixels.len()) {
        return Err(protocol_value_error("JPEG alpha sample count"));
    }
    for (pixel, alpha_sample) in image
        .pixels
        .chunks_exact_mut(4)
        .zip(alpha_samples.into_iter())
    {
        pixel[3] = alpha_sample;
    }
    Ok(())
}

fn stream_clip_contains(clip: &StreamClip, x: usize, y: usize) -> Result<bool, ClientError> {
    match clip {
        StreamClip::None => Ok(true),
        StreamClip::Rectangles(rectangles) => {
            let x = i64::try_from(x).map_err(|_| resource_limit_error("stream clip x"))?;
            let y = i64::try_from(y).map_err(|_| resource_limit_error("stream clip y"))?;
            Ok(rectangles.iter().any(|rectangle| {
                x >= i64::from(rectangle.left)
                    && x < i64::from(rectangle.right)
                    && y >= i64::from(rectangle.top)
                    && y < i64::from(rectangle.bottom)
            }))
        }
    }
}

/// Validates a signed rectangle against an image without unchecked casts.
fn image_rect_bounds(rect: Rect, width: u32, height: u32) -> Result<Bounds, ClientError> {
    let x = usize::try_from(rect.left).map_err(|_| protocol_value_error("negative rectangle x"))?;
    let y = usize::try_from(rect.top).map_err(|_| protocol_value_error("negative rectangle y"))?;
    let rect_width =
        usize::try_from(rect.width()?).map_err(|_| resource_limit_error("rectangle width"))?;
    let rect_height =
        usize::try_from(rect.height()?).map_err(|_| resource_limit_error("rectangle height"))?;
    let image_width = usize::try_from(width).map_err(|_| resource_limit_error("image width"))?;
    let image_height = usize::try_from(height).map_err(|_| resource_limit_error("image height"))?;
    if x.checked_add(rect_width)
        .is_none_or(|right| right > image_width)
        || y.checked_add(rect_height)
            .is_none_or(|bottom| bottom > image_height)
    {
        return Err(protocol_value_error("rectangle outside image"));
    }
    Ok(Bounds {
        x,
        y,
        width: rect_width,
        height: rect_height,
    })
}

/// Resolves a surface identity without allowing an update to create implicit state.
fn surface_for_update(
    surfaces: &HashMap<u32, SurfaceHandle>,
    surface_id: u32,
) -> Result<SurfaceHandle, ClientError> {
    surfaces
        .get(&surface_id)
        .cloned()
        .ok_or_else(|| protocol_value_error("update for unknown surface"))
}

/// Replaces the latest notification; the shared surface preserves all intermediate updates.
fn notify_surface(
    sender: &watch::Sender<Option<FrameEvent>>,
    connection_generation: u64,
    graphics_epoch: u64,
    surface_id: u32,
    dirty: Rect,
    surface: SurfaceHandle,
) {
    if !surface.is_primary {
        return;
    }
    sender.send_replace(Some(FrameEvent {
        connection_generation,
        graphics_epoch,
        display_channel_id: surface.display_channel_id,
        surface_id,
        dirty,
        full_refresh_required: true,
        surface,
    }));
}

/// Creates a structured resource-limit failure using the protocol error path.
fn resource_limit_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::ResourceLimit,
        0,
        context,
    )
    .into()
}

/// Creates a structured invalid-value failure without retaining peer bytes.
fn protocol_value_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::InvalidValue,
        0,
        context,
    )
    .into()
}

/// Creates a structured unsupported-feature failure.
fn unsupported_wire(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::Unsupported,
        0,
        context,
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "composite-pixman")]
    use oxide_spice_protocol::{CompositeSurface, Point16};

    #[cfg(feature = "composite-pixman")]
    fn composite_command(
        operation: u8,
        destination: Rect,
        source_surface_id: u32,
        source_width: u32,
        source_height: u32,
    ) -> DrawComposite {
        DrawComposite {
            destination_surface_id: 0,
            destination,
            clip: CompositeClip::None,
            operation,
            source_filter: 3,
            mask_filter: 3,
            source_repeat: 0,
            mask_repeat: 0,
            component_alpha: false,
            source_opaque: false,
            mask_opaque: false,
            destination_opaque: false,
            source: CompositeImage::Surface(CompositeSurface {
                image_id: 1,
                image_flags: 0,
                width: source_width,
                height: source_height,
                surface_id: source_surface_id,
            }),
            mask: None,
            source_transform: None,
            mask_transform: None,
            source_origin: Point16 { x: 0, y: 0 },
            mask_origin: Point16 { x: 0, y: 0 },
        }
    }

    #[test]
    #[cfg(feature = "composite-pixman")]
    fn composite_source_operation_copies_surface_pixels() {
        let budget = Arc::new(SurfaceBudget {
            used: AtomicUsize::new(0),
            maximum: 16,
        });
        let create = |surface_id| SurfaceCreate {
            surface_id,
            width: 2,
            height: 1,
            format: SurfaceFormat::Xrgb32,
            flags: 0,
        };
        let mut destination = SurfaceData::new(create(0), budget.clone()).expect("destination");
        let mut source = SurfaceData::new(create(1), budget).expect("source");
        source
            .pixel_bytes_mut()
            .copy_from_slice(&[10, 20, 30, 255, 40, 50, 60, 255]);
        let composite = composite_command(
            0x01,
            Rect {
                top: 0,
                left: 0,
                bottom: 1,
                right: 2,
            },
            1,
            2,
            1,
        );
        let source_format = source.format;
        let source_width = source.width;
        let source_height = source.height;
        render_composite(
            &mut destination,
            &composite,
            (
                source_format,
                source_width,
                source_height,
                source.pixels.as_mut_slice(),
            ),
            None,
            false,
        )
        .expect("Composite renders");
        assert_eq!(
            destination.pixel_bytes(),
            [10, 20, 30, 255, 40, 50, 60, 255]
        );
    }

    #[test]
    #[cfg(feature = "composite-pixman")]
    fn composite_a8_mask_applies_unified_alpha() {
        let budget = Arc::new(SurfaceBudget {
            used: AtomicUsize::new(0),
            maximum: 12,
        });
        let mut destination = SurfaceData::new(
            SurfaceCreate {
                surface_id: 0,
                width: 1,
                height: 1,
                format: SurfaceFormat::Xrgb32,
                flags: 0,
            },
            budget.clone(),
        )
        .expect("destination");
        destination
            .pixel_bytes_mut()
            .copy_from_slice(&[0, 0, 255, 255]);
        let mut source = SurfaceData::new(
            SurfaceCreate {
                surface_id: 1,
                width: 1,
                height: 1,
                format: SurfaceFormat::Xrgb32,
                flags: 0,
            },
            budget.clone(),
        )
        .expect("source");
        source.pixel_bytes_mut().copy_from_slice(&[255, 0, 0, 255]);
        let mut mask = SurfaceData::new(
            SurfaceCreate {
                surface_id: 2,
                width: 1,
                height: 1,
                format: SurfaceFormat::A8,
                flags: 0,
            },
            budget,
        )
        .expect("mask");
        mask.pixel_bytes_mut()
            .copy_from_slice(&[255, 255, 255, 128]);
        let mut composite = composite_command(
            0x03,
            Rect {
                top: 0,
                left: 0,
                bottom: 1,
                right: 1,
            },
            1,
            1,
            1,
        );
        composite.mask = Some(CompositeImage::Surface(CompositeSurface {
            image_id: 2,
            image_flags: 0,
            width: 1,
            height: 1,
            surface_id: 2,
        }));
        render_composite(
            &mut destination,
            &composite,
            (
                source.format,
                source.width,
                source.height,
                source.pixels.as_mut_slice(),
            ),
            Some((
                mask.format,
                mask.width,
                mask.height,
                mask.pixels.as_mut_slice(),
            )),
            false,
        )
        .expect("masked Composite renders");
        assert_eq!(destination.pixel_bytes(), [128, 0, 127, 255]);
    }

    #[test]
    fn direct_color_pixels_expand_to_rgba_without_row_copies() {
        assert_eq!(
            direct_color_to_rgba(
                BitmapFormat::Bgr24,
                &[0x10, 0x20, 0x30],
                SurfaceFormat::Xrgb32
            )
            .expect("direct-color pixel"),
            [0x30, 0x20, 0x10, u8::MAX]
        );
        let red_rgb555 = 0x7C00_u16.to_le_bytes();
        assert_eq!(
            direct_color_to_rgba(BitmapFormat::Rgb16, &red_rgb555, SurfaceFormat::Xrgb32)
                .expect("direct-color pixel"),
            [u8::MAX, 0, 0, u8::MAX]
        );
    }

    #[test]
    fn palette_cache_resolves_wire_colors_and_invalidates_by_identity() {
        let mut cache = PaletteCache::new();
        let inline = cache
            .resolve(Some(BitmapPalette::Inline {
                unique_id: 17,
                cache_me: true,
                entries_bgrx: &[0x10, 0x20, 0x30, 0, 0x40, 0x50, 0x60, 0],
            }))
            .expect("inline palette")
            .expect("palette entries");
        assert_eq!(
            inline.as_ref(),
            &[[0x30, 0x20, 0x10, 255], [0x60, 0x50, 0x40, 255]]
        );
        let cached = cache
            .resolve(Some(BitmapPalette::Cached { unique_id: 17 }))
            .expect("cached palette")
            .expect("cached entries");
        assert!(Arc::ptr_eq(&inline, &cached));

        cache.invalidate(17);
        let error = cache
            .resolve(Some(BitmapPalette::Cached { unique_id: 17 }))
            .expect_err("invalidated palette must miss");
        assert_eq!(error.category(), crate::ErrorCategory::Protocol);
    }

    #[test]
    fn indexed_big_endian_pixels_select_expected_palette_entries() {
        assert_eq!(
            indexed_palette_index(BitmapFormat::Indexed1Be, &[0b1000_0000], 0)
                .expect("first 1-bit pixel"),
            1
        );
        assert_eq!(
            indexed_palette_index(BitmapFormat::Indexed4Be, &[0xA3], 0).expect("high nibble first"),
            10
        );
        assert_eq!(
            indexed_palette_index(BitmapFormat::Indexed4Be, &[0xA3], 1).expect("low nibble second"),
            3
        );
    }

    #[test]
    fn retained_surface_handle_keeps_bytes_charged_to_budget() {
        let budget = Arc::new(SurfaceBudget {
            used: AtomicUsize::new(0),
            maximum: 4,
        });
        let create = SurfaceCreate {
            surface_id: 0,
            width: 1,
            height: 1,
            format: SurfaceFormat::Xrgb32,
            flags: SURFACE_FLAG_PRIMARY,
        };
        let retained = Arc::new(RwLock::new(
            SurfaceData::new(create, budget.clone()).expect("first surface fits"),
        ));
        let host_handle = retained.clone();
        drop(retained);

        let error = match SurfaceData::new(create, budget.clone()) {
            Ok(_) => panic!("retained host handle must keep its bytes charged"),
            Err(error) => error,
        };
        assert_eq!(error.category(), crate::ErrorCategory::ResourceLimit);
        drop(host_handle);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        SurfaceData::new(create, budget).expect("released bytes can be reused");
    }

    #[test]
    fn jpeg_alpha_requires_matching_xxxa_plane_and_merges_samples() {
        let mut jpeg = DecodedJpeg {
            width: 2,
            height: 1,
            pixels: vec![10, 20, 30, 255, 40, 50, 60, 255],
        };
        attach_jpeg_alpha(
            &mut jpeg,
            DecodedImage {
                width: 2,
                height: 1,
                top_down: true,
                image_type: LzImageType::XxxAlpha,
                pixels: DecodedPixels::Alpha8(vec![17, 34]),
            },
            true,
        )
        .expect("matching alpha plane");
        assert_eq!(jpeg.pixels, [10, 20, 30, 17, 40, 50, 60, 34]);

        let error = attach_jpeg_alpha(
            &mut jpeg,
            DecodedImage {
                width: 2,
                height: 1,
                top_down: false,
                image_type: LzImageType::XxxAlpha,
                pixels: DecodedPixels::Alpha8(vec![17, 34]),
            },
            true,
        )
        .expect_err("outer and LZ row orientation must agree");
        assert_eq!(error.category(), crate::ErrorCategory::Protocol);
    }
}
