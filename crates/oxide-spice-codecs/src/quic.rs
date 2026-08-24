//! Decoder for the SPICE QUIC image codec.

use thiserror::Error;

use crate::DecodeLimits;

const QUIC_MAGIC: u32 = 0x4349_5551;
const QUIC_VERSION: u32 = 0;
const HEADER_WORDS: usize = 5;
const MAX_CODE_BITS: usize = 26;
const INITIAL_CODE_INDEX: usize = 0;
const MAX_CODE_INDEX: usize = 6;
const CODE_INDEX_PIXELS: usize = 2048;
const MODEL_CODE_COUNT: usize = 8;
const RUN_STATE_COUNT: usize = 32;
const CANCELLATION_INTERVAL_PIXELS: usize = 4096;

const RUN_LENGTH_BITS: [usize; RUN_STATE_COUNT] = [
    0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 9, 10, 11, 12, 13,
    14, 15,
];

// This fixed sequence is part of the codec's model-update schedule.
const MODEL_RANDOM: [u32; 256] = [
    0x02c57542, 0x35427717, 0x2f5a2153, 0x9244f155, 0x7bd26d07, 0x354c6052, 0x57329b28, 0x2993868e,
    0x6cd8808c, 0x147b46e0, 0x99db66af, 0xe32b4cac, 0x1b671264, 0x9d433486, 0x62a4c192, 0x06089a4b,
    0x9e3dce44, 0xdaabee13, 0x222425ea, 0xa46f331d, 0xcd589250, 0x8bb81d7f, 0xc8b736b9, 0x35948d33,
    0xd7ac7fd0, 0x5fbe2803, 0x2cfbc105, 0x013dbc4e, 0x7a37820f, 0x39f88e9e, 0xedd58794, 0xc5076689,
    0xfcada5a4, 0x64c2f46d, 0xb3ba3243, 0x8974b4f9, 0x5a05aebd, 0x20afcd00, 0x39e2b008, 0x88a18a45,
    0x600bde29, 0xf3971ace, 0xf37b0a6b, 0x7041495b, 0x70b707ab, 0x06beffbb, 0x4206051f, 0xe13c4ee3,
    0xc1a78327, 0x91aa067c, 0x8295f72a, 0x732917a6, 0x1d871b4d, 0x4048f136, 0xf1840e7e, 0x6a6048c1,
    0x696cb71a, 0x7ff501c3, 0x0fc6310b, 0x57e0f83d, 0x8cc26e74, 0x11a525a2, 0x946934c7, 0x7cd888f0,
    0x8f9d8604, 0x4f86e73b, 0x04520316, 0xdeeea20c, 0xf1def496, 0x67687288, 0xf540c5b2, 0x22401484,
    0x3478658a, 0xc2385746, 0x01979c2c, 0x5dad73c8, 0x0321f58b, 0xf0fedbee, 0x92826ddf, 0x284bec73,
    0x5b1a1975, 0x03df1e11, 0x20963e01, 0xa17cf12b, 0x740d776e, 0xa7a6bf3c, 0x01b5cce4, 0x1118aa76,
    0xfc6fac0a, 0xce927e9b, 0x00bf2567, 0x806f216c, 0xbca69056, 0x795bd3e9, 0xc9dc4557, 0x8929b6c2,
    0x789d52ec, 0x3f3fbf40, 0xb9197368, 0xa38c15b5, 0xc3b44fa8, 0xca8333b0, 0xb7e8d590, 0xbe807feb,
    0xbf5f8360, 0xd99e2f5c, 0x372928e1, 0x7c757c4c, 0x0db5b154, 0xc01ede02, 0x1fc86e78, 0x1f3985be,
    0xb4805c77, 0x00c880fa, 0x974c1b12, 0x35ab0214, 0xb2dc840d, 0x5b00ae37, 0xd313b026, 0xb260969d,
    0x7f4c8879, 0x1734c4d3, 0x49068631, 0xb9f6a021, 0x6b863e6f, 0xcee5debf, 0x29f8c9fb, 0x53dd6880,
    0x72b61223, 0x1f67a9fd, 0x0a0f6993, 0x13e59119, 0x11cca12e, 0xfe6b6766, 0x16b6effc, 0x97918fc4,
    0xc2b8a563, 0x94f2f741, 0x0bfa8c9a, 0xd1537ae8, 0xc1da349c, 0x873c60ca, 0x95005b85, 0x9b5c080e,
    0xbc8abbd9, 0xe1eab1d2, 0x6dac9070, 0x4ea9ebf1, 0xe0cf30d4, 0x1ef5bd7b, 0xd161043e, 0x5d2fa2e2,
    0xff5d3cae, 0x86ed9f87, 0x2aa1daa1, 0xbd731a34, 0x9e8f4b22, 0xb1c2c67a, 0xc21758c9, 0xa182215d,
    0xccb01948, 0x8d168df7, 0x04238cfe, 0x368c3dbc, 0x0aeadca5, 0xbad21c24, 0x0a71fee5, 0x9fc5d872,
    0x54c152c6, 0xfc329483, 0x6783384a, 0xeddb3e1c, 0x65f90e30, 0x884ad098, 0xce81675a, 0x4b372f7d,
    0x68bf9a39, 0x43445f1e, 0x40f8d8cb, 0x90d5acb6, 0x4cd07282, 0x349eeb06, 0x0c9d5332, 0x520b24ef,
    0x80020447, 0x67976491, 0x2f931ca3, 0xfe9b0535, 0xfcd30220, 0x61a9e6cc, 0xa487d8d7, 0x3f7c5dd1,
    0x7d0127c5, 0x48f51d15, 0x60dea871, 0xc9a91cb7, 0x58b53bb3, 0x9d5e0b2d, 0x624a78b4, 0x30dbee1b,
    0x9bdf22e7, 0x1df5c299, 0x2d5643a7, 0xf4dd35ff, 0x03ca8fd6, 0x53b47ed8, 0x6f2c19aa, 0xfeb0c1f4,
    0x49e54438, 0x2f2577e6, 0xbf876969, 0x72440ea9, 0xfa0bafb8, 0x74f5b3a0, 0x7dd357cd, 0x89ce1358,
    0x6ef2cdda, 0x1e7767f3, 0xa6be9fdb, 0x4f5f88f8, 0xba994a3a, 0x08ca6b65, 0xe0893818, 0x9e00a16a,
    0xf42bfc8f, 0x9972eedc, 0x749c8b51, 0x32c05f5e, 0xd706805f, 0x6bfbb7cf, 0xd9210a10, 0x31a1db97,
    0x923a9559, 0x37a7a1f6, 0x059f8861, 0xca493e62, 0x65157e81, 0x8f6467dd, 0xab85ff9f, 0x9331aff2,
    0x8616b9f5, 0xedbd5695, 0xee7e29b1, 0x313ac44f, 0xb903112f, 0x432ef649, 0xdc0a36c0, 0x61cf2bba,
    0x81474925, 0xa8b6c7ad, 0xee5931de, 0xb2f8158d, 0x59fb7409, 0x2e3dfaed, 0x9af25a3f, 0xe1fed4d5,
];

