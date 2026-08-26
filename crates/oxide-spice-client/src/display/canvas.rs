use std::collections::HashMap;

use oxide_spice_protocol::{
    CompositeClip, CompositeSurface, DisplayBrush, DisplayImageSource, DisplayMask, DrawAlphaBlend,
    DrawCopy, DrawFill, DrawMaskedDestination, DrawOpaque, DrawRop3, DrawStroke, DrawText,
    DrawTransparent, FixedPoint, GlyphFormat, Rect, SurfaceFormat,
};

use super::{
    Bounds, OwnedCompositePixels, PreparedCompositeImage, SurfaceData, SurfaceHandle,
    image_rect_bounds, protocol_value_error, resource_limit_error, surface_for_update,
    unsupported_wire,
};
use crate::ClientError;

const ROP_INVERT_SOURCE: u16 = 1 << 0;
const ROP_INVERT_BRUSH: u16 = 1 << 1;
const ROP_INVERT_DESTINATION: u16 = 1 << 2;
const ROP_PUT: u16 = 1 << 3;
const ROP_OR: u16 = 1 << 4;
const ROP_AND: u16 = 1 << 5;
const ROP_XOR: u16 = 1 << 6;
const ROP_BLACKNESS: u16 = 1 << 7;
const ROP_WHITENESS: u16 = 1 << 8;
const ROP_INVERT: u16 = 1 << 9;
const ROP_INVERT_RESULT: u16 = 1 << 10;
const KNOWN_ROP_BITS: u16 = (1 << 11) - 1;

