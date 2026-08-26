#[cfg(feature = "video-h264")]
use openh264::formats::YUVSource;
#[cfg(feature = "video-h265")]
use rust_h265::PixelData;
#[cfg(feature = "video-vpx")]
use vpx_rs::DecodedImageData;
#[cfg(feature = "video-vpx")]
use vpx_rs::image::{ImageFormat, UVImagePlanes};

use crate::{DecodeLimits, decode_jpeg_with_cancel};

/// Video codecs negotiated by the SPICE Display channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiceVideoCodec {
    Mjpeg,
    Vp8,
    H264,
    Vp9,
    H265,
}

/// One decoded top-down frame in the client's canonical pixel layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedVideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Bounded video decoder failure.
#[derive(Debug, thiserror::Error)]
pub enum VideoDecodeError {
    #[error("video decoder initialization failed: {0}")]
    Initialization(String),
    #[error("video codec is not enabled in this build: {0}")]
    Unavailable(&'static str),
    #[error("video bitstream decode failed: {0}")]
    InvalidBitstream(String),
    #[error("video pixel layout is unsupported")]
    UnsupportedPixelLayout,
    #[error("decoded video frame exceeds configured limits")]
    ResourceLimit,
    #[error("video decode was cancelled")]
    Cancelled,
}

enum DecoderBackend {
    Mjpeg,
    #[cfg(feature = "video-vpx")]
    Vpx(vpx_rs::Decoder),
    #[cfg(feature = "video-h264")]
    H264(openh264::decoder::Decoder),
    #[cfg(feature = "video-h265")]
    H265(rust_h265::Decoder),
}

/// Stateful decoder owner isolated from protocol and async network code.
pub struct SpiceVideoDecoder {
    codec: SpiceVideoCodec,
    backend: DecoderBackend,
}

impl std::fmt::Debug for SpiceVideoDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpiceVideoDecoder")
            .field("codec", &self.codec)
            .finish_non_exhaustive()
    }
}

impl SpiceVideoDecoder {
    pub fn new(
        codec: SpiceVideoCodec,
        initial_width: u32,
        initial_height: u32,
    ) -> Result<Self, VideoDecodeError> {
        if initial_width == 0 || initial_height == 0 {
            return Err(VideoDecodeError::Initialization(
                "zero initial dimensions".to_owned(),
            ));
        }
        let backend = match codec {
            SpiceVideoCodec::Mjpeg => DecoderBackend::Mjpeg,
            #[cfg(feature = "video-vpx")]
            SpiceVideoCodec::Vp8 | SpiceVideoCodec::Vp9 => {
                let codec_id = match codec {
                    SpiceVideoCodec::Vp8 => vpx_rs::dec::CodecId::VP8,
                    SpiceVideoCodec::Vp9 => vpx_rs::dec::CodecId::VP9,
                    _ => unreachable!("matched VPx codecs"),
                };
                let configuration =
                    vpx_rs::DecoderConfig::new(codec_id, initial_width, initial_height);
                DecoderBackend::Vpx(
                    vpx_rs::Decoder::new(configuration)
                        .map_err(|error| VideoDecodeError::Initialization(error.to_string()))?,
                )
            }
            #[cfg(not(feature = "video-vpx"))]
            SpiceVideoCodec::Vp8 | SpiceVideoCodec::Vp9 => {
                return Err(VideoDecodeError::Unavailable("VP8/VP9"));
            }
            #[cfg(feature = "video-h264")]
            SpiceVideoCodec::H264 => DecoderBackend::H264(
                openh264::decoder::Decoder::new()
                    .map_err(|error| VideoDecodeError::Initialization(error.to_string()))?,
            ),
            #[cfg(not(feature = "video-h264"))]
            SpiceVideoCodec::H264 => return Err(VideoDecodeError::Unavailable("H.264")),
            #[cfg(feature = "video-h265")]
            SpiceVideoCodec::H265 => DecoderBackend::H265(rust_h265::Decoder::new()),
            #[cfg(not(feature = "video-h265"))]
            SpiceVideoCodec::H265 => return Err(VideoDecodeError::Unavailable("H.265")),
        };
        Ok(Self { codec, backend })
    }