/// Pixel formats declared by the QUIC stream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QuicImageType {
    Gray = 1,
    Rgb16 = 2,
    Rgb24 = 3,
    Rgb32 = 4,
    Rgba = 5,
}

impl TryFrom<u32> for QuicImageType {
    type Error = QuicError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Gray),
            2 => Ok(Self::Rgb16),
            3 => Ok(Self::Rgb24),
            4 => Ok(Self::Rgb32),
            5 => Ok(Self::Rgba),
            _ => Err(QuicError::new(QuicErrorKind::UnsupportedType, "image type")),
        }
    }
}

/// One top-down QUIC image converted to RGBA pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedQuicImage {
    pub width: u32,
    pub height: u32,
    pub image_type: QuicImageType,
    pub pixels: Vec<[u8; 4]>,
}

/// Stable categories for malformed, unsupported, or cancelled QUIC data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuicErrorKind {
    Truncated,
    InvalidHeader,
    UnsupportedType,
    DimensionMismatch,
    InvalidCode,
    InvalidRun,
    ResourceLimit,
    Cancelled,
}

/// A QUIC failure that does not retain peer-controlled bytes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("SPICE QUIC {context}: {kind:?}")]
pub struct QuicError {
    pub kind: QuicErrorKind,
    pub context: &'static str,
}

