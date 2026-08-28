//! Composite an arriving radar sweep over the previous complete image.
//!
//! This module deliberately operates on already-rendered RGBA rasters. The two
//! source rasters therefore have identical viewport geometry and color rules;
//! the only decision left is which sweep owns each pixel. Ownership is angular,
//! never alpha-based: inside the revealed arc the incoming image is
//! authoritative even when its pixel is transparent, while outside the arc the
//! previous image remains visible.

use rayon::prelude::*;

use super::{
    Result, ViewportRasterOptions, azimuth_from_xy, ensure_rgba_buffer, viewport_dimensions,
};

/// Allocate and return the angular composite of `incoming` over `previous`.
///
/// Both inputs must be RGBA8 rasters with the dimensions in `options`. The
/// revealed arc is clockwise and half-open:
/// `[start_deg, start_deg + revealed_deg)`. This keeps the radial at the moving
/// frontier on the previous image until the antenna has actually swept past it.
pub fn composite_sweep_reveal_rgba(
    incoming: &[u8],
    previous: &[u8],
    options: ViewportRasterOptions,
    start_deg: f32,
    revealed_deg: f32,
) -> Result<Vec<u8>> {
    let (width, height) = viewport_dimensions(options);
    let mut output = vec![0; width as usize * height as usize * 4];
    composite_sweep_reveal_rgba_into(
        incoming,
        previous,
        options,
        start_deg,
        revealed_deg,
        &mut output,
    )?;
    Ok(output)
}

/// Write the angular composite of `incoming` over `previous` into `output`.
///
/// This selects complete RGBA pixels; it never alpha-blends them. Consequently,
/// a transparent incoming pixel inside the revealed arc stays transparent and
/// cannot resurrect echo from the previous sweep.
///
/// Returns the normalized raster dimensions (`width` and `height` are each at
/// least one, matching the viewport renderer).
pub fn composite_sweep_reveal_rgba_into(
    incoming: &[u8],
    previous: &[u8],
    options: ViewportRasterOptions,
    start_deg: f32,
    revealed_deg: f32,
    output: &mut [u8],
) -> Result<(u32, u32)> {
    let (width, height) = viewport_dimensions(options);
    ensure_rgba_buffer(incoming, width, height)?;
    ensure_rgba_buffer(previous, width, height)?;
    ensure_rgba_buffer(output, width, height)?;

    let revealed_deg = revealed_arc_deg(revealed_deg);
    if revealed_deg <= 0.0 {
        output.copy_from_slice(previous);
        return Ok((width, height));
    }

    // A complete reveal is byte-identical to the ordinary incoming render.
    // A non-finite start cannot place a partial arc honestly, so it takes the
    // same fail-current path instead of presenting the stale sweep as live.
    if revealed_deg >= 360.0 || !start_deg.is_finite() {
        output.copy_from_slice(incoming);
        return Ok((width, height));
    }

    let row_stride = width as usize * 4;
    output
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, output_row)| {
            let row_start = y * row_stride;
            let incoming_row = &incoming[row_start..row_start + row_stride];
            let previous_row = &previous[row_start..row_start + row_stride];

            for x in 0..width {
                let pixel = x as usize * 4;
                let azimuth_deg = pixel_azimuth_deg(options, x, y as u32);
                let source = if clockwise_offset_deg(start_deg, azimuth_deg) < revealed_deg {
                    incoming_row
                } else {
                    previous_row
                };
                output_row[pixel..pixel + 4].copy_from_slice(&source[pixel..pixel + 4]);
            }
        });

    Ok((width, height))
}

/// Mask an already-rendered incoming sweep in place.
///
/// Pixels outside the revealed arc are replaced by `previous`, or made
/// transparent when no completed underlay exists. This is the worker hot path:
/// unlike [`composite_sweep_reveal_rgba_into`], it needs neither a transparent
/// scratch frame nor a second full-size output allocation on every animation
/// step.
pub fn mask_sweep_reveal_rgba_in_place(
    incoming: &mut [u8],
    previous: Option<&[u8]>,
    options: ViewportRasterOptions,
    start_deg: f32,
    revealed_deg: f32,
) -> Result<(u32, u32)> {
    let (width, height) = viewport_dimensions(options);
    ensure_rgba_buffer(incoming, width, height)?;
    if let Some(previous) = previous {
        ensure_rgba_buffer(previous, width, height)?;
    }

    let revealed_deg = revealed_arc_deg(revealed_deg);
    if revealed_deg <= 0.0 {
        if let Some(previous) = previous {
            incoming.copy_from_slice(previous);
        } else {
            incoming.fill(0);
        }
        return Ok((width, height));
    }

    // Full or unplaceable arcs leave the authoritative incoming render intact.
    if revealed_deg >= 360.0 || !start_deg.is_finite() {
        return Ok((width, height));
    }

    let row_stride = width as usize * 4;
    incoming
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, incoming_row)| {
            let previous_row = previous.map(|previous| {
                let row_start = y * row_stride;
                &previous[row_start..row_start + row_stride]
            });
            for x in 0..width {
                let pixel = x as usize * 4;
                if clockwise_offset_deg(start_deg, pixel_azimuth_deg(options, x, y as u32))
                    >= revealed_deg
                {
                    if let Some(previous_row) = previous_row {
                        incoming_row[pixel..pixel + 4]
                            .copy_from_slice(&previous_row[pixel..pixel + 4]);
                    } else {
                        incoming_row[pixel..pixel + 4].fill(0);
                    }
                }
            }
        });

    Ok((width, height))
}

