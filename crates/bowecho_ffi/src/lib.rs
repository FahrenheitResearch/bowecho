//! C ABI bridging the BowEcho radar engine to the iOS (SwiftUI + MapKit) app.
//!
//! The engine crates (`data_source`, `nexrad_io`, `render2d`, ...) do all the
//! real work — fetch a Level 2 volume from the NEXRAD S3 buckets, decode it, and
//! CPU-rasterize a moment into an RGBA image centered on the radar. This layer
//! just exposes that pipeline over `extern "C"` so Swift can call it.
//!
//! Threading: the fetch is blocking network I/O — Swift MUST call
//! `bowecho_render_latest` off the main thread.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::ptr;

use radar_core::{MomentType, RadarVolume};
use render2d::{ViewportRasterOptions, render_moment_viewport_rgba};

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: impl Into<String>) {
    let c = CString::new(msg.into()).unwrap_or_else(|_| CString::new("error").unwrap());
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(c));
}

/// Returns the last error message for the calling thread, or NULL. The pointer
/// is valid until the next FFI call on this thread; copy it immediately.
#[unsafe(no_mangle)]
pub extern "C" fn bowecho_last_error() -> *const c_char {
    LAST_ERROR.with(|e| match &*e.borrow() {
        Some(c) => c.as_ptr(),
        None => ptr::null(),
    })
}

/// A rendered radar frame plus the georeferencing the map overlay needs.
/// The raster is a square image centered on the radar site; `half_width_m` /
/// `half_height_m` are the ground half-extents (meters) from center to edge.
#[repr(C)]
pub struct BowEchoRender {
    pub rgba: *mut u8,
    pub len: usize,
    pub width: u32,
    pub height: u32,
    pub center_lat: f64,
    pub center_lon: f64,
    pub half_width_m: f64,
    pub half_height_m: f64,
    pub volume_time_unix: i64,
}

impl BowEchoRender {
    fn empty() -> Self {
        Self {
            rgba: ptr::null_mut(),
            len: 0,
            width: 0,
            height: 0,
            center_lat: 0.0,
            center_lon: 0.0,
            half_width_m: 0.0,
            half_height_m: 0.0,
            volume_time_unix: 0,
        }
    }
}

fn moment_from_code(code: c_int) -> MomentType {
    match code {
        1 => MomentType::Velocity,
        2 => MomentType::SpectrumWidth,
        3 => MomentType::DifferentialReflectivity,
        4 => MomentType::CorrelationCoefficient,
        5 => MomentType::DifferentialPhase,
        6 => MomentType::SpecificDifferentialPhase,
        _ => MomentType::Reflectivity,
    }
}

/// Fetch the most recent volume for `site` to `cache_dir`, returning the local
/// file path. Tries the near-live realtime chunk bucket first, then falls back
/// to the most recent archived volume.
fn fetch_latest_volume_path(site: &str, cache_dir: &Path) -> Result<PathBuf, String> {
    if let Ok(realtime) = data_source::latest_realtime_level2_volume(site) {
        match data_source::download_realtime_volume(&realtime, cache_dir) {
            Ok(downloaded) => return Ok(downloaded.path),
            Err(e) => set_last_error(format!("realtime download failed, trying archive: {e}")),
        }
    }
    let object = data_source::latest_level2_object(site, 2)
        .map_err(|e| format!("no recent archive volume for {site}: {e}"))?;
    let downloaded = data_source::download_object(data_source::LEVEL2_ARCHIVE_BUCKET, object, cache_dir)
        .map_err(|e| format!("archive download failed: {e}"))?;
    Ok(downloaded.path)
}

/// Index of the lowest elevation cut that carries the requested moment.
fn cut_index_for_moment(volume: &RadarVolume, moment: &MomentType) -> Option<usize> {
    volume.cuts.iter().position(|c| c.moments.contains_key(moment))
}

