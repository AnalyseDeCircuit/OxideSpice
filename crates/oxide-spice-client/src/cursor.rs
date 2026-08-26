//! Cursor state, bounded image cache, and latest-only host notifications.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxide_spice_protocol::{
    CursorHeader, CursorImage, CursorInit, CursorPosition, CursorSet, CursorType, cursor_server,
    decode_cursor_cache_id, decode_cursor_position, decode_cursor_trail,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::ClientError;
use crate::channel::{
    Channel, ChannelIdentity, ControlDisposition, ControlState, IncomingEnvelope, ProgressRegistry,
    handle_channel_wait,
};

/// Protocol cursor caches traditionally contain at most 256 entries.
const MAX_CURSOR_CACHE_ENTRIES: usize = 256;
/// Hard limit for decoded images retained by one Cursor channel.
const MAX_CURSOR_CACHE_BYTES: usize = 4 * 1024 * 1024;
/// Prevents a malformed cursor dimension from causing disproportionate allocation.
const MAX_CURSOR_DIMENSION: u16 = 512;
/// Upper bound for one decoded RGBA cursor image.
const MAX_CURSOR_IMAGE_BYTES: usize = 1024 * 1024;

/// Host-ready cursor image in straight, unpremultiplied RGBA byte order.
#[derive(Debug)]
pub struct CursorShape {
    pub unique_id: u64,
    pub width: u16,
    pub height: u16,
    pub hot_spot_x: u16,
    pub hot_spot_y: u16,
    pub rgba: Arc<[u8]>,
    _allocation: CursorAllocation,
}

impl PartialEq for CursorShape {
    fn eq(&self, other: &Self) -> bool {
        self.unique_id == other.unique_id
            && self.width == other.width
            && self.height == other.height
            && self.hot_spot_x == other.hot_spot_x
            && self.hot_spot_y == other.hot_spot_y
            && self.rgba == other.rgba
    }
}

impl Eq for CursorShape {}

/// Latest complete cursor state; intermediate moves may be coalesced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorState {
    pub connection_generation: u64,
    pub channel_id: u8,
    pub cursor_epoch: u64,
    pub position: CursorPosition,
    pub visible: bool,
    pub shape: Option<Arc<CursorShape>>,
    pub trail_length: u16,
    pub trail_frequency: u16,
}

/// Cloneable latest-only cursor event stream independent from frame consumption.
#[derive(Clone)]
pub struct CursorEvents {
    receiver: watch::Receiver<Option<CursorState>>,
}

impl std::fmt::Debug for CursorEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CursorEvents")
            .field("latest", &*self.receiver.borrow())
            .finish_non_exhaustive()
    }
}

impl CursorEvents {
    /// Returns the latest complete state after Cursor Init has arrived.
    pub fn latest(&self) -> Option<CursorState> {
        self.receiver.borrow().clone()
    }

    /// Waits for the next complete state while intermediate moves may be coalesced.
    pub async fn next(&mut self) -> Result<CursorState, ClientError> {
        loop {
            self.receiver
                .changed()
                .await
                .map_err(|_| ClientError::TaskTerminated)?;
            if let Some(state) = self.receiver.borrow_and_update().clone() {
                return Ok(state);
            }
        }
    }
}

/// Creates a host event receiver and its task-owned sender.
pub(crate) fn cursor_events() -> (watch::Sender<Option<CursorState>>, CursorEvents) {
    let (sender, receiver) = watch::channel(None);
    (sender, CursorEvents { receiver })
}

#[derive(Debug)]
struct CursorBudget {
    used: AtomicUsize,
    maximum: usize,
}