#[derive(Debug, Clone, Copy)]
enum RopInput {
    Source,
    Brush,
    Destination,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum MaskedDestinationOperation {
    Blackness,
    Whiteness,
    Invert,
}

struct SampledPixels {
    format: SurfaceFormat,
    pixels: Vec<u32>,
}

const MAX_STROKE_CURVE_DEPTH: u8 = 10;
const STROKE_FLATNESS_PIXELS: f64 = 0.25;
const STROKE_WORK_MULTIPLIER: usize = 8;

/// Renders one classic Fill command through shared brush, clip, mask, and ROP semantics.
pub(super) async fn render_fill(
    surfaces: &HashMap<u32, SurfaceHandle>,
    command: &DrawFill,
    pattern: Option<&PreparedCompositeImage>,
    mask: Option<&PreparedCompositeImage>,
) -> Result<SurfaceHandle, ClientError> {
    validate_rop_descriptor(command.rop_descriptor)?;
    let destination_handle = surface_for_update(surfaces, command.base.surface_id)?;
    let destination_metadata = destination_handle.inner.read().await;
    let destination_bounds = destination_metadata.rect_bounds(command.base.destination)?;
    let destination_format = destination_metadata.format;
    drop(destination_metadata);
    let brush = sample_brush(
        surfaces,
        command.brush,
        pattern,
        command.base.destination,
        destination_bounds,
        destination_format,
    )
    .await?;
    let mask = sample_mask(surfaces, command.mask, mask, destination_bounds).await?;
    let mut destination = destination_handle.inner.write().await;
    apply_pixels(
        &mut destination,
        destination_bounds,
        &command.base.clip,
        mask.as_deref(),
        |index, old| {
            apply_binary_rop(
                command.rop_descriptor,
                brush.pixels[index],
                old,
                RopInput::Brush,
                RopInput::Destination,
            )
        },
    )?;
    drop(destination);
    Ok(destination_handle)
}

/// Renders Draw Copy and the wire-identical Draw Blend command.
pub(super) async fn render_copy(
    surfaces: &HashMap<u32, SurfaceHandle>,
    command: &DrawCopy,
    source: &PreparedCompositeImage,
    mask: Option<&PreparedCompositeImage>,
) -> Result<SurfaceHandle, ClientError> {
    validate_rop_descriptor(command.rop_descriptor)?;
    let destination_handle = surface_for_update(surfaces, command.base.surface_id)?;
    let destination = destination_handle.inner.read().await;
    let destination_bounds = destination.rect_bounds(command.base.destination)?;
    drop(destination);
    let source = sample_source(
        surfaces,
        source,
        command.source,
        destination_bounds.width,
        destination_bounds.height,
    )
    .await?;
    let mask = sample_mask(surfaces, command.mask, mask, destination_bounds).await?;
    let mut destination = destination_handle.inner.write().await;
    apply_pixels(
        &mut destination,
        destination_bounds,
        &command.base.clip,
        mask.as_deref(),
        |index, old| {
            apply_binary_rop(
                command.rop_descriptor,
                source.pixels[index],
                old,
                RopInput::Source,
                RopInput::Destination,
            )
        },
    )?;
    drop(destination);
    Ok(destination_handle)
}

/// Renders Draw Opaque as a brush/source raster operation without an intermediate surface write.
pub(super) async fn render_opaque(
    surfaces: &HashMap<u32, SurfaceHandle>,
    command: &DrawOpaque,
    source: &PreparedCompositeImage,
    pattern: Option<&PreparedCompositeImage>,
    mask: Option<&PreparedCompositeImage>,
) -> Result<SurfaceHandle, ClientError> {
    validate_rop_descriptor(command.rop_descriptor)?;
    let destination_handle = surface_for_update(surfaces, command.base.surface_id)?;
    let destination = destination_handle.inner.read().await;
    let destination_bounds = destination.rect_bounds(command.base.destination)?;
    let destination_format = destination.format;
    drop(destination);
    let source = sample_source(
        surfaces,
        source,
        command.source,
        destination_bounds.width,
        destination_bounds.height,
    )
    .await?;
    let brush = sample_brush(
        surfaces,
        command.brush,
        pattern,
        command.base.destination,
        destination_bounds,
        destination_format,
    )
    .await?;
    let mask = sample_mask(surfaces, command.mask, mask, destination_bounds).await?;
    let mut destination = destination_handle.inner.write().await;
    apply_pixels(
        &mut destination,
        destination_bounds,
        &command.base.clip,
        mask.as_deref(),
        |index, _old| {
            apply_binary_rop(
                command.rop_descriptor,
                brush.pixels[index],
                source.pixels[index],
                RopInput::Brush,
                RopInput::Source,
            )
        },
    )?;
    drop(destination);
    Ok(destination_handle)
}

/// Applies Blackness, Whiteness, or Invers over a clipped and optionally masked destination.
pub(super) async fn render_masked_destination(
    surfaces: &HashMap<u32, SurfaceHandle>,
    command: &DrawMaskedDestination,
    mask: Option<&PreparedCompositeImage>,
    operation: MaskedDestinationOperation,
) -> Result<SurfaceHandle, ClientError> {
    let destination_handle = surface_for_update(surfaces, command.base.surface_id)?;
    let destination = destination_handle.inner.read().await;
    let destination_bounds = destination.rect_bounds(command.base.destination)?;
    drop(destination);
    let mask = sample_mask(surfaces, command.mask, mask, destination_bounds).await?;
    let mut destination = destination_handle.inner.write().await;
    apply_pixels(
        &mut destination,
        destination_bounds,
        &command.base.clip,
        mask.as_deref(),
        |_index, old| match operation {
            MaskedDestinationOperation::Blackness => 0,
            MaskedDestinationOperation::Whiteness => u32::MAX,
            MaskedDestinationOperation::Invert => !old,
        },
    )?;
    drop(destination);
    Ok(destination_handle)
}

/// Evaluates the complete eight-bit ROP3 truth table for every selected destination pixel.
pub(super) async fn render_rop3(
    surfaces: &HashMap<u32, SurfaceHandle>,
    command: &DrawRop3,
    source: &PreparedCompositeImage,
    pattern: Option<&PreparedCompositeImage>,
    mask: Option<&PreparedCompositeImage>,
) -> Result<SurfaceHandle, ClientError> {
    let destination_handle = surface_for_update(surfaces, command.base.surface_id)?;
    let destination = destination_handle.inner.read().await;
    let destination_bounds = destination.rect_bounds(command.base.destination)?;
    let destination_format = destination.format;
    drop(destination);
    let source = sample_source(
        surfaces,
        source,
        command.source,
        destination_bounds.width,
        destination_bounds.height,
    )
    .await?;
    let pattern = sample_brush(
        surfaces,
        command.brush,
        pattern,
        command.base.destination,
        destination_bounds,
        destination_format,
    )
    .await?;
    let mask = sample_mask(surfaces, command.mask, mask, destination_bounds).await?;
    let mut destination = destination_handle.inner.write().await;
    apply_pixels(
        &mut destination,
        destination_bounds,
        &command.base.clip,
        mask.as_deref(),
        |index, old| {
            apply_rop3(
                command.rop3,
                pattern.pixels[index],
                source.pixels[index],
                old,
            )
        },
    )?;
    drop(destination);
    Ok(destination_handle)
}

/// Applies nearest-neighbor color-key scaling for Draw Transparent.
pub(super) async fn render_transparent(
    surfaces: &HashMap<u32, SurfaceHandle>,
    command: &DrawTransparent,
    source: &PreparedCompositeImage,
) -> Result<SurfaceHandle, ClientError> {
    let destination_handle = surface_for_update(surfaces, command.base.surface_id)?;
    let destination = destination_handle.inner.read().await;
    let destination_bounds = destination.rect_bounds(command.base.destination)?;
    drop(destination);
    let source = sample_source(
        surfaces,
        source,
        DisplayImageSource {
            image: command.source_image,
            area: command.source_area,
            scale_mode: 1,
        },
        destination_bounds.width,
        destination_bounds.height,
    )
    .await?;
    let transparent = bgrx_to_rgba_word(command.transparent_color, SurfaceFormat::Xrgb32);
    let mut destination = destination_handle.inner.write().await;
    apply_pixels(
        &mut destination,
        destination_bounds,
        &command.base.clip,
        None,
        |index, old| {
            if source.pixels[index].to_ne_bytes()[..3] == transparent.to_ne_bytes()[..3] {
                old
            } else {
                source.pixels[index]
            }
        },
    )?;
    drop(destination);
    Ok(destination_handle)
}

/// Applies global-alpha source-over blending with nearest-neighbor scaling.
pub(super) async fn render_alpha_blend(
    surfaces: &HashMap<u32, SurfaceHandle>,
    command: &DrawAlphaBlend,
    source: &PreparedCompositeImage,
) -> Result<SurfaceHandle, ClientError> {
    let destination_handle = surface_for_update(surfaces, command.base.surface_id)?;
    let destination = destination_handle.inner.read().await;
    let destination_bounds = destination.rect_bounds(command.base.destination)?;
    drop(destination);
    let source = sample_source(
        surfaces,
        source,
        DisplayImageSource {
            image: command.source_image,
            area: command.source_area,
            scale_mode: 1,
        },
        destination_bounds.width,
        destination_bounds.height,
    )
    .await?;
    let source_has_alpha = matches!(source.format, SurfaceFormat::Argb32 | SurfaceFormat::A8)
        || command.source_surface_has_alpha;
    let mut destination = destination_handle.inner.write().await;
    apply_pixels(
        &mut destination,
        destination_bounds,
        &command.base.clip,
        None,
        |index, old| {
            alpha_blend_word(
                source.pixels[index],
                old,
                command.alpha,
                source_has_alpha,
                command.destination_has_alpha,
            )
        },
    )?;
    drop(destination);
    Ok(destination_handle)
}

/// Rasterizes one bounded cosmetic stroke, including Bezier, dash, clip, and brush semantics.
pub(super) async fn render_stroke(
    surfaces: &HashMap<u32, SurfaceHandle>,
    command: &DrawStroke,
    pattern: Option<&PreparedCompositeImage>,
) -> Result<SurfaceHandle, ClientError> {
    validate_rop_descriptor(command.foreground_rop)?;
    validate_rop_descriptor(command.background_rop)?;
    let destination_handle = surface_for_update(surfaces, command.base.surface_id)?;
    let destination = destination_handle.inner.read().await;
    let destination_bounds = destination.rect_bounds(command.base.destination)?;
    let destination_format = destination.format;
    drop(destination);
    let brush_kind = if command.brush == DisplayBrush::None {
        DisplayBrush::Solid(0)
    } else {
        command.brush
    };
    let brush = sample_brush(
        surfaces,
        brush_kind,
        pattern,
        command.base.destination,
        destination_bounds,
        destination_format,
    )
    .await?;
    let coverage = rasterize_stroke(command, destination_bounds)?;
    let mut destination = destination_handle.inner.write().await;
    apply_pixels(
        &mut destination,
        destination_bounds,
        &command.base.clip,
        Some(&coverage),
        |index, old| {
            let descriptor = if coverage[index] == 1 {
                command.foreground_rop
            } else {
                command.background_rop
            };
            apply_binary_rop(
                descriptor,
                brush.pixels[index],
                old,
                RopInput::Brush,
                RopInput::Destination,
            )
        },
    )?;
    drop(destination);
    Ok(destination_handle)
}

/// Renders the background and bounded A1/A4/A8 glyph masks of one Draw Text command.
pub(super) async fn render_text(
    surfaces: &HashMap<u32, SurfaceHandle>,
    command: &DrawText<'_>,
    foreground_pattern: Option<&PreparedCompositeImage>,
    background_pattern: Option<&PreparedCompositeImage>,
) -> Result<SurfaceHandle, ClientError> {
    validate_rop_descriptor(command.foreground_rop)?;
    validate_rop_descriptor(command.background_rop)?;
    let destination_handle = surface_for_update(surfaces, command.base.surface_id)?;
    let destination = destination_handle.inner.read().await;
    let glyph_bounds = destination.rect_bounds(command.base.destination)?;
    let background_bounds = destination.rect_bounds(command.background_area)?;
    let destination_format = destination.format;
    drop(destination);

    if command.background_brush != DisplayBrush::None {
        let background_brush = sample_brush(
            surfaces,
            command.background_brush,
            background_pattern,
            command.background_area,
            background_bounds,
            destination_format,
        )
        .await?;
        let mut destination = destination_handle.inner.write().await;
        apply_pixels(
            &mut destination,
            background_bounds,
            &command.base.clip,
            None,
            |index, old| {
                apply_binary_rop(
                    command.background_rop,
                    background_brush.pixels[index],
                    old,
                    RopInput::Brush,
                    RopInput::Destination,
                )
            },
        )?;
    }

    let foreground_brush_kind = if command.foreground_brush == DisplayBrush::None {
        DisplayBrush::Solid(0)
    } else {
        command.foreground_brush
    };
    let foreground_brush = sample_brush(
        surfaces,
        foreground_brush_kind,
        foreground_pattern,
        command.base.destination,
        glyph_bounds,
        destination_format,
    )
    .await?;
    let glyph_coverage = rasterize_glyphs(command, glyph_bounds)?;
    let mut destination = destination_handle.inner.write().await;
    apply_pixels(
        &mut destination,
        glyph_bounds,
        &command.base.clip,
        Some(&glyph_coverage),
        |index, old| {
            let raster = apply_binary_rop(
                command.foreground_rop,
                foreground_brush.pixels[index],
                old,
                RopInput::Brush,
                RopInput::Destination,
            );
            blend_coverage(raster, old, glyph_coverage[index])
        },
    )?;
    drop(destination);
    Ok(destination_handle)
}

fn rasterize_stroke(command: &DrawStroke, destination: Bounds) -> Result<Vec<u8>, ClientError> {
    const PATH_BEGIN: u8 = 1 << 0;
    const PATH_END: u8 = 1 << 1;
    const PATH_CLOSE: u8 = 1 << 3;
    const PATH_BEZIER: u8 = 1 << 4;
    let pixel_count = destination
        .width
        .checked_mul(destination.height)
        .ok_or_else(|| resource_limit_error("Stroke coverage"))?;
    let work_limit = pixel_count
        .checked_mul(STROKE_WORK_MULTIPLIER)
        .and_then(|work| work.checked_add(65_536))
        .ok_or_else(|| resource_limit_error("Stroke work limit"))?;
    let mut coverage = vec![0_u8; pixel_count];
    let mut dash = DashState::new(&command.line.style, command.line.flags)?;
    let mut current_path = Vec::<(f64, f64)>::new();
    let mut work = 0_usize;
    for segment in &command.path.segments {
        let mut points = segment.points.iter();
        if segment.flags & PATH_BEGIN != 0 {
            rasterize_polyline(
                &current_path,
                false,
                destination,
                &mut coverage,
                &mut dash,
                &mut work,
                work_limit,
            )?;
            current_path.clear();
            let first = points
                .next()
                .ok_or_else(|| protocol_value_error("empty begun path"))?;
            current_path.push(fixed_point(*first));
        }
        if current_path.is_empty() {
            return Err(protocol_value_error("path segment without begin point"));
        }
        if segment.flags & PATH_BEZIER != 0 {
            let remaining: Vec<_> = points.copied().collect();
            if remaining.len() % 3 != 0 {
                return Err(protocol_value_error("Bezier control point count"));
            }
            for curve in remaining.chunks_exact(3) {
                let start = *current_path.last().expect("non-empty path");
                flatten_cubic(
                    start,
                    fixed_point(curve[0]),
                    fixed_point(curve[1]),
                    fixed_point(curve[2]),
                    0,
                    &mut current_path,
                    work_limit,
                )?;
            }
        } else {
            current_path.extend(points.map(|point| fixed_point(*point)));
        }
        if segment.flags & PATH_END != 0 {
            rasterize_polyline(
                &current_path,
                segment.flags & PATH_CLOSE != 0,
                destination,
                &mut coverage,
                &mut dash,
                &mut work,
                work_limit,
            )?;
            current_path.clear();
        }
    }
    rasterize_polyline(
        &current_path,
        false,
        destination,
        &mut coverage,
        &mut dash,
        &mut work,
        work_limit,
    )?;
    Ok(coverage)
}

fn rasterize_polyline(
    points: &[(f64, f64)],
    close: bool,
    destination: Bounds,
    coverage: &mut [u8],
    dash: &mut DashState,
    work: &mut usize,
    work_limit: usize,
) -> Result<(), ClientError> {
    for pair in points.windows(2) {
        rasterize_line(
            pair[0],
            pair[1],
            destination,
            coverage,
            dash,
            work,
            work_limit,
        )?;
    }
    if close && points.len() > 1 {
        rasterize_line(
            *points.last().expect("closed path has final point"),
            points[0],
            destination,
            coverage,
            dash,
            work,
            work_limit,
        )?;
    }
    Ok(())
}

fn rasterize_line(
    start: (f64, f64),
    end: (f64, f64),
    destination: Bounds,
    coverage: &mut [u8],
    dash: &mut DashState,
    work: &mut usize,
    work_limit: usize,
) -> Result<(), ClientError> {
    let delta_x = end.0 - start.0;
    let delta_y = end.1 - start.1;
    let length = delta_x.hypot(delta_y);
    if length == 0.0 {
        return Ok(());
    }
    let Some((clip_start, clip_end)) = clip_line_to_bounds(start, end, destination) else {
        dash.advance(length);
        return Ok(());
    };
    dash.advance(length * clip_start);
    let clipped_start = (
        start.0 + delta_x * clip_start,
        start.1 + delta_y * clip_start,
    );
    let clipped_end = (start.0 + delta_x * clip_end, start.1 + delta_y * clip_end);
    let clipped_delta_x = clipped_end.0 - clipped_start.0;
    let clipped_delta_y = clipped_end.1 - clipped_start.1;
    let steps = clipped_delta_x
        .abs()
        .max(clipped_delta_y.abs())
        .ceil()
        .max(1.0) as usize;
    *work = work
        .checked_add(steps)
        .ok_or_else(|| resource_limit_error("Stroke raster work"))?;
    if *work > work_limit {
        return Err(resource_limit_error("Stroke raster work"));
    }
    let step_length = length * (clip_end - clip_start) / steps as f64;
    for step in 0..steps {
        let fraction = step as f64 / steps as f64;
        let x = (clipped_start.0 + clipped_delta_x * fraction).floor() as i64;
        let y = (clipped_start.1 + clipped_delta_y * fraction).floor() as i64;
        if let Some(index) = destination_index(destination, x, y) {
            coverage[index] = if dash.foreground() { 1 } else { 2 };
        }
        dash.advance(step_length);
    }
    dash.advance(length * (1.0 - clip_end));
    Ok(())
}

fn flatten_cubic(
    start: (f64, f64),
    control_one: (f64, f64),
    control_two: (f64, f64),
    end: (f64, f64),
    depth: u8,
    output: &mut Vec<(f64, f64)>,
    output_limit: usize,
) -> Result<(), ClientError> {
    let flatness = point_line_distance(control_one, start, end).max(point_line_distance(
        control_two,
        start,
        end,
    ));
    if flatness <= STROKE_FLATNESS_PIXELS || depth == MAX_STROKE_CURVE_DEPTH {
        if output.len() >= output_limit {
            return Err(resource_limit_error("Bezier flattened points"));
        }
        output.push(end);
        return Ok(());
    }
    let first = midpoint(start, control_one);
    let second = midpoint(control_one, control_two);
    let third = midpoint(control_two, end);
    let fourth = midpoint(first, second);
    let fifth = midpoint(second, third);
    let middle = midpoint(fourth, fifth);
    flatten_cubic(
        start,
        first,
        fourth,
        middle,
        depth + 1,
        output,
        output_limit,
    )?;
    flatten_cubic(middle, fifth, third, end, depth + 1, output, output_limit)
}

fn rasterize_glyphs(command: &DrawText<'_>, destination: Bounds) -> Result<Vec<u8>, ClientError> {
    let pixel_count = destination
        .width
        .checked_mul(destination.height)
        .ok_or_else(|| resource_limit_error("Text glyph coverage"))?;
    let mut coverage = vec![0_u8; pixel_count];
    for glyph in &command.text.glyphs {
        let glyph_left = i64::from(glyph.render_position.x) + i64::from(glyph.origin.x);
        let glyph_top = i64::from(glyph.render_position.y) + i64::from(glyph.origin.y);
        let width = usize::from(glyph.width);
        let height = usize::from(glyph.height);
        let row_bytes = match command.text.format {
            GlyphFormat::Alpha1 => width.div_ceil(8),
            GlyphFormat::Alpha4 => width.div_ceil(2),
            GlyphFormat::Alpha8 => width,
        };
        for logical_y in 0..height {
            let storage_y = if command.text.top_down {
                logical_y
            } else {
                height - logical_y - 1
            };
            let row = &glyph.pixels[storage_y * row_bytes..(storage_y + 1) * row_bytes];
            for x in 0..width {
                let alpha = match command.text.format {
                    GlyphFormat::Alpha1 => {
                        if row[x / 8] & (1 << (7 - x % 8)) != 0 {
                            u8::MAX
                        } else {
                            0
                        }
                    }
                    GlyphFormat::Alpha4 => {
                        let nibble = if x % 2 == 0 {
                            row[x / 2] >> 4
                        } else {
                            row[x / 2] & 0x0f
                        };
                        nibble * 17
                    }
                    GlyphFormat::Alpha8 => row[x],
                };
                let surface_x = glyph_left
                    + i64::try_from(x).map_err(|_| resource_limit_error("Text glyph x"))?;
                let surface_y = glyph_top
                    + i64::try_from(logical_y).map_err(|_| resource_limit_error("Text glyph y"))?;
                if let Some(index) = destination_index(destination, surface_x, surface_y) {
                    coverage[index] = coverage[index].max(alpha);
                }
            }
        }
    }
    Ok(coverage)
}

struct DashState {
    lengths: Vec<f64>,
    index: usize,
    remaining: f64,
    starts_with_gap: bool,
}

impl DashState {
    fn new(style: &[i32], flags: u8) -> Result<Self, ClientError> {
        let lengths: Vec<f64> = style.iter().map(|value| f64::from(*value) / 16.0).collect();
        if lengths.iter().any(|length| *length <= 0.0) {
            return Err(protocol_value_error("non-positive Stroke dash length"));
        }
        let remaining = lengths.first().copied().unwrap_or(f64::INFINITY);
        Ok(Self {
            lengths,
            index: 0,
            remaining,
            starts_with_gap: flags & (1 << 2) != 0,
        })
    }

