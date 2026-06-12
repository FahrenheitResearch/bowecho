//! Pure screen-space geometry for the meteorological annotation glyphs.
//!
//! The spacing/size ratios and path math are a Rust reimplementation of the
//! canvas/SVG renderer from GBW Overlay (graphics vocabulary contributed by
//! its author, grayskieswx on YouTube): Catmull-Rom smoothing, arc-length
//! glyph stations (`spacing = 28 + 5·thickness`, first station at 0.55× the
//! spacing), triangle/semicircle pip proportions, and the watch-box hatch
//! spacing. Everything here is screen-px and projection-agnostic so callers
//! can geo-anchor shapes and project per frame.

use eframe::egui::{Pos2, Vec2, pos2, vec2};

/// Catmull-Rom segments per control-point span for committed shapes
/// (GBW renders fronts with 18).
pub(crate) const SMOOTH_SEGMENTS: usize = 18;
/// Front pip spacing in px for thickness `t`: `28 + 5t`.
pub(crate) fn front_pip_spacing(thickness: f32) -> f32 {
    28.0 + thickness * 5.0
}
/// Cold-front triangle height for thickness `t` (GBW `sz = 5.5t`).
pub(crate) fn front_triangle_size(thickness: f32) -> f32 {
    thickness * 5.5
}
/// Warm-front semicircle radius for thickness `t` (GBW `r = 5t`).
pub(crate) fn front_arc_radius(thickness: f32) -> f32 {
    thickness * 5.0
}
/// Squall-line tick spacing (`22 + 2t`) and half-length (`5.5t`).
pub(crate) fn squall_tick_spacing(thickness: f32) -> f32 {
    22.0 + thickness * 2.0
}
pub(crate) fn squall_tick_half_len(thickness: f32) -> f32 {
    thickness * 5.5
}
/// Triangle base half-width as a fraction of its height (GBW 0.55).
const TRI_BASE_HALF_RATIO: f32 = 0.55;
/// First glyph sits at 0.55× the spacing; none within 4 px of the path end.
const FIRST_STATION_FRACTION: f32 = 0.55;
const STATION_END_MARGIN_PX: f32 = 4.0;

/// Uniform Catmull-Rom through `points` with `segments_per_span` samples per
/// span (the GBW `catmullRom` basis). Two-point paths still produce the
/// straight segment samples, which keeps arc-length math uniform.
pub(crate) fn catmull_rom(points: &[Pos2], segments_per_span: usize) -> Vec<Pos2> {
    if points.len() < 2 || segments_per_span == 0 {
        return points.to_vec();
    }
    let n = points.len();
    let mut out = Vec::with_capacity((n - 1) * segments_per_span + 1);
    for i in 0..n - 1 {
        let p0 = points[i.saturating_sub(1)];
        let p1 = points[i];
        let p2 = points[i + 1];
        let p3 = points[(i + 2).min(n - 1)];
        for s in 0..segments_per_span {
            let t = s as f32 / segments_per_span as f32;
            let t2 = t * t;
            let t3 = t2 * t;
            out.push(pos2(
                0.5 * (2.0 * p1.x
                    + (p2.x - p0.x) * t
                    + (2.0 * p0.x - 5.0 * p1.x + 4.0 * p2.x - p3.x) * t2
                    + (3.0 * p1.x - p0.x - 3.0 * p2.x + p3.x) * t3),
                0.5 * (2.0 * p1.y
                    + (p2.y - p0.y) * t
                    + (2.0 * p0.y - 5.0 * p1.y + 4.0 * p2.y - p3.y) * t2
                    + (3.0 * p1.y - p0.y - 3.0 * p2.y + p3.y) * t3),
            ));
        }
    }
    out.push(points[n - 1]);
    out
}

pub(crate) fn polyline_length(points: &[Pos2]) -> f32 {
    points
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).length())
        .sum()
}

/// A point along a polyline with the local tangent angle (radians, screen
/// y-down convention — matches `atan2` on screen deltas).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PathSample {
    pub(crate) pos: Pos2,
    pub(crate) angle: f32,
}