impl CursorBudget {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<CursorAllocation, ClientError> {
        let mut current = self.used.load(Ordering::Acquire);
        loop {
            let updated = current
                .checked_add(bytes)
                .ok_or_else(|| resource_limit_error("cursor live byte budget"))?;
            if updated > self.maximum {
                return Err(resource_limit_error("cursor live byte budget"));
            }
            match self.used.compare_exchange_weak(
                current,
                updated,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(CursorAllocation {
                        bytes,
                        budget: self.clone(),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

#[derive(Debug)]
struct CursorAllocation {
    bytes: usize,
    budget: Arc<CursorBudget>,
}

impl Drop for CursorAllocation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

struct CursorCache {
    entries: HashMap<u64, Arc<CursorShape>>,
}

impl CursorCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn insert(&mut self, shape: Arc<CursorShape>) -> Result<(), ClientError> {
        let is_new_entry = !self.entries.contains_key(&shape.unique_id);
        if is_new_entry && self.entries.len() >= MAX_CURSOR_CACHE_ENTRIES {
            return Err(resource_limit_error("cursor cache capacity"));
        }
        self.entries.insert(shape.unique_id, shape);
        Ok(())
    }

    fn remove(&mut self, unique_id: u64) {
        self.entries.remove(&unique_id);
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Owns cursor cache semantics and publishes only complete states.
pub(crate) async fn run_cursor<S>(
    mut channel: Channel<S>,
    mut cancel: watch::Receiver<bool>,
    state_sender: watch::Sender<Option<CursorState>>,
    connection_generation: u64,
    channel_id: u8,
    progress: ProgressRegistry,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut control = ControlState::new();
    let mut cache = CursorCache::new();
    let budget = Arc::new(CursorBudget {
        used: AtomicUsize::new(0),
        maximum: MAX_CURSOR_CACHE_BYTES,
    });
    let mut state = CursorState {
        connection_generation,
        channel_id,
        cursor_epoch: 1,
        ..CursorState::default()
    };
    let mut message_body = Vec::new();
    let mut awaiting_init = true;
    let identity = ChannelIdentity {
        channel_type: oxide_spice_protocol::ChannelType::Cursor,
        channel_id,
    };
    let mut observed_migration_activation = channel.migration_activation_count();
    loop {
        let header = tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    let _ = channel.shutdown().await;
                    return Ok(());
                }
                continue;
            }
            incoming = channel.read_message(&mut message_body) => incoming?,
        };
        let envelope = IncomingEnvelope::decode(header, &message_body)?;
        let counts_for_ack = envelope.counts_for_ack();
        let serial = channel.received_serial();
        if let Some(seamless) =
            channel.observe_migration_activation(&mut observed_migration_activation)
            && !seamless
        {
            let next_epoch = state
                .cursor_epoch
                .checked_add(1)
                .ok_or_else(|| resource_limit_error("cursor epoch"))?;
            state = CursorState {
                connection_generation,
                channel_id,
                cursor_epoch: next_epoch,
                ..CursorState::default()
            };
            cache.clear();
            awaiting_init = true;
            state_sender.send_replace(None);
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
            if awaiting_init && message.header.message_type != cursor_server::INIT {
                return Err(protocol_value_error("Cursor message before Init"));
            }
            match message.header.message_type {
                cursor_server::INIT => {
                    if !awaiting_init {
                        return Err(protocol_value_error("repeated Cursor Init"));
                    }
                    let update = CursorInit::decode(message.body)?;
                    cache.clear();
                    state.position = update.position;
                    state.trail_length = update.trail_length;
                    state.trail_frequency = update.trail_frequency;
                    state.visible = update.visible;
                    state.shape = resolve_image(update.image, &mut cache, &budget)?;
                    awaiting_init = false;
                }
                cursor_server::RESET => {
                    require_empty(message.body, "cursor reset body")?;
                    let next_epoch = state
                        .cursor_epoch
                        .checked_add(1)
                        .ok_or_else(|| resource_limit_error("cursor epoch"))?;
                    state = CursorState {
                        connection_generation,
                        channel_id,
                        cursor_epoch: next_epoch,
                        ..CursorState::default()
                    };
                    cache.clear();
                    awaiting_init = true;
                }
                cursor_server::SET => {
                    let update = CursorSet::decode(message.body)?;
                    state.position = update.position;
                    state.visible = update.visible;
                    state.shape = resolve_image(update.image, &mut cache, &budget)?;
                }
                cursor_server::MOVE => {
                    state.position = decode_cursor_position(message.body)?;
                    state.visible = true;
                }
                cursor_server::HIDE => {
                    require_empty(message.body, "cursor hide body")?;
                    state.visible = false;
                }
                cursor_server::TRAIL => {
                    (state.trail_length, state.trail_frequency) =
                        decode_cursor_trail(message.body)?;
                }
                cursor_server::INVALIDATE_ONE => {
                    cache.remove(decode_cursor_cache_id(message.body)?);
                }
                cursor_server::INVALIDATE_ALL => {
                    require_empty(message.body, "cursor invalidate all body")?;
                    cache.clear();
                }
                message_type => {
                    return Err(ClientError::UnsupportedMessage {
                        channel: "cursor",
                        message_type,
                    });
                }
            }
            state_sender.send_replace(Some(state.clone()));
        }
        if counts_for_ack {
            control.acknowledge_envelope(&mut channel).await?;
        }
        progress.complete(identity, serial)?;
    }
}

fn resolve_image(
    image: CursorImage<'_>,
    cache: &mut CursorCache,
    budget: &Arc<CursorBudget>,
) -> Result<Option<Arc<CursorShape>>, ClientError> {
    match image {
        CursorImage::None => Ok(None),
        CursorImage::Cached(unique_id) => cache
            .entries
            .get(&unique_id)
            .cloned()
            .map(Some)
            .ok_or_else(|| protocol_value_error("cursor cache miss")),
        CursorImage::Data {
            header,
            cache_me,
            data,
        } => {
            let shape = Arc::new(decode_shape(header, data, budget)?);
            if cache_me {
                cache.insert(shape.clone())?;
            }
            Ok(Some(shape))
        }
    }
}

fn decode_shape(
    header: CursorHeader,
    data: &[u8],
    budget: &Arc<CursorBudget>,
) -> Result<CursorShape, ClientError> {
    if header.width == 0
        || header.height == 0
        || header.width > MAX_CURSOR_DIMENSION
        || header.height > MAX_CURSOR_DIMENSION
        || header.hot_spot_x > header.width
        || header.hot_spot_y > header.height
    {
        return Err(resource_limit_error("cursor dimensions or hotspot"));
    }
    let pixels = usize::from(header.width)
        .checked_mul(usize::from(header.height))
        .ok_or_else(|| resource_limit_error("cursor pixel count"))?;
    let rgba_bytes = pixels
        .checked_mul(4)
        .ok_or_else(|| resource_limit_error("cursor image bytes"))?;
    if rgba_bytes > MAX_CURSOR_IMAGE_BYTES {
        return Err(resource_limit_error("cursor image bytes"));
    }
    let allocation = budget.reserve(rgba_bytes)?;
    let rgba = match header.cursor_type {
        CursorType::Alpha => decode_alpha_cursor(data, rgba_bytes)?,
        CursorType::Mono => {
            decode_mono_cursor(data, usize::from(header.width), usize::from(header.height))?
        }
        CursorType::Color4 => decode_palette_cursor(
            data,
            usize::from(header.width),
            usize::from(header.height),
            4,
        )?,
        CursorType::Color8 => decode_palette_cursor(
            data,
            usize::from(header.width),
            usize::from(header.height),
            8,
        )?,
        CursorType::Color32 => {
            decode_color32_cursor(data, usize::from(header.width), usize::from(header.height))?
        }
        CursorType::Color16 => {
            decode_color16_cursor(data, usize::from(header.width), usize::from(header.height))?
        }
        CursorType::Color24 => {
            decode_color24_cursor(data, usize::from(header.width), usize::from(header.height))?
        }
    };
    Ok(CursorShape {
        unique_id: header.unique_id,
        width: header.width,
        height: header.height,
        hot_spot_x: header.hot_spot_x,
        hot_spot_y: header.hot_spot_y,
        rgba: rgba.into(),
        _allocation: allocation,
    })
}

fn decode_alpha_cursor(data: &[u8], expected_bytes: usize) -> Result<Vec<u8>, ClientError> {
    if data.len() < expected_bytes {
        return Err(protocol_value_error("alpha cursor data size"));
    }
    let mut rgba = Vec::with_capacity(expected_bytes);
    for bgra in data[..expected_bytes].chunks_exact(4) {
        let alpha = bgra[3];
        rgba.extend_from_slice(&[
            unpremultiply(bgra[2], alpha),
            unpremultiply(bgra[1], alpha),
            unpremultiply(bgra[0], alpha),
            alpha,
        ]);
    }
    Ok(rgba)
}

fn unpremultiply(component: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        return 0;
    }
    let straight =
        (u16::from(component) * u16::from(u8::MAX) + u16::from(alpha) / 2) / u16::from(alpha);
    straight.min(u16::from(u8::MAX)) as u8
}

fn decode_mono_cursor(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ClientError> {
    let mask_stride = cursor_mask_stride(width)?;
    let mask_bytes = mask_stride
        .checked_mul(height)
        .ok_or_else(|| resource_limit_error("monochrome cursor mask"))?;
    let expected_bytes = mask_bytes
        .checked_mul(2)
        .ok_or_else(|| resource_limit_error("monochrome cursor bytes"))?;
    if data.len() < expected_bytes {
        return Err(protocol_value_error("monochrome cursor data size"));
    }
    let (and_mask, xor_and_padding) = data.split_at(mask_bytes);
    let xor_mask = &xor_and_padding[..mask_bytes];
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            match (
                cursor_mask_bit(and_mask, mask_stride, x, y),
                cursor_mask_bit(xor_mask, mask_stride, x, y),
            ) {
                (false, false) => rgba.extend_from_slice(&[0, 0, 0, u8::MAX]),
                (false, true) => rgba.extend_from_slice(&[u8::MAX; 4]),
                (true, false) => rgba.extend_from_slice(&[0; 4]),
                (true, true) => push_invert_fallback(&mut rgba, x, y),
            }
        }
    }
    Ok(rgba)
}

fn decode_palette_cursor(
    data: &[u8],
    width: usize,
    height: usize,
    bits_per_pixel: u8,
) -> Result<Vec<u8>, ClientError> {
    let (palette_entries, row_stride) = match bits_per_pixel {
        4 => (
            16_usize,
            width
                .checked_add(1)
                .map(|pixels| pixels / 2)
                .ok_or_else(|| resource_limit_error("color4 cursor stride"))?,
        ),
        8 => (256, width),
        _ => return Err(protocol_value_error("palette cursor bit depth")),
    };
    let index_bytes = row_stride
        .checked_mul(height)
        .ok_or_else(|| resource_limit_error("palette cursor indices"))?;
    let palette_bytes = palette_entries
        .checked_mul(4)
        .ok_or_else(|| resource_limit_error("cursor palette bytes"))?;
    let mask_stride = cursor_mask_stride(width)?;
    let mask_bytes = mask_stride
        .checked_mul(height)
        .ok_or_else(|| resource_limit_error("palette cursor mask"))?;
    let expected_bytes = index_bytes
        .checked_add(palette_bytes)
        .and_then(|bytes| bytes.checked_add(mask_bytes))
        .ok_or_else(|| resource_limit_error("palette cursor bytes"))?;
    if data.len() < expected_bytes {
        return Err(protocol_value_error("palette cursor data size"));
    }
    let indices = &data[..index_bytes];
    let palette = &data[index_bytes..index_bytes + palette_bytes];
    let mask = &data[index_bytes + palette_bytes..expected_bytes];
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let palette_index = if bits_per_pixel == 4 {
                let packed = indices[y * row_stride + x / 2];
                if x & 1 == 0 {
                    packed >> 4
                } else {
                    packed & 0x0f
                }
            } else {
                indices[y * row_stride + x]
            };
            let entry = usize::from(palette_index) * 4;
            let color = [palette[entry + 2], palette[entry + 1], palette[entry]];
            push_masked_color(
                &mut rgba,
                color,
                cursor_mask_bit(mask, mask_stride, x, y),
                x,
                y,
            );
        }
    }
    Ok(rgba)
}