    fn foreground(&self) -> bool {
        self.lengths.is_empty() || (self.index % 2 == 0) != self.starts_with_gap
    }

    fn advance(&mut self, mut distance: f64) {
        if self.lengths.is_empty() {
            return;
        }
        while distance >= self.remaining {
            distance -= self.remaining;
            self.index = (self.index + 1) % self.lengths.len();
            self.remaining = self.lengths[self.index];
        }
        self.remaining -= distance;
    }
}

fn destination_index(destination: Bounds, x: i64, y: i64) -> Option<usize> {
    let left = i64::try_from(destination.x).ok()?;
    let top = i64::try_from(destination.y).ok()?;
    let local_x = usize::try_from(x.checked_sub(left)?).ok()?;
    let local_y = usize::try_from(y.checked_sub(top)?).ok()?;
    if local_x >= destination.width || local_y >= destination.height {
        return None;
    }
    local_y.checked_mul(destination.width)?.checked_add(local_x)
}

fn clip_line_to_bounds(start: (f64, f64), end: (f64, f64), bounds: Bounds) -> Option<(f64, f64)> {
    let left = bounds.x as f64;
    let top = bounds.y as f64;
    let right = bounds.x.checked_add(bounds.width)? as f64;
    let bottom = bounds.y.checked_add(bounds.height)? as f64;
    let delta = (end.0 - start.0, end.1 - start.1);
    let mut lower = 0.0_f64;
    let mut upper = 1.0_f64;
    for (p, q) in [
        (-delta.0, start.0 - left),
        (delta.0, right - start.0),
        (-delta.1, start.1 - top),
        (delta.1, bottom - start.1),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
            continue;
        }
        let ratio = q / p;
        if p < 0.0 {
            lower = lower.max(ratio);
        } else {
            upper = upper.min(ratio);
        }
        if lower > upper {
            return None;
        }
    }
    Some((lower.clamp(0.0, 1.0), upper.clamp(0.0, 1.0)))
}