fn render_latest_inner(
    site: &str,
    moment_code: c_int,
    size_px: u32,
    cache_dir: &Path,
) -> Result<BowEchoRender, String> {
    let moment = moment_from_code(moment_code);
    let size = size_px.clamp(256, 8192);

    let path = fetch_latest_volume_path(site, cache_dir)?;
    let volume = nexrad_io::decode_volume_from_path(&path).map_err(|e| format!("decode failed: {e}"))?;

    let cut_index = cut_index_for_moment(&volume, &moment)
        .ok_or_else(|| format!("volume has no {moment:?} data"))?;

    let lat = volume
        .site
        .latitude_deg
        .ok_or("radar site has no latitude")? as f64;
    let lon = volume
        .site
        .longitude_deg
        .ok_or("radar site has no longitude")? as f64;

    // Ground extent of the chosen moment's lowest cut.
    let grid = volume.cuts[cut_index]
        .moments
        .get(&moment)
        .ok_or("moment grid missing")?;
    let max_range_m = grid.gate_range.first_gate_m as f64
        + grid.gate_range.gate_spacing_m as f64 * grid.gate_range.gate_count as f64;
    let max_range_km = (max_range_m / 1000.0).max(1.0);

    // Square raster, radar at the center, square pixels (km/px equal on both axes)
    // so the range ring stays circular. The radar's max range reaches the edge.
    let half_px = size as f32 / 2.0;
    let km_per_px = (max_range_km as f32) / half_px;
    let options = ViewportRasterOptions {
        width: size,
        height: size,
        radar_x_px: half_px,
        radar_y_px: half_px,
        km_per_px_x: km_per_px,
        km_per_px_y: km_per_px,
    };

    let (w, h, pixels) = render_moment_viewport_rgba(&volume, cut_index, moment, options)
        .map_err(|e| format!("render failed: {e}"))?;

    let half_width_m = (w as f64 / 2.0) * km_per_px as f64 * 1000.0;
    let half_height_m = (h as f64 / 2.0) * km_per_px as f64 * 1000.0;

    let boxed: Box<[u8]> = pixels.into_boxed_slice();
    let len = boxed.len();
    let rgba = Box::into_raw(boxed) as *mut u8;

    Ok(BowEchoRender {
        rgba,
        len,
        width: w,
        height: h,
        center_lat: lat,
        center_lon: lon,
        half_width_m,
        half_height_m,
        volume_time_unix: volume.volume_time.timestamp(),
    })
}

/// Fetch + decode + render the latest volume for a radar site.
///
/// `site`       — null-terminated 4-letter ICAO id (e.g. "KTLX").
/// `moment_code`— 0=Reflectivity 1=Velocity 2=SpectrumWidth 3=ZDR 4=CC 5=PHI 6=KDP.
/// `size_px`    — square raster dimension in pixels (clamped 256..=8192).
/// `cache_dir`  — null-terminated writable dir (the app's Caches directory).
/// `out`        — filled on success; on error left zeroed (call `bowecho_last_error`).
///
/// Returns 0 on success, negative on failure. BLOCKS on network — call off-main-thread.
#[unsafe(no_mangle)]
pub extern "C" fn bowecho_render_latest(
    site: *const c_char,
    moment_code: c_int,
    size_px: u32,
    cache_dir: *const c_char,
    out: *mut BowEchoRender,
) -> c_int {
    if out.is_null() {
        return -1;
    }
    unsafe { *out = BowEchoRender::empty() };

    if site.is_null() || cache_dir.is_null() {
        set_last_error("null site or cache_dir");
        return -1;
    }
    let site = match unsafe { CStr::from_ptr(site) }.to_str() {
        Ok(s) => s.to_owned(),
        Err(_) => {
            set_last_error("site is not valid UTF-8");
            return -1;
        }
    };
    let cache_dir = match unsafe { CStr::from_ptr(cache_dir) }.to_str() {
        Ok(s) => PathBuf::from(s),
        Err(_) => {
            set_last_error("cache_dir is not valid UTF-8");
            return -1;
        }
    };

    let result = catch_unwind(AssertUnwindSafe(|| {
        render_latest_inner(&site, moment_code, size_px, &cache_dir)
    }));

    match result {
        Ok(Ok(render)) => {
            unsafe { *out = render };
            0
        }
        Ok(Err(msg)) => {
            set_last_error(msg);
            -2
        }
        Err(_) => {
            set_last_error("internal panic while rendering");
            -3
        }
    }
}

/// Free the RGBA buffer owned by a `BowEchoRender` produced by this library.
#[unsafe(no_mangle)]
pub extern "C" fn bowecho_render_free(out: *mut BowEchoRender) {
    if out.is_null() {
        return;
    }
    unsafe {
        let r = &mut *out;
        if !r.rgba.is_null() && r.len > 0 {
            let slice = ptr::slice_from_raw_parts_mut(r.rgba, r.len);
            drop(Box::from_raw(slice));
        }
        *out = BowEchoRender::empty();
    }
}