fn decode_color32_cursor(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ClientError> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| resource_limit_error("color32 cursor pixels"))?;
    let color_bytes = pixels
        .checked_mul(4)
        .ok_or_else(|| resource_limit_error("color32 cursor bytes"))?;
    decode_masked_color_cursor(data, width, height, 4, color_bytes, |pixel| {
        [pixel[2], pixel[1], pixel[0]]
    })
}

fn decode_color16_cursor(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ClientError> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| resource_limit_error("color16 cursor pixels"))?;
    let color_bytes = pixels
        .checked_mul(2)
        .ok_or_else(|| resource_limit_error("color16 cursor bytes"))?;
    decode_masked_color_cursor(data, width, height, 2, color_bytes, |pixel| {
        let value = u16::from_le_bytes([pixel[0], pixel[1]]);
        [
            ((value >> 10) as u8 & 0x1f) << 3,
            ((value >> 5) as u8 & 0x1f) << 3,
            (value as u8 & 0x1f) << 3,
        ]
    })
}

fn decode_color24_cursor(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ClientError> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| resource_limit_error("color24 cursor pixels"))?;
    let color_bytes = pixels
        .checked_mul(3)
        .ok_or_else(|| resource_limit_error("color24 cursor bytes"))?;
    decode_masked_color_cursor(data, width, height, 3, color_bytes, |pixel| {
        [pixel[2], pixel[1], pixel[0]]
    })
}