fn blend_coverage(source: u32, destination: u32, coverage: u8) -> u32 {
    let source = source.to_ne_bytes();
    let destination = destination.to_ne_bytes();
    let alpha = u16::from(coverage);
    let inverse = 255 - alpha;
    let mut output = [0_u8; 4];
    for component in 0..4 {
        output[component] = u8::try_from(
            (u16::from(source[component]) * alpha
                + u16::from(destination[component]) * inverse
                + 127)
                / 255,
        )
        .expect("coverage blend fits u8");
    }
    u32::from_ne_bytes(output)
}

fn fixed_point(point: FixedPoint) -> (f64, f64) {
    (f64::from(point.x) / 16.0, f64::from(point.y) / 16.0)
}

fn midpoint(first: (f64, f64), second: (f64, f64)) -> (f64, f64) {
    ((first.0 + second.0) / 2.0, (first.1 + second.1) / 2.0)
}

fn point_line_distance(point: (f64, f64), start: (f64, f64), end: (f64, f64)) -> f64 {
    let delta = (end.0 - start.0, end.1 - start.1);
    let length = delta.0.hypot(delta.1);
    if length == 0.0 {
        return (point.0 - start.0).hypot(point.1 - start.1);
    }
    ((delta.1 * point.0 - delta.0 * point.1 + end.0 * start.1 - end.1 * start.0).abs()) / length
}