/// Point + tangent at arc-length `dist` along `points` (GBW `atDist`).
/// Distances past the end clamp to the final point with the last segment's
/// heading.
pub(crate) fn sample_at(points: &[Pos2], dist: f32) -> PathSample {
    let Some(&first) = points.first() else {
        return PathSample {
            pos: Pos2::ZERO,
            angle: 0.0,
        };
    };
    let mut walked = 0.0;
    for pair in points.windows(2) {
        let seg = pair[1] - pair[0];
        let len = seg.length();
        if len > 0.0 && walked + len >= dist {
            let f = ((dist - walked) / len).max(0.0);
            return PathSample {
                pos: pair[0] + seg * f,
                angle: seg.y.atan2(seg.x),
            };
        }
        walked += len;
    }
    let last = *points.last().unwrap_or(&first);
    let prev = if points.len() >= 2 {
        points[points.len() - 2]
    } else {
        last
    };
    let d = last - prev;
    PathSample {
        pos: last,
        angle: d.y.atan2(d.x),
    }
}

/// Even arc-length glyph stations: first at `0.55·spacing`, then every
/// `spacing`, stopping 4 px short of the end (the GBW pip loop).
pub(crate) fn glyph_stations(total_len: f32, spacing: f32) -> Vec<f32> {
    let mut out = Vec::new();
    if spacing <= 0.0 {
        return out;
    }
    let mut d = spacing * FIRST_STATION_FRACTION;
    while d < total_len - STATION_END_MARGIN_PX {
        out.push(d);
        d += spacing;
    }
    out
}

/// Splits a polyline into dashed sub-polylines following `pattern`
/// (alternating on/off lengths in px, starting "on"). Used for the outflow
/// `[10,7]` and trough `[12,5,3,5]` strokes so the dashes hug the smoothed
/// curve instead of chord-cutting it.
pub(crate) fn dash_segments(points: &[Pos2], pattern: &[f32]) -> Vec<Vec<Pos2>> {
    if points.len() < 2 {
        return Vec::new();
    }
    if pattern.iter().copied().sum::<f32>() <= 0.0 {
        return vec![points.to_vec()];
    }
    let mut out = Vec::new();
    let mut current: Vec<Pos2> = vec![points[0]];
    let mut on = true;
    let mut phase_idx = 0usize;
    let mut phase_left = pattern[0];
    for pair in points.windows(2) {
        let b = pair[1];
        let mut a = pair[0];
        let mut seg_left = (b - a).length();
        if seg_left <= 0.0 {
            continue;
        }
        let dir = (b - a) / seg_left;
        while seg_left > 0.0 {
            if phase_left >= seg_left {
                phase_left -= seg_left;
                if on {
                    current.push(b);
                }
                seg_left = 0.0;
            } else {
                let cut = a + dir * phase_left;
                seg_left -= phase_left;
                a = cut;
                if on {
                    current.push(cut);
                    out.push(std::mem::take(&mut current));
                }
                current.clear();
                current.push(cut);
                on = !on;
                phase_idx = (phase_idx + 1) % pattern.len();
                phase_left = pattern[phase_idx].max(1e-3);
            }
        }
    }
    if on && current.len() >= 2 {
        out.push(current);
    }
    out.retain(|seg| seg.len() >= 2);
    out
}

/// The three corners of a front pip triangle: base centered on `center`
/// along the tangent, apex `size` px out on `side` (−1 = left of travel in
/// screen y-down coords, the GBW default; +1 = flipped).
pub(crate) fn triangle_points(center: Pos2, size: f32, tangent_angle: f32, side: f32) -> [Pos2; 3] {
    let base_half = size * TRI_BASE_HALF_RATIO;
    let t = vec2(tangent_angle.cos(), tangent_angle.sin());
    let normal_angle = tangent_angle + side * std::f32::consts::FRAC_PI_2;
    let n = vec2(normal_angle.cos(), normal_angle.sin());
    [
        center + base_half * t,
        center - base_half * t,
        center + size * n,
    ]
}

/// Semicircle sample points (chord along the tangent, bulge toward `side`).
/// Closing the returned arc yields the filled warm-front pip; stroking it
/// open yields the dryline scallop.
pub(crate) fn semicircle_points(
    center: Pos2,
    radius: f32,
    tangent_angle: f32,
    side: f32,
    segments: usize,
) -> Vec<Pos2> {
    let segments = segments.max(3);
    (0..=segments)
        .map(|i| {
            let theta = tangent_angle + side * std::f32::consts::PI * (i as f32 / segments as f32);
            center + radius * vec2(theta.cos(), theta.sin())
        })
        .collect()
}