fn decode_masked_color_cursor(
    data: &[u8],
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    color_bytes: usize,
    decode_color: impl Fn(&[u8]) -> [u8; 3],
) -> Result<Vec<u8>, ClientError> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| resource_limit_error("cursor pixel count"))?;
    let mask_stride = cursor_mask_stride(width)?;
    let mask_bytes = mask_stride
        .checked_mul(height)
        .ok_or_else(|| resource_limit_error("cursor mask bytes"))?;
    let expected_bytes = color_bytes
        .checked_add(mask_bytes)
        .ok_or_else(|| resource_limit_error("masked cursor bytes"))?;
    if data.len() < expected_bytes || color_bytes != pixels * bytes_per_pixel {
        return Err(protocol_value_error("masked cursor data size"));
    }
    let (colors, mask) = data.split_at(color_bytes);
    let mut rgba = Vec::with_capacity(pixels * 4);
    for (pixel_index, pixel) in colors.chunks_exact(bytes_per_pixel).enumerate() {
        let color = decode_color(pixel);
        let x = pixel_index % width;
        let y = pixel_index / width;
        push_masked_color(
            &mut rgba,
            color,
            cursor_mask_bit(mask, mask_stride, x, y),
            x,
            y,
        );
    }
    Ok(rgba)
}