async fn sample_source(
    surfaces: &HashMap<u32, SurfaceHandle>,
    image: &PreparedCompositeImage,
    source: DisplayImageSource,
    output_width: usize,
    output_height: usize,
) -> Result<SampledPixels, ClientError> {
    inspect_prepared(surfaces, image, |format, width, height, pixels| {
        let source_bounds = image_rect_bounds(source.area, width, height)?;
        let sampled = scale_pixels(
            pixels,
            usize::try_from(width).map_err(|_| resource_limit_error("Canvas image width"))?,
            source_bounds,
            output_width,
            output_height,
            source.scale_mode,
        )?;
        Ok(SampledPixels {
            format,
            pixels: sampled,
        })
    })
    .await
}

async fn sample_brush(
    surfaces: &HashMap<u32, SurfaceHandle>,
    brush: DisplayBrush,
    pattern: Option<&PreparedCompositeImage>,
    destination: Rect,
    destination_bounds: Bounds,
    destination_format: SurfaceFormat,
) -> Result<SampledPixels, ClientError> {
    let pixel_count = destination_bounds
        .width
        .checked_mul(destination_bounds.height)
        .ok_or_else(|| resource_limit_error("Canvas brush pixels"))?;
    match brush {
        DisplayBrush::None => Err(unsupported_wire("empty Canvas brush")),
        DisplayBrush::Solid(color) => Ok(SampledPixels {
            format: destination_format,
            pixels: vec![bgrx_to_rgba_word(color, destination_format); pixel_count],
        }),
        DisplayBrush::Pattern { image: _, position } => {
            let pattern = pattern.ok_or(ClientError::Internal("missing prepared pattern"))?;
            inspect_prepared(surfaces, pattern, |format, width, height, pixels| {
                let width = usize::try_from(width)
                    .map_err(|_| resource_limit_error("Canvas pattern width"))?;
                let height = usize::try_from(height)
                    .map_err(|_| resource_limit_error("Canvas pattern height"))?;
                if width == 0 || height == 0 {
                    return Err(protocol_value_error("zero-sized Canvas pattern"));
                }
                let mut sampled = Vec::with_capacity(pixel_count);
                for local_y in 0..destination_bounds.height {
                    let surface_y = i64::from(destination.top)
                        + i64::try_from(local_y)
                            .map_err(|_| resource_limit_error("Canvas pattern y"))?;
                    let pattern_y = usize::try_from(
                        (surface_y - i64::from(position.y))
                            .rem_euclid(i64::try_from(height).expect("usize fits i64 on targets")),
                    )
                    .expect("non-negative pattern y");
                    for local_x in 0..destination_bounds.width {
                        let surface_x = i64::from(destination.left)
                            + i64::try_from(local_x)
                                .map_err(|_| resource_limit_error("Canvas pattern x"))?;
                        let pattern_x =
                            usize::try_from((surface_x - i64::from(position.x)).rem_euclid(
                                i64::try_from(width).expect("usize fits i64 on targets"),
                            ))
                            .expect("non-negative pattern x");
                        sampled.push(pixels[pattern_y * width + pattern_x]);
                    }
                }
                Ok(SampledPixels {
                    format,
                    pixels: sampled,
                })
            })
            .await
        }
    }
}