impl QuicError {
    const fn new(kind: QuicErrorKind, context: &'static str) -> Self {
        Self { kind, context }
    }
}

#[derive(Debug)]
struct BitReader<'a> {
    input: &'a [u8],
    bit_offset: usize,
}

impl<'a> BitReader<'a> {
    fn new(input: &'a [u8]) -> Result<Self, QuicError> {
        if input.len() < HEADER_WORDS * size_of::<u32>() || !input.len().is_multiple_of(4) {
            return Err(QuicError::new(QuicErrorKind::Truncated, "word stream"));
        }
        Ok(Self {
            input,
            bit_offset: 0,
        })
    }

    fn read_u32(&mut self) -> Result<u32, QuicError> {
        let high = self.read_bits(16)?;
        let low = self.read_bits(16)?;
        Ok((high << 16) | low)
    }

    fn read_bit(&mut self) -> Result<u32, QuicError> {
        self.read_bits(1)
    }

    fn read_bits(&mut self, count: usize) -> Result<u32, QuicError> {
        if count > 31 {
            return Err(QuicError::new(QuicErrorKind::InvalidCode, "bit count"));
        }
        let end = self
            .bit_offset
            .checked_add(count)
            .filter(|end| *end <= self.input.len() * 8)
            .ok_or_else(|| QuicError::new(QuicErrorKind::Truncated, "bit stream"))?;
        let mut output = 0_u32;
        while self.bit_offset < end {
            let word_offset = self.bit_offset / 32 * 4;
            let word = u32::from_le_bytes(
                self.input[word_offset..word_offset + 4]
                    .try_into()
                    .expect("validated QUIC word bounds"),
            );
            let offset_in_word = self.bit_offset % 32;
            let available = 32 - offset_in_word;
            let take = available.min(end - self.bit_offset);
            let shift = available - take;
            let mask = if take == 32 {
                u32::MAX
            } else {
                (1_u32 << take) - 1
            };
            output = (output << take) | ((word >> shift) & mask);
            self.bit_offset += take;
        }
        Ok(output)
    }
}

#[derive(Debug)]
struct Bucket {
    counters: [u32; MODEL_CODE_COUNT],
    best_code: usize,
}

#[derive(Debug)]
struct ChannelModel {
    residuals: Vec<u8>,
    prior_context: u8,
    context_buckets: Vec<usize>,
    buckets: Vec<Bucket>,
}

impl ChannelModel {
    fn new(width: usize, bits_per_component: usize) -> Self {
        let levels = 1_usize << bits_per_component;
        let bucket_count = bits_per_component;
        let context_buckets = (0..levels)
            .map(|context| {
                if context == 0 {
                    0
                } else {
                    (usize::BITS as usize - (context + 1).leading_zeros() as usize - 1)
                        .min(bucket_count - 1)
                }
            })
            .collect();
        let buckets = (0..bucket_count)
            .map(|_| Bucket {
                counters: [0; MODEL_CODE_COUNT],
                best_code: bits_per_component - 1,
            })
            .collect();
        Self {
            residuals: vec![0; width],
            prior_context: 0,
            context_buckets,
            buckets,
        }
    }

    fn start_next_row(&mut self) {
        self.prior_context = self.residuals[0];
    }

    fn best_code(&self, context: u8) -> usize {
        self.buckets[self.context_buckets[usize::from(context)]].best_code
    }