/// Clips the (finite) segment `a→b` to a simple polygon via even-odd
/// intersection pairing. Handles concave outlines; used by the hatch fill.
pub(crate) fn clip_line_to_polygon(a: Pos2, b: Pos2, poly: &[Pos2]) -> Vec<(Pos2, Pos2)> {
    if poly.len() < 3 {
        return Vec::new();
    }
    let d = b - a;
    let cross = |u: Vec2, v: Vec2| u.x * v.y - u.y * v.x;
    let mut ts: Vec<f32> = Vec::new();
    for i in 0..poly.len() {
        let p = poly[i];
        let e = poly[(i + 1) % poly.len()] - p;
        let denom = cross(d, e);
        if denom.abs() < 1e-9 {
            continue;
        }
        let w = p - a;
        let t = cross(w, e) / denom;
        let u = cross(w, d) / denom;
        // Half-open on the edge param so shared vertices count once.
        if (0.0..1.0).contains(&u) && (0.0..=1.0).contains(&t) {
            ts.push(t);
        }
    }
    ts.sort_by(f32::total_cmp);
    let mut out = Vec::new();
    for pair in ts.chunks_exact(2) {
        if pair[1] - pair[0] > 1e-6 {
            out.push((a + d * pair[0], a + d * pair[1]));
        }
    }
    out
}

/// Parallel hatch segments filling `poly`: lines run along `angle_deg`
/// (screen y-down), spaced `period` px apart, anchored to the polygon's
/// bounding-box center so the pattern pans with the shape. GBW's hatch
/// pattern tiles at `2 × spacing`, so callers pass `period = 2 * spacing`.
pub(crate) fn hatch_lines(poly: &[Pos2], angle_deg: f32, period: f32) -> Vec<(Pos2, Pos2)> {
    if poly.len() < 3 || period <= 0.0 {
        return Vec::new();
    }
    let theta = angle_deg.to_radians();
    let dir = vec2(theta.cos(), theta.sin());
    let normal = vec2(-dir.y, dir.x);
    let (mut min, mut max) = (poly[0], poly[0]);
    for p in poly {
        min = min.min(*p);
        max = max.max(*p);
    }
    let center = pos2((min.x + max.x) * 0.5, (min.y + max.y) * 0.5);
    let half_diag = (max - min).length() * 0.5 + period;
    let mut lo = f32::MAX;
    let mut hi = f32::MIN;
    for p in poly {
        let s = (*p - center).dot(normal);
        lo = lo.min(s);
        hi = hi.max(s);
    }
    let mut out = Vec::new();
    let mut k = (lo / period).floor();
    while k * period <= hi {
        let base = center + normal * (k * period);
        out.extend(clip_line_to_polygon(
            base - dir * half_diag,
            base + dir * half_diag,
            poly,
        ));
        k += 1.0;
    }
    out
}