fn cursor_mask_stride(width: usize) -> Result<usize, ClientError> {
    width
        .checked_add(7)
        .map(|bits| bits / 8)
        .ok_or_else(|| resource_limit_error("cursor mask stride"))
}

fn cursor_mask_bit(mask: &[u8], stride: usize, x: usize, y: usize) -> bool {
    mask[y * stride + x / 8] & (0x80 >> (x % 8)) != 0
}

fn push_masked_color(rgba: &mut Vec<u8>, color: [u8; 3], masked: bool, x: usize, y: usize) {
    if masked && color == [u8::MAX; 3] {
        // Destination inversion cannot be represented by static RGBA, so retain a visual fallback.
        push_invert_fallback(rgba, x, y);
    } else {
        rgba.extend_from_slice(&[
            color[0],
            color[1],
            color[2],
            if masked { 0 } else { u8::MAX },
        ]);
    }
}

fn push_invert_fallback(rgba: &mut Vec<u8>, x: usize, y: usize) {
    let dark = (x ^ y) & 1 != 0;
    let (shade, alpha) = if dark { (0x30, 0xc0) } else { (0x50, 0x30) };
    rgba.extend_from_slice(&[shade, shade, shade, alpha]);
}

fn require_empty(body: &[u8], context: &'static str) -> Result<(), ClientError> {
    if body.is_empty() {
        Ok(())
    } else {
        Err(protocol_value_error(context))
    }
}