    fn update(&mut self, context: u8, value: u8, bits_per_component: usize, trigger: u32) {
        let bucket = &mut self.buckets[self.context_buckets[usize::from(context)]];
        let mut best_code = bits_per_component - 1;
        bucket.counters[best_code] += golomb_code_length(value, best_code, bits_per_component);
        let mut best_length = bucket.counters[best_code];
        for code in (0..best_code).rev() {
            bucket.counters[code] += golomb_code_length(value, code, bits_per_component);
            if bucket.counters[code] < best_length {
                best_code = code;
                best_length = bucket.counters[code];
            }
        }
        bucket.best_code = best_code;
        if best_length > trigger {
            for counter in &mut bucket.counters[..bits_per_component] {
                *counter >>= 1;
            }
        }
    }
}

#[derive(Debug)]
struct CodingState {
    wait_count: usize,
    random_index: u8,
    code_index: usize,
    pixels_until_code_index: usize,
    model_trigger: u32,
    run_state: usize,
}

impl CodingState {
    fn new() -> Self {
        let mut state = Self {
            wait_count: 0,
            random_index: u8::MAX,
            code_index: INITIAL_CODE_INDEX,
            pixels_until_code_index: CODE_INDEX_PIXELS,
            model_trigger: 0,
            run_state: 0,
        };
        state.update_model_trigger();
        state
    }

    fn next_wait(&mut self) -> usize {
        self.random_index = self.random_index.wrapping_add(1);
        MODEL_RANDOM[usize::from(self.random_index)] as usize & ((1 << self.code_index) - 1)
    }

    fn update_model_trigger(&mut self) {
        const EVOLUTION_THREE_TRIGGERS: [u32; 11] =
            [110, 550, 900, 800, 550, 400, 350, 250, 140, 160, 140];
        self.model_trigger = EVOLUTION_THREE_TRIGGERS[self.code_index.min(10)];
    }

    fn increase_code_index(&mut self) {
        self.code_index += 1;
        self.pixels_until_code_index = CODE_INDEX_PIXELS;
        self.update_model_trigger();
    }
}