/// Ear-clipping triangulation of a simple polygon (either winding).
/// Warning polygons are user-drawn and can be concave, and egui only
/// tessellates convex fills natively.
pub(crate) fn ear_clip(poly: &[Pos2]) -> Vec<[usize; 3]> {
    let n = poly.len();
    if n < 3 {
        return Vec::new();
    }
    let signed_area: f32 = (0..n)
        .map(|i| {
            let p = poly[i];
            let q = poly[(i + 1) % n];
            p.x * q.y - q.x * p.y
        })
        .sum();
    let winding = if signed_area >= 0.0 { 1.0 } else { -1.0 };
    let cross = |o: Pos2, a: Pos2, b: Pos2| (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x);
    let mut indices: Vec<usize> = (0..n).collect();
    let mut tris = Vec::with_capacity(n - 2);
    let mut guard = 0usize;
    while indices.len() > 3 && guard < n * n {
        guard += 1;
        let m = indices.len();
        let mut clipped = false;
        for i in 0..m {
            let prev = indices[(i + m - 1) % m];
            let curr = indices[i];
            let next = indices[(i + 1) % m];
            if winding * cross(poly[prev], poly[curr], poly[next]) <= 0.0 {
                continue; // reflex corner — not an ear
            }
            let contains_other = indices.iter().any(|&j| {
                if j == prev || j == curr || j == next {
                    return false;
                }
                let p = poly[j];
                winding * cross(poly[prev], poly[curr], p) >= 0.0
                    && winding * cross(poly[curr], poly[next], p) >= 0.0
                    && winding * cross(poly[next], poly[prev], p) >= 0.0
            });
            if contains_other {
                continue;
            }
            tris.push([prev, curr, next]);
            indices.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            break; // degenerate input — fall through with what we have
        }
    }
    if indices.len() == 3 {
        tris.push([indices[0], indices[1], indices[2]]);
    }
    tris
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Pos2, b: Pos2) -> bool {
        (a - b).length() < 1e-3
    }

    #[test]
    fn catmull_rom_passes_through_control_points() {
        let pts = [
            pos2(0.0, 0.0),
            pos2(40.0, 30.0),
            pos2(90.0, -10.0),
            pos2(140.0, 5.0),
        ];
        let sm = catmull_rom(&pts, 16);
        assert_eq!(sm.len(), 3 * 16 + 1);
        // The spline interpolates every control point at span boundaries.
        for (i, p) in pts.iter().enumerate() {
            assert!(close(sm[(i * 16).min(sm.len() - 1)], *p), "point {i}");
        }
        assert!(close(*sm.last().unwrap(), pts[3]));
    }

    #[test]
    fn catmull_rom_two_points_is_a_straight_segment() {
        let sm = catmull_rom(&[pos2(0.0, 0.0), pos2(10.0, 0.0)], 8);
        assert!(close(*sm.last().unwrap(), pos2(10.0, 0.0)));
        for p in &sm {
            assert!(p.y.abs() < 1e-4 && (0.0..=10.0).contains(&p.x));
        }
    }

    #[test]
    fn sample_at_walks_arc_length_and_reports_tangent() {
        // L-shaped path: 100 px east then 100 px south.
        let pts = [pos2(0.0, 0.0), pos2(100.0, 0.0), pos2(100.0, 100.0)];
        let s = sample_at(&pts, 50.0);
        assert!(close(s.pos, pos2(50.0, 0.0)));
        assert!(s.angle.abs() < 1e-4);
        let s = sample_at(&pts, 150.0);
        assert!(close(s.pos, pos2(100.0, 50.0)));
        assert!((s.angle - std::f32::consts::FRAC_PI_2).abs() < 1e-4);
        // Past the end clamps to the last point with the final heading.
        let s = sample_at(&pts, 999.0);
        assert!(close(s.pos, pos2(100.0, 100.0)));
    }

    #[test]
    fn glyph_stations_match_gbw_spacing_rules() {
        // thickness 3 → spacing 43, first station at 23.65 (0.55×43).
        let spacing = front_pip_spacing(3.0);
        assert!((spacing - 43.0).abs() < 1e-4);
        let stations = glyph_stations(200.0, spacing);
        assert!(!stations.is_empty());
        assert!((stations[0] - 0.55 * spacing).abs() < 1e-3);
        for pair in stations.windows(2) {
            assert!((pair[1] - pair[0] - spacing).abs() < 1e-3, "even spacing");
        }
        // Nothing within 4 px of the path end.
        assert!(*stations.last().unwrap() < 200.0 - 4.0);
        // Short path → no glyphs rather than a crowded one.
        assert!(glyph_stations(10.0, spacing).is_empty());
    }

    #[test]
    fn dash_segments_cover_the_expected_on_lengths() {
        let line = [pos2(0.0, 0.0), pos2(100.0, 0.0)];
        let dashes = dash_segments(&line, &[10.0, 10.0]);
        assert_eq!(dashes.len(), 5);
        let on_total: f32 = dashes.iter().map(|d| polyline_length(d)).sum();
        assert!((on_total - 50.0).abs() < 1e-3);
        assert!(close(dashes[0][0], pos2(0.0, 0.0)));
        assert!(close(*dashes[0].last().unwrap(), pos2(10.0, 0.0)));
        assert!(close(dashes[1][0], pos2(20.0, 0.0)));

        // Dash-dot (trough) pattern across a corner keeps total on-length.
        let bent = [pos2(0.0, 0.0), pos2(50.0, 0.0), pos2(50.0, 50.0)];
        let pattern = [12.0, 5.0, 3.0, 5.0];
        let dashes = dash_segments(&bent, &pattern);
        let on_total: f32 = dashes.iter().map(|d| polyline_length(d)).sum();
        // 100 px / 25 px cycle = 4 cycles → 4×(12+3)=60 px of ink.
        assert!((on_total - 60.0).abs() < 0.1, "got {on_total}");
    }

    #[test]
    fn triangle_points_sit_on_base_and_apex() {
        // Eastbound tangent, default side (−1) → apex above (screen y-down).
        let [b1, b2, apex] = triangle_points(pos2(0.0, 0.0), 10.0, 0.0, -1.0);
        assert!(close(b1, pos2(5.5, 0.0)));
        assert!(close(b2, pos2(-5.5, 0.0)));
        assert!(close(apex, pos2(0.0, -10.0)));
        // Flipped side → apex below.
        let [_, _, apex] = triangle_points(pos2(0.0, 0.0), 10.0, 0.0, 1.0);
        assert!(close(apex, pos2(0.0, 10.0)));
    }

    #[test]
    fn semicircle_spans_the_chord_and_bulges_to_side() {
        let pts = semicircle_points(pos2(0.0, 0.0), 5.0, 0.0, -1.0, 12);
        assert!(close(pts[0], pos2(5.0, 0.0)));
        assert!(close(*pts.last().unwrap(), pos2(-5.0, 0.0)));
        // Mid sample bulges up (−y) for the default side.
        assert!(close(pts[6], pos2(0.0, -5.0)));
        for p in &pts {
            assert!((p.to_vec2().length() - 5.0).abs() < 1e-3);
        }
    }

    #[test]
    fn hatch_lines_clip_to_polygon_at_gbw_period() {
        let square = [
            pos2(0.0, 0.0),
            pos2(100.0, 0.0),
            pos2(100.0, 100.0),
            pos2(0.0, 100.0),
        ];
        // Horizontal hatch, period 20 px (GBW spacing 10 → pattern 20).
        let lines = hatch_lines(&square, 0.0, 20.0);
        assert_eq!(lines.len(), 5, "100 px tall box at 20 px period");
        for (a, b) in &lines {
            assert!((a.y - b.y).abs() < 1e-3, "horizontal lines");
            assert!(((b.x - a.x).abs() - 100.0).abs() < 1e-2, "full width");
            assert!((-1e-3..=100.001).contains(&a.y));
        }
        // Diagonal hatch stays inside the polygon.
        for (a, b) in hatch_lines(&square, 135.0, 20.0) {
            for p in [a, b] {
                assert!((-0.01..=100.01).contains(&p.x));
                assert!((-0.01..=100.01).contains(&p.y));
            }
        }
    }

    #[test]
    fn ear_clip_triangulates_convex_and_concave() {
        let square = [
            pos2(0.0, 0.0),
            pos2(10.0, 0.0),
            pos2(10.0, 10.0),
            pos2(0.0, 10.0),
        ];
        assert_eq!(ear_clip(&square).len(), 2);
        // Concave "arrow notch" pentagon.
        let concave = [
            pos2(0.0, 0.0),
            pos2(10.0, 0.0),
            pos2(10.0, 10.0),
            pos2(5.0, 4.0),
            pos2(0.0, 10.0),
        ];
        let tris = ear_clip(&concave);
        assert_eq!(tris.len(), 3);
        // Triangulation area equals polygon area regardless of winding.
        let tri_area: f32 = tris
            .iter()
            .map(|t| {
                let (a, b, c) = (concave[t[0]], concave[t[1]], concave[t[2]]);
                ((b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)).abs() * 0.5
            })
            .sum();
        let poly_area = {
            let n = concave.len();
            (0..n)
                .map(|i| {
                    let p = concave[i];
                    let q = concave[(i + 1) % n];
                    p.x * q.y - q.x * p.y
                })
                .sum::<f32>()
                .abs()
                * 0.5
        };
        assert!((tri_area - poly_area).abs() < 1e-3);
    }
}