    /// Decodes one complete SPICE stream packet and returns the newest presentation frame.
    pub fn decode<F>(
        &mut self,
        packet: &[u8],
        expected_width: u32,
        expected_height: u32,
        limits: DecodeLimits,
        cancelled: F,
    ) -> Result<Option<DecodedVideoFrame>, VideoDecodeError>
    where
        F: Fn() -> bool + Send + Sync + Clone + 'static,
    {
        if cancelled() {
            return Err(VideoDecodeError::Cancelled);
        }
        validate_frame_bound(expected_width, expected_height, limits)?;
        let frame = match &mut self.backend {
            DecoderBackend::Mjpeg => {
                let jpeg = decode_jpeg_with_cancel(
                    packet,
                    expected_width,
                    expected_height,
                    limits,
                    cancelled.clone(),
                )
                .map_err(|error| VideoDecodeError::InvalidBitstream(error.to_string()))?;
                Some(DecodedVideoFrame {
                    width: jpeg.width,
                    height: jpeg.height,
                    rgba: jpeg.pixels,
                })
            }
            #[cfg(feature = "video-h264")]
            DecoderBackend::H264(decoder) => decoder
                .decode(packet)
                .map_err(|error| VideoDecodeError::InvalidBitstream(error.to_string()))?
                .map(|decoded| {
                    let (width, height) = decoded.dimensions();
                    let dimensions = checked_dimensions(width, height, limits)?;
                    let mut rgba = vec![0; dimensions.2];
                    decoded.write_rgba8(&mut rgba);
                    Ok(DecodedVideoFrame {
                        width: dimensions.0,
                        height: dimensions.1,
                        rgba,
                    })
                })
                .transpose()?,
            #[cfg(feature = "video-vpx")]
            DecoderBackend::Vpx(decoder) => {
                let mut newest = None;
                for decoded in decoder
                    .decode(packet)
                    .map_err(|error| VideoDecodeError::InvalidBitstream(error.to_string()))?
                {
                    if cancelled() {
                        return Err(VideoDecodeError::Cancelled);
                    }
                    newest = Some(vpx_frame_to_rgba(&decoded, limits)?);
                }
                newest
            }
            #[cfg(feature = "video-h265")]
            DecoderBackend::H265(decoder) => {
                let mut newest = None;
                for nal in rust_h265::parse_annex_b(packet) {
                    if cancelled() {
                        return Err(VideoDecodeError::Cancelled);
                    }
                    if let Some(decoded) = decoder
                        .decode_nal(&nal)
                        .map_err(|error| VideoDecodeError::InvalidBitstream(error.to_string()))?
                    {
                        newest = Some(h265_frame_to_rgba(decoded, limits)?);
                    }
                }
                newest
            }
        };
        if let Some(frame) = &frame
            && (frame.width != expected_width || frame.height != expected_height)
        {
            return Err(VideoDecodeError::InvalidBitstream(
                "decoded dimensions differ from the SPICE frame".to_owned(),
            ));
        }
        Ok(frame)
    }
}