async fn sample_mask(
    surfaces: &HashMap<u32, SurfaceHandle>,
    mask: DisplayMask,
    prepared: Option<&PreparedCompositeImage>,
    destination_bounds: Bounds,
) -> Result<Option<Vec<u8>>, ClientError> {
    let Some(prepared) = prepared else {
        return if mask.image.is_none() {
            Ok(None)
        } else {
            Err(ClientError::Internal("missing prepared Canvas mask"))
        };
    };
    inspect_prepared(surfaces, prepared, |format, width, height, pixels| {
        let width =
            usize::try_from(width).map_err(|_| resource_limit_error("Canvas mask width"))?;
        let height =
            usize::try_from(height).map_err(|_| resource_limit_error("Canvas mask height"))?;
        let pixel_count = destination_bounds
            .width
            .checked_mul(destination_bounds.height)
            .ok_or_else(|| resource_limit_error("Canvas mask pixels"))?;
        let mut coverage = Vec::with_capacity(pixel_count);
        for local_y in 0..destination_bounds.height {
            let mask_y = i64::try_from(local_y)
                .map_err(|_| resource_limit_error("Canvas mask y"))?
                + i64::from(mask.position.y);
            for local_x in 0..destination_bounds.width {
                let mask_x = i64::try_from(local_x)
                    .map_err(|_| resource_limit_error("Canvas mask x"))?
                    + i64::from(mask.position.x);
                let inside = mask_x >= 0
                    && mask_y >= 0
                    && usize::try_from(mask_x).is_ok_and(|x| x < width)
                    && usize::try_from(mask_y).is_ok_and(|y| y < height);
                let mut selected = if inside {
                    let x = usize::try_from(mask_x).expect("validated mask x");
                    let y = usize::try_from(mask_y).expect("validated mask y");
                    let bytes = pixels[y * width + x].to_ne_bytes();
                    if format == SurfaceFormat::A8 {
                        bytes[3] != 0
                    } else {
                        bytes[..3].iter().any(|component| *component != 0)
                    }
                } else {
                    false
                };
                if inside && mask.inverted {
                    selected = !selected;
                }
                coverage.push(if selected { u8::MAX } else { 0 });
            }
        }
        Ok(Some(coverage))
    })
    .await
}

async fn inspect_prepared<T>(
    surfaces: &HashMap<u32, SurfaceHandle>,
    prepared: &PreparedCompositeImage,
    inspect: impl FnOnce(SurfaceFormat, u32, u32, &[u32]) -> Result<T, ClientError>,
) -> Result<T, ClientError> {
    match prepared {
        PreparedCompositeImage::Pixels(OwnedCompositePixels {
            format,
            width,
            height,
            pixels,
        }) => inspect(*format, *width, *height, pixels),
        PreparedCompositeImage::Surface(descriptor) => {
            inspect_surface(surfaces, *descriptor, inspect).await
        }
    }
}

async fn inspect_surface<T>(
    surfaces: &HashMap<u32, SurfaceHandle>,
    descriptor: CompositeSurface,
    inspect: impl FnOnce(SurfaceFormat, u32, u32, &[u32]) -> Result<T, ClientError>,
) -> Result<T, ClientError> {
    let handle = surface_for_update(surfaces, descriptor.surface_id)?;
    let surface = handle.inner.read().await;
    if descriptor.width != surface.width || descriptor.height != surface.height {
        return Err(protocol_value_error("Canvas surface descriptor dimensions"));
    }
    inspect(
        surface.format,
        surface.width,
        surface.height,
        &surface.pixels,
    )
}

fn scale_pixels(
    pixels: &[u32],
    image_width: usize,
    source: Bounds,
    output_width: usize,
    output_height: usize,
    scale_mode: u8,
) -> Result<Vec<u32>, ClientError> {
    let output_pixels = output_width
        .checked_mul(output_height)
        .ok_or_else(|| resource_limit_error("Canvas scaled pixels"))?;
    let mut output = Vec::with_capacity(output_pixels);
    for output_y in 0..output_height {
        for output_x in 0..output_width {
            output.push(if scale_mode == 1 {
                let source_x = source.x
                    + output_x
                        .checked_mul(source.width)
                        .ok_or_else(|| resource_limit_error("Canvas source x"))?
                        / output_width;
                let source_y = source.y
                    + output_y
                        .checked_mul(source.height)
                        .ok_or_else(|| resource_limit_error("Canvas source y"))?
                        / output_height;
                pixels[source_y * image_width + source_x]
            } else {
                interpolate_pixel(
                    pixels,
                    image_width,
                    source,
                    output_x,
                    output_y,
                    output_width,
                    output_height,
                )?
            });
        }
    }
    Ok(output)
}