/// Whether `azimuth_deg` lies inside the clockwise revealed arc.
///
/// The end of the arc is excluded. A finite reveal is clamped into `0..=360`;
/// a non-finite reveal degrades to fully incoming rather than silently showing
/// a stale sweep. Likewise, a non-finite start reveals every pixel once the arc
/// is non-empty, because no honest partial boundary can be placed.
pub fn azimuth_is_revealed(start_deg: f32, revealed_deg: f32, azimuth_deg: f32) -> bool {
    let revealed_deg = revealed_arc_deg(revealed_deg);
    if revealed_deg >= 360.0 {
        return true;
    }
    clockwise_offset_deg(start_deg, azimuth_deg) < revealed_deg
}

fn revealed_arc_deg(revealed_deg: f32) -> f32 {
    if revealed_deg.is_finite() {
        revealed_deg.clamp(0.0, 360.0)
    } else {
        360.0
    }
}

fn clockwise_offset_deg(start_deg: f32, azimuth_deg: f32) -> f32 {
    let offset = (azimuth_deg - start_deg).rem_euclid(360.0);
    if !offset.is_finite() || offset >= 360.0 {
        // A broken start or pixel coordinate must not make the old image own
        // the whole display. Zero places it inside every non-empty reveal.
        0.0
    } else {
        offset
    }
}

/// Compass azimuth of a pixel center under the viewport renderer's exact
/// screen-ENU to radar-ENU rotation convention.
fn pixel_azimuth_deg(options: ViewportRasterOptions, x: u32, y: u32) -> f32 {
    let km_per_px_x = options.km_per_px_x.max(f32::EPSILON);
    let km_per_px_y = options.km_per_px_y.max(f32::EPSILON);
    let dx_km = (x as f32 + 0.5 - options.radar_x_px) * km_per_px_x;
    let dy_km = (options.radar_y_px - (y as f32 + 0.5)) * km_per_px_y;
    let (rot_sin, rot_cos) = if options.rotation_rad.is_finite() {
        options.rotation_rad.sin_cos()
    } else {
        (0.0, 1.0)
    };
    let east_km = dx_km * rot_cos - dy_km * rot_sin;
    let north_km = dx_km * rot_sin + dy_km * rot_cos;
    azimuth_from_xy(east_km, north_km)
}

#[cfg(test)]
mod tests {
    use std::f32::consts::FRAC_PI_2;

    use super::*;
    use crate::RenderError;

    const INCOMING: [u8; 4] = [11, 22, 33, 44];
    const PREVIOUS: [u8; 4] = [201, 202, 203, 204];

    fn options(rotation_rad: f32) -> ViewportRasterOptions {
        ViewportRasterOptions {
            width: 3,
            height: 3,
            radar_x_px: 1.5,
            radar_y_px: 1.5,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
            rotation_rad,
        }
    }

    fn solid(pixel: [u8; 4]) -> Vec<u8> {
        pixel.repeat(9)
    }

    fn pixel(rgba: &[u8], x: usize, y: usize) -> [u8; 4] {
        let offset = (y * 3 + x) * 4;
        rgba[offset..offset + 4].try_into().expect("RGBA pixel")
    }

    #[test]
    fn zero_and_full_reveals_are_byte_identical_to_their_owning_rasters() {
        let incoming = (0..36).map(|value| value as u8).collect::<Vec<_>>();
        let previous = (0..36)
            .map(|value| 255_u8.saturating_sub(value as u8))
            .collect::<Vec<_>>();

        assert_eq!(
            composite_sweep_reveal_rgba(&incoming, &previous, options(0.0), 197.5, 0.0)
                .expect("zero reveal"),
            previous
        );
        assert_eq!(
            composite_sweep_reveal_rgba(&incoming, &previous, options(0.0), 197.5, 360.0)
                .expect("full reveal"),
            incoming
        );
        assert_eq!(
            composite_sweep_reveal_rgba(&incoming, &previous, options(0.0), 197.5, 720.0)
                .expect("over-full reveal"),
            incoming
        );
    }