/// Decodes one complete SPICE QUIC payload into bounded RGBA storage.
pub fn decode_quic_with_cancel(
    input: &[u8],
    expected_width: u32,
    expected_height: u32,
    limits: DecodeLimits,
    mut should_cancel: impl FnMut() -> bool,
) -> Result<DecodedQuicImage, QuicError> {
    if should_cancel() {
        return Err(QuicError::new(QuicErrorKind::Cancelled, "decode"));
    }
    let mut reader = BitReader::new(input)?;
    if reader.read_u32()? != QUIC_MAGIC || reader.read_u32()? != QUIC_VERSION {
        return Err(QuicError::new(
            QuicErrorKind::InvalidHeader,
            "magic or version",
        ));
    }
    let image_type = QuicImageType::try_from(reader.read_u32()?)?;
    if image_type == QuicImageType::Gray {
        return Err(QuicError::new(
            QuicErrorKind::UnsupportedType,
            "grayscale display image",
        ));
    }
    let width = reader.read_u32()?;
    let height = reader.read_u32()?;
    if width != expected_width || height != expected_height {
        return Err(QuicError::new(
            QuicErrorKind::DimensionMismatch,
            "image descriptor dimensions",
        ));
    }
    if width == 0 || height == 0 || width > limits.maximum_width || height > limits.maximum_height {
        return Err(QuicError::new(QuicErrorKind::ResourceLimit, "dimensions"));
    }
    let width_usize = usize::try_from(width)
        .map_err(|_| QuicError::new(QuicErrorKind::ResourceLimit, "width"))?;
    let height_usize = usize::try_from(height)
        .map_err(|_| QuicError::new(QuicErrorKind::ResourceLimit, "height"))?;
    let pixel_count = width_usize
        .checked_mul(height_usize)
        .ok_or_else(|| QuicError::new(QuicErrorKind::ResourceLimit, "pixel count"))?;
    let output_bytes = pixel_count
        .checked_mul(4)
        .ok_or_else(|| QuicError::new(QuicErrorKind::ResourceLimit, "output size"))?;
    if output_bytes > limits.maximum_output_bytes {
        return Err(QuicError::new(QuicErrorKind::ResourceLimit, "output size"));
    }

    let bits_per_component = if image_type == QuicImageType::Rgb16 {
        5
    } else {
        8
    };
    let channel_count = if image_type == QuicImageType::Rgba {
        4
    } else {
        3
    };
    let mut channels: Vec<_> = (0..channel_count)
        .map(|_| ChannelModel::new(width_usize, bits_per_component))
        .collect();
    let mut rgb_state = CodingState::new();
    let mut alpha_state = CodingState::new();
    let mut pixels = vec![[0_u8, 0, 0, u8::MAX]; pixel_count];

    for row in 0..height_usize {
        if row.is_multiple_of(CANCELLATION_INTERVAL_PIXELS.div_ceil(width_usize)) && should_cancel()
        {
            return Err(QuicError::new(QuicErrorKind::Cancelled, "decode"));
        }
        if row > 0 {
            for channel in &mut channels {
                channel.start_next_row();
            }
        }
        let row_start = row * width_usize;
        let (previous_rows, current_and_later) = pixels.split_at_mut(row_start);
        let current = &mut current_and_later[..width_usize];
        let previous = (row > 0).then(|| &previous_rows[row_start - width_usize..row_start]);
        decode_row(
            &mut reader,
            &mut rgb_state,
            &mut channels[..3],
            &[0, 1, 2],
            previous,
            current,
            bits_per_component,
        )?;
        if image_type == QuicImageType::Rgba {
            decode_row(
                &mut reader,
                &mut alpha_state,
                &mut channels[3..4],
                &[3],
                previous,
                current,
                8,
            )?;
        }
    }
    if should_cancel() {
        return Err(QuicError::new(QuicErrorKind::Cancelled, "decode"));
    }

    for pixel in &mut pixels {
        if image_type == QuicImageType::Rgb16 {
            for component in &mut pixel[..3] {
                *component = (*component << 3) | (*component >> 2);
            }
        }
    }
    Ok(DecodedQuicImage {
        width,
        height,
        image_type,
        pixels,
    })
}