fn interpolate_pixel(
    pixels: &[u32],
    image_width: usize,
    source: Bounds,
    output_x: usize,
    output_y: usize,
    output_width: usize,
    output_height: usize,
) -> Result<u32, ClientError> {
    const UNIT: u64 = 1 << 16;
    let fixed_x = centered_source_coordinate(output_x, output_width, source.width)?;
    let fixed_y = centered_source_coordinate(output_y, output_height, source.height)?;
    let x0 = usize::try_from(fixed_x / UNIT).expect("fixed coordinate fits usize");
    let y0 = usize::try_from(fixed_y / UNIT).expect("fixed coordinate fits usize");
    let x1 = (x0 + 1).min(source.width - 1);
    let y1 = (y0 + 1).min(source.height - 1);
    let fraction_x = fixed_x % UNIT;
    let fraction_y = fixed_y % UNIT;
    let top_left = pixels[(source.y + y0) * image_width + source.x + x0].to_ne_bytes();
    let top_right = pixels[(source.y + y0) * image_width + source.x + x1].to_ne_bytes();
    let bottom_left = pixels[(source.y + y1) * image_width + source.x + x0].to_ne_bytes();
    let bottom_right = pixels[(source.y + y1) * image_width + source.x + x1].to_ne_bytes();
    let mut output = [0_u8; 4];
    for component in 0..4 {
        let top = u64::from(top_left[component]) * (UNIT - fraction_x)
            + u64::from(top_right[component]) * fraction_x;
        let bottom = u64::from(bottom_left[component]) * (UNIT - fraction_x)
            + u64::from(bottom_right[component]) * fraction_x;
        let value = top * (UNIT - fraction_y) + bottom * fraction_y;
        output[component] = u8::try_from((value + UNIT * UNIT / 2) / (UNIT * UNIT))
            .expect("interpolated channel fits u8");
    }
    Ok(u32::from_ne_bytes(output))
}

fn centered_source_coordinate(
    output_coordinate: usize,
    output_length: usize,
    source_length: usize,
) -> Result<u64, ClientError> {
    const UNIT: u64 = 1 << 16;
    let numerator = u64::try_from(output_coordinate)
        .ok()
        .and_then(|coordinate| coordinate.checked_mul(2))
        .and_then(|coordinate| coordinate.checked_add(1))
        .and_then(|coordinate| coordinate.checked_mul(u64::try_from(source_length).ok()?))
        .and_then(|coordinate| coordinate.checked_mul(UNIT))
        .ok_or_else(|| resource_limit_error("Canvas interpolation coordinate"))?;
    let denominator = u64::try_from(output_length)
        .ok()
        .and_then(|length| length.checked_mul(2))
        .ok_or_else(|| resource_limit_error("Canvas interpolation length"))?;
    Ok((numerator / denominator).saturating_sub(UNIT / 2))
}

fn apply_pixels(
    destination: &mut SurfaceData,
    bounds: Bounds,
    clip: &CompositeClip,
    mask: Option<&[u8]>,
    mut operation: impl FnMut(usize, u32) -> u32,
) -> Result<(), ClientError> {
    let surface_width = usize::try_from(destination.width)
        .map_err(|_| resource_limit_error("Canvas destination width"))?;
    for local_y in 0..bounds.height {
        let surface_y = bounds.y + local_y;
        for local_x in 0..bounds.width {
            let surface_x = bounds.x + local_x;
            let local_index = local_y * bounds.width + local_x;
            if !clip_contains(clip, surface_x, surface_y)?
                || mask.is_some_and(|coverage| coverage[local_index] == 0)
            {
                continue;
            }
            let destination_index = surface_y * surface_width + surface_x;
            let old = destination.pixels[destination_index];
            destination.pixels[destination_index] =
                normalize_word(destination.format, operation(local_index, old));
        }
    }
    Ok(())
}

pub(super) fn clip_contains(clip: &CompositeClip, x: usize, y: usize) -> Result<bool, ClientError> {
    match clip {
        CompositeClip::None => Ok(true),
        CompositeClip::Rectangles(rectangles) => {
            let x = i64::try_from(x).map_err(|_| resource_limit_error("Canvas clip x"))?;
            let y = i64::try_from(y).map_err(|_| resource_limit_error("Canvas clip y"))?;
            Ok(rectangles.iter().any(|rectangle| {
                x >= i64::from(rectangle.left)
                    && x < i64::from(rectangle.right)
                    && y >= i64::from(rectangle.top)
                    && y < i64::from(rectangle.bottom)
            }))
        }
    }
}

fn validate_rop_descriptor(descriptor: u16) -> Result<(), ClientError> {
    if descriptor & !KNOWN_ROP_BITS != 0 {
        return Err(unsupported_wire("Canvas raster operation bits"));
    }
    Ok(())
}

fn apply_binary_rop(
    descriptor: u16,
    mut source: u32,
    mut destination: u32,
    source_kind: RopInput,
    destination_kind: RopInput,
) -> u32 {
    if descriptor & inversion_bit(source_kind) != 0 {
        source = !source;
    }
    if descriptor & inversion_bit(destination_kind) != 0 {
        destination = !destination;
    }
    let mut result = if descriptor & ROP_PUT != 0 {
        source
    } else if descriptor & ROP_OR != 0 {
        source | destination
    } else if descriptor & ROP_AND != 0 {
        source & destination
    } else if descriptor & ROP_XOR != 0 {
        source ^ destination
    } else if descriptor & ROP_BLACKNESS != 0 {
        0
    } else if descriptor & ROP_WHITENESS != 0 {
        u32::MAX
    } else if descriptor & ROP_INVERT != 0 {
        !destination
    } else {
        source
    };
    if descriptor & ROP_INVERT_RESULT != 0 {
        result = !result;
    }
    result
}

const fn inversion_bit(input: RopInput) -> u16 {
    match input {
        RopInput::Source => ROP_INVERT_SOURCE,
        RopInput::Brush => ROP_INVERT_BRUSH,
        RopInput::Destination => ROP_INVERT_DESTINATION,
    }
}

fn apply_rop3(code: u8, pattern: u32, source: u32, destination: u32) -> u32 {
    let mut result = 0_u32;
    for index in 0_u8..8 {
        if code & (1 << index) == 0 {
            continue;
        }
        let pattern_term = if index & 0b100 != 0 {
            pattern
        } else {
            !pattern
        };
        let source_term = if index & 0b010 != 0 { source } else { !source };
        let destination_term = if index & 0b001 != 0 {
            destination
        } else {
            !destination
        };
        result |= pattern_term & source_term & destination_term;
    }
    result
}