    #[test]
    fn half_open_arc_uses_incoming_inside_and_previous_at_the_frontier() {
        let incoming = solid(INCOMING);
        let previous = solid(PREVIOUS);
        let rgba = composite_sweep_reveal_rgba(&incoming, &previous, options(0.0), 0.0, 180.0)
            .expect("half reveal");

        assert_eq!(pixel(&rgba, 1, 0), INCOMING, "north/start");
        assert_eq!(pixel(&rgba, 2, 1), INCOMING, "east/inside");
        assert_eq!(pixel(&rgba, 1, 2), PREVIOUS, "south/frontier");
        assert_eq!(pixel(&rgba, 0, 1), PREVIOUS, "west/outside");
    }

    #[test]
    fn transparent_incoming_pixel_does_not_fall_back_to_old_echo() {
        let mut incoming = solid(INCOMING);
        incoming[4..8].fill(0);
        let previous = solid(PREVIOUS);
        let rgba = composite_sweep_reveal_rgba(&incoming, &previous, options(0.0), 0.0, 90.0)
            .expect("transparent reveal");

        assert_eq!(pixel(&rgba, 1, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn clockwise_arc_wraps_across_zero() {
        assert!(azimuth_is_revealed(300.0, 120.0, 300.0));
        assert!(azimuth_is_revealed(300.0, 120.0, 359.9));
        assert!(azimuth_is_revealed(300.0, 120.0, 0.0));
        assert!(azimuth_is_revealed(300.0, 120.0, 59.9));
        assert!(!azimuth_is_revealed(300.0, 120.0, 60.0));
        assert!(!azimuth_is_revealed(300.0, 120.0, 299.9));
    }

    #[test]
    fn viewport_rotation_uses_the_same_true_azimuth_convention_as_gate_lookup() {
        let incoming = solid(INCOMING);
        let previous = solid(PREVIOUS);
        let rgba = composite_sweep_reveal_rgba(&incoming, &previous, options(FRAC_PI_2), 0.0, 1.0)
            .expect("rotated reveal");

        // With +90 degrees baked into the viewport, true north is displayed
        // to the right of the radar. Screen-up is true west.
        assert_eq!(pixel(&rgba, 2, 1), INCOMING, "rotated true north");
        assert_eq!(
            pixel(&rgba, 1, 0),
            PREVIOUS,
            "screen north is not true north"
        );
    }

    #[test]
    fn nonfinite_geometry_never_promotes_the_stale_sweep_to_live() {
        let incoming = solid(INCOMING);
        let previous = solid(PREVIOUS);

        for revealed_deg in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                composite_sweep_reveal_rgba(&incoming, &previous, options(0.0), 0.0, revealed_deg,)
                    .expect("nonfinite reveal"),
                incoming
            );
        }
        for start_deg in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                composite_sweep_reveal_rgba(&incoming, &previous, options(0.0), start_deg, 180.0,)
                    .expect("nonfinite start"),
                incoming
            );
            assert_eq!(
                composite_sweep_reveal_rgba(&incoming, &previous, options(0.0), start_deg, 0.0,)
                    .expect("empty nonfinite arc"),
                previous
            );
        }
    }

    #[test]
    fn every_buffer_must_match_the_viewport_dimensions() {
        let good = solid(INCOMING);
        let short = &good[..good.len() - 1];
        let mut output = vec![0; good.len()];

        let error =
            composite_sweep_reveal_rgba_into(short, &good, options(0.0), 0.0, 180.0, &mut output)
                .expect_err("short incoming must fail");
        assert!(matches!(
            error,
            RenderError::BufferSizeMismatch { actual: 35, .. }
        ));

        let mut short_output = vec![0; good.len() - 1];
        let error = composite_sweep_reveal_rgba_into(
            &good,
            &good,
            options(0.0),
            0.0,
            180.0,
            &mut short_output,
        )
        .expect_err("short output must fail");
        assert!(matches!(
            error,
            RenderError::BufferSizeMismatch { actual: 35, .. }
        ));
    }

    #[test]
    fn in_place_mask_uses_cached_underlay_or_transparency_without_scratch_frames() {
        let previous = solid(PREVIOUS);
        let mut with_previous = solid(INCOMING);
        mask_sweep_reveal_rgba_in_place(
            &mut with_previous,
            Some(&previous),
            options(0.0),
            0.0,
            180.0,
        )
        .expect("in-place underlay mask");
        assert_eq!(pixel(&with_previous, 1, 0), INCOMING);
        assert_eq!(pixel(&with_previous, 0, 1), PREVIOUS);

        let mut without_previous = solid(INCOMING);
        mask_sweep_reveal_rgba_in_place(&mut without_previous, None, options(0.0), 0.0, 180.0)
            .expect("in-place transparent mask");
        assert_eq!(pixel(&without_previous, 1, 0), INCOMING);
        assert_eq!(pixel(&without_previous, 0, 1), [0, 0, 0, 0]);
    }
}