#[cfg(feature = "video-vpx")]
fn vpx_frame_to_rgba(
    decoded: &vpx_rs::DecodedImage,
    limits: DecodeLimits,
) -> Result<DecodedVideoFrame, VideoDecodeError> {
    match decoded.data() {
        DecodedImageData::Data8b(image) => {
            if !matches!(
                image.format(),
                ImageFormat::I420 | ImageFormat::YV12 | ImageFormat::NV12
            ) {
                return Err(VideoDecodeError::UnsupportedPixelLayout);
            }
            let dimensions = checked_dimensions(image.width(), image.height(), limits)?;
            let planes = image.planes();
            let y_stride = planes.y_stride();
            let mut rgba = vec![0; dimensions.2];
            match planes.uv {
                UVImagePlanes::Separate(chroma) => write_yuv420_u8(
                    dimensions.0 as usize,
                    dimensions.1 as usize,
                    planes.y,
                    y_stride,
                    chroma.u,
                    chroma.u_stride(),
                    chroma.v,
                    chroma.v_stride(),
                    &mut rgba,
                )?,
                UVImagePlanes::Interleaved(chroma) => write_nv12_u8(
                    dimensions.0 as usize,
                    dimensions.1 as usize,
                    planes.y,
                    y_stride,
                    chroma.uv,
                    chroma.uv_stride(),
                    &mut rgba,
                )?,
            }
            Ok(DecodedVideoFrame {
                width: dimensions.0,
                height: dimensions.1,
                rgba,
            })
        }
        DecodedImageData::Data16b(_) => Err(VideoDecodeError::UnsupportedPixelLayout),
    }
}

#[cfg(feature = "video-h265")]
fn h265_frame_to_rgba(
    frame: rust_h265::Frame,
    limits: DecodeLimits,
) -> Result<DecodedVideoFrame, VideoDecodeError> {
    let dimensions = checked_dimensions(frame.width as usize, frame.height as usize, limits)?;
    let mut rgba = vec![0; dimensions.2];
    match (&frame.y, &frame.u, &frame.v) {
        (PixelData::U8(y), PixelData::U8(u), PixelData::U8(v)) => write_yuv420_u8(
            dimensions.0 as usize,
            dimensions.1 as usize,
            y,
            dimensions.0 as usize,
            u,
            dimensions.0 as usize / 2,
            v,
            dimensions.0 as usize / 2,
            &mut rgba,
        )?,
        (PixelData::U16(y), PixelData::U16(u), PixelData::U16(v)) => write_yuv420_u16(
            dimensions.0 as usize,
            dimensions.1 as usize,
            frame.bit_depth,
            y,
            u,
            v,
            &mut rgba,
        )?,
        _ => return Err(VideoDecodeError::UnsupportedPixelLayout),
    }
    Ok(DecodedVideoFrame {
        width: dimensions.0,
        height: dimensions.1,
        rgba,
    })
}

#[cfg(any(feature = "video-h264", feature = "video-h265", feature = "video-vpx"))]
fn checked_dimensions(
    width: usize,
    height: usize,
    limits: DecodeLimits,
) -> Result<(u32, u32, usize), VideoDecodeError> {
    let width = u32::try_from(width).map_err(|_| VideoDecodeError::ResourceLimit)?;
    let height = u32::try_from(height).map_err(|_| VideoDecodeError::ResourceLimit)?;
    let bytes = validate_frame_bound(width, height, limits)?;
    Ok((width, height, bytes))
}

fn validate_frame_bound(
    width: u32,
    height: u32,
    limits: DecodeLimits,
) -> Result<usize, VideoDecodeError> {
    if width == 0 || height == 0 || width > limits.maximum_width || height > limits.maximum_height {
        return Err(VideoDecodeError::ResourceLimit);
    }
    let pixels = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or(VideoDecodeError::ResourceLimit)?;
    pixels
        .checked_mul(4)
        .filter(|bytes| *bytes <= limits.maximum_output_bytes)
        .ok_or(VideoDecodeError::ResourceLimit)
}

#[allow(clippy::too_many_arguments)]
#[cfg(any(feature = "video-vpx", feature = "video-h265"))]
fn write_yuv420_u8(
    width: usize,
    height: usize,
    y_plane: &[u8],
    y_stride: usize,
    u_plane: &[u8],
    u_stride: usize,
    v_plane: &[u8],
    v_stride: usize,
    rgba: &mut [u8],
) -> Result<(), VideoDecodeError> {
    for row in 0..height {
        for column in 0..width {
            let y =
                *y_plane
                    .get(row * y_stride + column)
                    .ok_or(VideoDecodeError::InvalidBitstream(
                        "short Y plane".to_owned(),
                    ))?;
            let u = *u_plane.get((row / 2) * u_stride + column / 2).ok_or(
                VideoDecodeError::InvalidBitstream("short U plane".to_owned()),
            )?;
            let v = *v_plane.get((row / 2) * v_stride + column / 2).ok_or(
                VideoDecodeError::InvalidBitstream("short V plane".to_owned()),
            )?;
            write_yuv_pixel(y, u, v, &mut rgba[(row * width + column) * 4..][..4]);
        }
    }
    Ok(())
}