fn alpha_blend_word(
    source: u32,
    destination: u32,
    global_alpha: u8,
    source_has_alpha: bool,
    destination_has_alpha: bool,
) -> u32 {
    let source = source.to_ne_bytes();
    let destination = destination.to_ne_bytes();
    let source_alpha = if source_has_alpha {
        u16::from(source[3])
    } else {
        u16::from(u8::MAX)
    };
    let alpha = (source_alpha * u16::from(global_alpha) + 127) / 255;
    let inverse = 255 - alpha;
    let mut output = [0_u8; 4];
    for component in 0..3 {
        output[component] = u8::try_from(
            (u16::from(source[component]) * alpha
                + u16::from(destination[component]) * inverse
                + 127)
                / 255,
        )
        .expect("blended channel fits u8");
    }
    output[3] = if destination_has_alpha {
        u8::try_from(alpha + (u16::from(destination[3]) * inverse + 127) / 255)
            .expect("blended alpha fits u8")
    } else {
        u8::MAX
    };
    u32::from_ne_bytes(output)
}

fn bgrx_to_rgba_word(color: u32, format: SurfaceFormat) -> u32 {
    let bgrx = color.to_le_bytes();
    let rgba = if format == SurfaceFormat::A8 {
        [u8::MAX, u8::MAX, u8::MAX, bgrx[0]]
    } else {
        [bgrx[2], bgrx[1], bgrx[0], u8::MAX]
    };
    u32::from_ne_bytes(rgba)
}

fn normalize_word(format: SurfaceFormat, word: u32) -> u32 {
    let bytes = word.to_ne_bytes();
    u32::from_ne_bytes(match format {
        SurfaceFormat::A8 => [u8::MAX, u8::MAX, u8::MAX, bytes[3]],
        SurfaceFormat::Xrgb32 => [bytes[0], bytes[1], bytes[2], u8::MAX],
        SurfaceFormat::Argb32 => bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxide_spice_protocol::{
        DisplayBase, DisplayPath, LineAttributes, PathSegment, Point, RasterGlyph, RasterString,
    };

    #[test]
    fn rop3_matches_a_per_bit_truth_table_for_every_code() {
        let pattern = 0xa53c_0ff0_u32;
        let source = 0x5ac3_33cc_u32;
        let destination = 0x0f0f_9696_u32;
        for code in 0_u8..=u8::MAX {
            let actual = apply_rop3(code, pattern, source, destination);
            let mut expected = 0_u32;
            for bit in 0..32 {
                let index = (((pattern >> bit) & 1) << 2)
                    | (((source >> bit) & 1) << 1)
                    | ((destination >> bit) & 1);
                if (u32::from(code) >> index) & 1 != 0 {
                    expected |= 1 << bit;
                }
            }
            assert_eq!(actual, expected, "ROP3 code {code:#04x}");
        }
        assert_eq!(apply_rop3(0xcc, pattern, source, destination), source);
        assert_eq!(apply_rop3(0xf0, pattern, source, destination), pattern);
        assert_eq!(apply_rop3(0xaa, pattern, source, destination), destination);
        assert_eq!(
            apply_rop3(0x66, pattern, source, destination),
            source ^ destination
        );
    }

    #[test]
    fn binary_rop_uses_the_semantic_inversion_bits() {
        let source = 0x00ff_00ff;
        let destination = 0x0f0f_0f0f;
        assert_eq!(
            apply_binary_rop(
                ROP_XOR | ROP_INVERT_SOURCE,
                source,
                destination,
                RopInput::Source,
                RopInput::Destination,
            ),
            !source ^ destination
        );
        assert_eq!(
            apply_binary_rop(
                ROP_AND | ROP_INVERT_BRUSH,
                source,
                destination,
                RopInput::Brush,
                RopInput::Destination,
            ),
            !source & destination
        );
    }

    #[test]
    fn global_alpha_blend_preserves_straight_rgba_channels() {
        let source = u32::from_ne_bytes([200, 100, 0, 128]);
        let destination = u32::from_ne_bytes([0, 100, 200, 64]);
        let result = alpha_blend_word(source, destination, 128, true, true).to_ne_bytes();
        assert_eq!(result, [50, 100, 150, 112]);
    }

    #[test]
    fn cosmetic_stroke_is_endpoint_exclusive_and_bounded_to_destination() {
        let command = DrawStroke {
            base: DisplayBase {
                surface_id: 0,
                destination: Rect {
                    top: 0,
                    left: 0,
                    bottom: 4,
                    right: 8,
                },
                clip: CompositeClip::None,
            },
            path: DisplayPath {
                segments: vec![PathSegment {
                    flags: (1 << 0) | (1 << 1),
                    points: vec![FixedPoint { x: 16, y: 16 }, FixedPoint { x: 96, y: 16 }],
                }],
            },
            line: LineAttributes {
                flags: 0,
                style: Vec::new(),
            },
            brush: DisplayBrush::Solid(0),
            foreground_rop: ROP_PUT,
            background_rop: ROP_PUT,
        };
        let coverage = rasterize_stroke(
            &command,
            Bounds {
                x: 0,
                y: 0,
                width: 8,
                height: 4,
            },
        )
        .expect("bounded stroke");
        assert_eq!(&coverage[8 + 1..8 + 6], &[1, 1, 1, 1, 1]);
        assert_eq!(coverage[8 + 6], 0);
    }

    #[test]
    fn raster_glyph_expands_high_nibble_first() {
        let pixels = [0xf1];
        let command = DrawText {
            base: DisplayBase {
                surface_id: 0,
                destination: Rect {
                    top: 0,
                    left: 0,
                    bottom: 1,
                    right: 2,
                },
                clip: CompositeClip::None,
            },
            text: RasterString {
                format: GlyphFormat::Alpha4,
                top_down: true,
                glyphs: vec![RasterGlyph {
                    render_position: Point { x: 0, y: 0 },
                    origin: Point { x: 0, y: 0 },
                    width: 2,
                    height: 1,
                    pixels: &pixels,
                }],
            },
            background_area: Rect {
                top: 0,
                left: 0,
                bottom: 1,
                right: 2,
            },
            foreground_brush: DisplayBrush::Solid(0),
            background_brush: DisplayBrush::None,
            foreground_rop: ROP_PUT,
            background_rop: ROP_PUT,
        };
        let coverage = rasterize_glyphs(
            &command,
            Bounds {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
        )
        .expect("bounded glyph");
        assert_eq!(coverage, [255, 17]);
    }
}