fn decode_row(
    reader: &mut BitReader<'_>,
    state: &mut CodingState,
    channels: &mut [ChannelModel],
    components: &[usize],
    previous: Option<&[[u8; 4]]>,
    current: &mut [[u8; 4]],
    bits_per_component: usize,
) -> Result<(), QuicError> {
    let mut position = 0_usize;
    let mut remaining = current.len();
    while state.code_index < MAX_CODE_INDEX && state.pixels_until_code_index <= remaining {
        if state.pixels_until_code_index != 0 {
            let segment_end = position + state.pixels_until_code_index;
            decode_row_segment(
                reader,
                state,
                channels,
                components,
                previous,
                current,
                position,
                segment_end,
                bits_per_component,
            )?;
            position = segment_end;
            remaining -= state.pixels_until_code_index;
        }
        state.increase_code_index();
    }
    if remaining != 0 {
        decode_row_segment(
            reader,
            state,
            channels,
            components,
            previous,
            current,
            position,
            position + remaining,
            bits_per_component,
        )?;
        if state.code_index < MAX_CODE_INDEX {
            state.pixels_until_code_index -= remaining;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_row_segment(
    reader: &mut BitReader<'_>,
    state: &mut CodingState,
    channels: &mut [ChannelModel],
    components: &[usize],
    previous: Option<&[[u8; 4]]>,
    current: &mut [[u8; 4]],
    start: usize,
    end: usize,
    bits_per_component: usize,
) -> Result<(), QuicError> {
    let mut position = start;
    let mut run_index = 0_usize;
    let wait_mask = (1 << state.code_index) - 1;
    let mut stop_index;
    if position == 0 {
        decode_pixel(
            reader,
            channels,
            components,
            previous,
            current,
            position,
            bits_per_component,
        )?;
        if state.wait_count != 0 {
            state.wait_count -= 1;
        } else {
            state.wait_count = state.next_wait() & wait_mask;
            update_models(channels, position, bits_per_component, state.model_trigger);
        }
        position += 1;
        stop_index = position + state.wait_count;
    } else {
        stop_index = position + state.wait_count;
    }

    loop {
        let decode_end = end.min(stop_index.saturating_add(1));
        let mut decoded_run = false;
        while position < decode_end {
            if should_decode_run(previous, current, components, position, run_index) {
                state.wait_count = stop_index - position;
                run_index = position;
                let run_length = decode_run(reader, state, end - position)?;
                let source = current[position - 1];
                for pixel in &mut current[position..position + run_length] {
                    for component in components {
                        pixel[*component] = source[*component];
                    }
                }
                position += run_length;
                if position == end {
                    return Ok(());
                }
                stop_index = position + state.wait_count;
                decoded_run = true;
                break;
            }
            decode_pixel(
                reader,
                channels,
                components,
                previous,
                current,
                position,
                bits_per_component,
            )?;
            position += 1;
        }
        if decoded_run {
            continue;
        }
        if stop_index < end {
            update_models(
                channels,
                stop_index,
                bits_per_component,
                state.model_trigger,
            );
            stop_index = position + (state.next_wait() & wait_mask);
        } else {
            state.wait_count = stop_index - end;
            return Ok(());
        }
    }
}

fn decode_pixel(
    reader: &mut BitReader<'_>,
    channels: &mut [ChannelModel],
    components: &[usize],
    previous: Option<&[[u8; 4]]>,
    current: &mut [[u8; 4]],
    position: usize,
    bits_per_component: usize,
) -> Result<(), QuicError> {
    let component_mask = (1_u16 << bits_per_component) - 1;
    for (channel, component) in channels.iter_mut().zip(components) {
        let context = if position == 0 {
            channel.prior_context
        } else {
            channel.residuals[position - 1]
        };
        let residual = decode_golomb(reader, channel.best_code(context), bits_per_component)?;
        channel.residuals[position] = residual;
        let predictor = match (previous, position) {
            (None, 0) => 0,
            (None, _) => u16::from(current[position - 1][*component]),
            (Some(previous), 0) => u16::from(previous[0][*component]),
            (Some(previous), _) => {
                (u16::from(current[position - 1][*component])
                    + u16::from(previous[position][*component]))
                    >> 1
            }
        };
        let unfolded = if residual & 1 == 0 {
            u16::from(residual >> 1)
        } else {
            component_mask - u16::from(residual >> 1)
        };
        current[position][*component] = ((predictor + unfolded) & component_mask) as u8;
    }
    Ok(())
}

fn update_models(
    channels: &mut [ChannelModel],
    position: usize,
    bits_per_component: usize,
    trigger: u32,
) {
    for channel in channels {
        let context = if position == 0 {
            channel.prior_context
        } else {
            channel.residuals[position - 1]
        };
        channel.update(
            context,
            channel.residuals[position],
            bits_per_component,
            trigger,
        );
    }
}

fn should_decode_run(
    previous: Option<&[[u8; 4]]>,
    current: &[[u8; 4]],
    components: &[usize],
    position: usize,
    run_index: usize,
) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    position > 2
        && run_index != position
        && same_components(previous[position - 1], previous[position], components)
        && same_components(current[position - 1], current[position - 2], components)
}

fn same_components(left: [u8; 4], right: [u8; 4], components: &[usize]) -> bool {
    components
        .iter()
        .all(|component| left[*component] == right[*component])
}

fn decode_run(
    reader: &mut BitReader<'_>,
    state: &mut CodingState,
    maximum_length: usize,
) -> Result<usize, QuicError> {
    let mut length = 0_usize;
    while reader.read_bit()? == 1 {
        let run_bits = RUN_LENGTH_BITS[state.run_state];
        length = length
            .checked_add(1_usize << run_bits)
            .filter(|length| *length <= maximum_length)
            .ok_or_else(|| QuicError::new(QuicErrorKind::InvalidRun, "run length"))?;
        if state.run_state + 1 < RUN_STATE_COUNT {
            state.run_state += 1;
        }
    }
    let remainder_bits = RUN_LENGTH_BITS[state.run_state];
    if remainder_bits != 0 {
        length = length
            .checked_add(reader.read_bits(remainder_bits)? as usize)
            .filter(|length| *length <= maximum_length)
            .ok_or_else(|| QuicError::new(QuicErrorKind::InvalidRun, "run remainder"))?;
    }
    state.run_state = state.run_state.saturating_sub(1);
    Ok(length)
}

fn decode_golomb(
    reader: &mut BitReader<'_>,
    code: usize,
    bits_per_component: usize,
) -> Result<u8, QuicError> {
    let maximum_value = (1_usize << bits_per_component) - 1;
    let maximum_prefix =
        (MAX_CODE_BITS - bits_per_component).min((1_usize << (bits_per_component - code)) - 1);
    let first_escape_value = maximum_prefix << code;
    let mut zero_prefix = 0_usize;
    while zero_prefix < maximum_prefix && reader.read_bit()? == 0 {
        zero_prefix += 1;
    }
    let value = if zero_prefix < maximum_prefix {
        (zero_prefix << code) | reader.read_bits(code)? as usize
    } else {
        let escape_values = (maximum_value + 1) - first_escape_value;
        let suffix_bits = ceil_log2(escape_values);
        first_escape_value + reader.read_bits(suffix_bits)? as usize
    };
    u8::try_from(value)
        .ok()
        .filter(|value| usize::from(*value) <= maximum_value)
        .ok_or_else(|| QuicError::new(QuicErrorKind::InvalidCode, "Golomb value"))
}

fn golomb_code_length(value: u8, code: usize, bits_per_component: usize) -> u32 {
    let maximum_prefix =
        (MAX_CODE_BITS - bits_per_component).min((1_usize << (bits_per_component - code)) - 1);
    let first_escape_value = maximum_prefix << code;
    if usize::from(value) < first_escape_value {
        u32::try_from((usize::from(value) >> code) + code + 1).expect("bounded Golomb code length")
    } else {
        let escape_values = (1_usize << bits_per_component) - first_escape_value;
        u32::try_from(maximum_prefix + ceil_log2(escape_values))
            .expect("bounded Golomb escape length")
    }
}

fn ceil_log2(value: usize) -> usize {
    if value <= 1 {
        0
    } else {
        usize::BITS as usize - (value - 1).leading_zeros() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated by the spice-common encoder at commit 71e45706981973014eaab3d4b533d35d79e19ffa.
    fn decode_hex(encoded: &str) -> Vec<u8> {
        assert!(encoded.len().is_multiple_of(2));
        (0..encoded.len())
            .step_by(2)
            .map(|offset| {
                u8::from_str_radix(&encoded[offset..offset + 2], 16).expect("fixture hex byte")
            })
            .collect()
    }

    fn pixels(bytes: &[[u8; 4]]) -> Vec<[u8; 4]> {
        bytes.to_vec()
    }

    #[test]
    fn official_rgb32_and_rgba_vectors_decode_in_component_order() {
        let rgb = decode_hex("515549430000000004000000010000000100000000e6c4a200000000");
        let decoded = decode_quic_with_cancel(&rgb, 1, 1, DecodeLimits::DISPLAY, || false)
            .expect("one RGB32 pixel");
        assert_eq!(decoded.pixels, [[0x11, 0x22, 0x33, 0xff]]);

        let rgba = decode_hex(
            "515549430000000005000000020000000200000000808080008003000100e0020000003a042000ec0606060200f0f70700000000",
        );
        let decoded = decode_quic_with_cancel(&rgba, 2, 2, DecodeLimits::DISPLAY, || false)
            .expect("official RGBA vector");
        assert_eq!(
            decoded.pixels,
            pixels(&[
                [0x00, 0x00, 0x00, 0x00],
                [0x10, 0x20, 0x30, 0x7f],
                [0x50, 0x60, 0x70, 0x80],
                [0xa0, 0xb0, 0xc0, 0xff],
            ])
        );
    }

    #[test]
    fn official_cross_row_prediction_and_mel_run_vectors_match_pixels() {
        let cross_row = decode_hex(
            "51554943000000000300000004000000030000009482848694949494224dd394c6c8288a7346464608ed9c9c8c31c6880000006000000000",
        );
        let decoded = decode_quic_with_cancel(&cross_row, 4, 3, DecodeLimits::DISPLAY, || false)
            .expect("official cross-row vector");
        assert_eq!(
            decoded.pixels,
            pixels(&[
                [3, 2, 1, 255],
                [13, 12, 11, 255],
                [23, 22, 21, 255],
                [33, 32, 31, 255],
                [4, 3, 2, 255],
                [14, 13, 12, 255],
                [24, 23, 22, 255],
                [34, 33, 32, 255],
                [2, 1, 0, 255],
                [12, 11, 10, 255],
                [22, 21, 20, 255],
                [32, 31, 30, 255],
            ])
        );

        let run = decode_hex(
            "515549430000000004000000100000000200000080e6c4a2018180804420080146428820ffff9f730020ffff00000000",
        );
        let decoded = decode_quic_with_cancel(&run, 16, 2, DecodeLimits::DISPLAY, || false)
            .expect("official MEL run vector");
        assert_eq!(decoded.pixels, vec![[0x11, 0x22, 0x33, 0xff]; 32]);

        let rgb16 = decode_hex(
            "51554943000000000200000005000000030000000004208406000008dd7837de8d151be334461a23a21a698c4863a4b0208d91c600000000",
        );
        let decoded = decode_quic_with_cancel(&rgb16, 5, 3, DecodeLimits::DISPLAY, || false)
            .expect("official RGB16 cross-row vector");
        let mut expected = Vec::new();
        for y in 0_u8..3 {
            for x in 0_u8..5 {
                let red = (3 * x + 5 * y) & 31;
                let green = (7 * x + 2 * y) & 31;
                let blue = (11 * x + 13 * y) & 31;
                expected.push([
                    (red << 3) | (red >> 2),
                    (green << 3) | (green >> 2),
                    (blue << 3) | (blue >> 2),
                    255,
                ]);
            }
        }
        assert_eq!(decoded.pixels, expected);
    }

    #[test]
    fn header_limits_and_cancellation_precede_image_allocation() {
        let stream = decode_hex("515549430000000004000000010000000100000000e6c4a200000000");
        let mismatch = decode_quic_with_cancel(&stream, 2, 1, DecodeLimits::DISPLAY, || false)
            .expect_err("descriptor mismatch");
        assert_eq!(mismatch.kind, QuicErrorKind::DimensionMismatch);

        let cancelled = decode_quic_with_cancel(&stream, 1, 1, DecodeLimits::DISPLAY, || true)
            .expect_err("cancel before allocation");
        assert_eq!(cancelled.kind, QuicErrorKind::Cancelled);

        let mut oversized = stream;
        oversized[12..16].copy_from_slice(&(limits_width() + 1).to_le_bytes());
        let error = decode_quic_with_cancel(
            &oversized,
            limits_width() + 1,
            1,
            DecodeLimits::DISPLAY,
            || false,
        )
        .expect_err("oversized dimensions");
        assert_eq!(error.kind, QuicErrorKind::ResourceLimit);
    }

    const fn limits_width() -> u32 {
        DecodeLimits::DISPLAY.maximum_width
    }
}