fn resource_limit_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::ResourceLimit,
        0,
        context,
    )
    .into()
}

fn protocol_value_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::InvalidValue,
        0,
        context,
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_cursor_converts_bgra_to_rgba() {
        let budget = Arc::new(CursorBudget {
            used: AtomicUsize::new(0),
            maximum: MAX_CURSOR_CACHE_BYTES,
        });
        let shape = decode_shape(
            CursorHeader {
                unique_id: 9,
                cursor_type: CursorType::Alpha,
                width: 1,
                height: 1,
                hot_spot_x: 0,
                hot_spot_y: 0,
            },
            &[0x10, 0x20, 0x30, 0x40, 0xAA, 0xAA],
            &budget,
        )
        .expect("valid alpha cursor");

        assert_eq!(&*shape.rgba, &[191, 128, 64, 0x40]);
    }

    #[test]
    fn monochrome_cursor_preserves_and_xor_truth_table() {
        let rgba =
            decode_mono_cursor(&[0b1100_0000, 0b0110_0000], 4, 1).expect("valid monochrome masks");
        assert_eq!(&rgba[0..4], &[0, 0, 0, 0]);
        assert_eq!(&rgba[4..8], &[0x30, 0x30, 0x30, 0xc0]);
        assert_eq!(&rgba[8..12], &[255; 4]);
        assert_eq!(&rgba[12..16], &[0, 0, 0, 255]);
    }

    #[test]
    fn color4_cursor_uses_row_stride_and_high_nibble_first() {
        let mut data = vec![0x12, 0x30, 0x45, 0x60];
        for index in 0_u8..16 {
            data.extend_from_slice(&[index, 0, 0, 0]);
        }
        data.extend_from_slice(&[0, 0]);

        let rgba = decode_palette_cursor(&data, 3, 2, 4).expect("valid color4 cursor");
        let blue: Vec<_> = rgba.chunks_exact(4).map(|pixel| pixel[2]).collect();
        assert_eq!(blue, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn color24_cursor_converts_bgr_and_ignores_protocol_padding() {
        let rgba = decode_color24_cursor(&[0x10, 0x20, 0x30, 0, 0xAA], 1, 1)
            .expect("valid color24 cursor");
        assert_eq!(rgba, [0x30, 0x20, 0x10, 255]);
    }

    #[test]
    fn retained_shape_keeps_bytes_charged_after_cache_invalidation() {
        let budget = Arc::new(CursorBudget {
            used: AtomicUsize::new(0),
            maximum: 4,
        });
        let shape = Arc::new(
            decode_shape(
                CursorHeader {
                    unique_id: 1,
                    cursor_type: CursorType::Alpha,
                    width: 1,
                    height: 1,
                    hot_spot_x: 0,
                    hot_spot_y: 0,
                },
                &[0, 0, 0, 0],
                &budget,
            )
            .expect("first shape"),
        );
        let mut cache = CursorCache::new();
        cache.insert(shape.clone()).expect("cache shape");
        cache.clear();

        let error = budget
            .reserve(1)
            .expect_err("retained host shape keeps budget");
        assert_eq!(error.category(), crate::ErrorCategory::ResourceLimit);
        drop(shape);
        budget.reserve(4).expect("released shape returns budget");
    }
}