#[cfg(feature = "video-vpx")]
fn write_nv12_u8(
    width: usize,
    height: usize,
    y_plane: &[u8],
    y_stride: usize,
    uv_plane: &[u8],
    uv_stride: usize,
    rgba: &mut [u8],
) -> Result<(), VideoDecodeError> {
    for row in 0..height {
        for column in 0..width {
            let y =
                *y_plane
                    .get(row * y_stride + column)
                    .ok_or(VideoDecodeError::InvalidBitstream(
                        "short Y plane".to_owned(),
                    ))?;
            let uv_offset = (row / 2) * uv_stride + (column / 2) * 2;
            let u = *uv_plane
                .get(uv_offset)
                .ok_or(VideoDecodeError::InvalidBitstream(
                    "short UV plane".to_owned(),
                ))?;
            let v = *uv_plane
                .get(uv_offset + 1)
                .ok_or(VideoDecodeError::InvalidBitstream(
                    "short UV plane".to_owned(),
                ))?;
            write_yuv_pixel(y, u, v, &mut rgba[(row * width + column) * 4..][..4]);
        }
    }
    Ok(())
}

#[cfg(feature = "video-h265")]
fn write_yuv420_u16(
    width: usize,
    height: usize,
    bit_depth: u8,
    y_plane: &[u16],
    u_plane: &[u16],
    v_plane: &[u16],
    rgba: &mut [u8],
) -> Result<(), VideoDecodeError> {
    if !(9..=16).contains(&bit_depth) {
        return Err(VideoDecodeError::UnsupportedPixelLayout);
    }
    let shift = bit_depth - 8;
    let chroma_width = width.div_ceil(2);
    for row in 0..height {
        for column in 0..width {
            let y = y_plane.get(row * width + column).copied().ok_or(
                VideoDecodeError::InvalidBitstream("short Y plane".to_owned()),
            )? >> shift;
            let chroma_offset = (row / 2) * chroma_width + column / 2;
            let u =
                u_plane
                    .get(chroma_offset)
                    .copied()
                    .ok_or(VideoDecodeError::InvalidBitstream(
                        "short U plane".to_owned(),
                    ))?
                    >> shift;
            let v =
                v_plane
                    .get(chroma_offset)
                    .copied()
                    .ok_or(VideoDecodeError::InvalidBitstream(
                        "short V plane".to_owned(),
                    ))?
                    >> shift;
            write_yuv_pixel(
                y as u8,
                u as u8,
                v as u8,
                &mut rgba[(row * width + column) * 4..][..4],
            );
        }
    }
    Ok(())
}

#[cfg(any(feature = "video-vpx", feature = "video-h265"))]
fn write_yuv_pixel(y: u8, u: u8, v: u8, rgba: &mut [u8]) {
    let luminance = (i32::from(y) - 16).max(0);
    let blue_difference = i32::from(u) - 128;
    let red_difference = i32::from(v) - 128;
    rgba[0] = clamp_u8((298 * luminance + 409 * red_difference + 128) >> 8);
    rgba[1] = clamp_u8((298 * luminance - 100 * blue_difference - 208 * red_difference + 128) >> 8);
    rgba[2] = clamp_u8((298 * luminance + 516 * blue_difference + 128) >> 8);
    rgba[3] = u8::MAX;
}

#[cfg(any(feature = "video-vpx", feature = "video-h265"))]
fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, i32::from(u8::MAX)) as u8
}
