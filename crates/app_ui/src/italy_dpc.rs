//! Italy DPC raw GeoTIFF map-layer ingest.
//!
//! Radar-DPC's WMTS tiles are a presentation cache.  The scientific source for
//! BowEcho is the REST `downloadProduct` GeoTIFF: one decoded raster per product
//! timestamp, with geolocation read from GeoTIFF tags and rendered through the
//! same inverse-LUT path used by model/satellite map layers.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use tiff::ColorType;
use tiff::decoder::{Decoder as TiffDecoder, DecodingResult};

use crate::model_layer::InverseLut;

/// DPC rasters are 1 km-ish fields. Rendering the map texture at half viewport
/// resolution keeps the layer responsive while egui's linear filter does the
/// final screen resampling.
pub(crate) const ITALY_DPC_RENDER_SCALE: f32 = 0.5;

#[derive(Clone)]
pub(crate) struct ItalyDpcRasterFrame {
    pub(crate) product_time_millis: i64,
    pub(crate) period: Option<String>,
    pub(crate) identity: String,
    pub(crate) image: Arc<egui::ColorImage>,
    pub(crate) lut: Arc<InverseLut>,
    pub(crate) nx: usize,
    pub(crate) ny: usize,
    pub(crate) flip_rows: bool,
    pub(crate) generation: u64,
}

#[cfg(test)]
pub(crate) fn italy_dpc_source_key(product_type: &str, identity: &str) -> String {
    format!(
        "italy-dpc-{}-{}",
        sanitize_cache_segment(product_type),
        sanitize_cache_segment(identity)
    )
}

pub(crate) fn italy_dpc_source_generation(product_type: &str, identity: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    product_type.to_ascii_uppercase().hash(&mut hasher);
    identity.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn load_latest_frame(product_type: &str) -> Result<ItalyDpcRasterFrame, String> {
    let plan = data_source::grid_products::italy_dpc_latest_download_plan(product_type)?;
    let bytes = fetch_plan_bytes(&plan)?;
    decode_plan_bytes(&plan, &bytes)
}

fn fetch_plan_bytes(
    plan: &data_source::grid_products::ItalyDpcDownloadPlan,
) -> Result<Vec<u8>, String> {
    let cache_path = raw_cache_path(&plan.identity);
    if let Ok(bytes) = fs::read(&cache_path) {
        return Ok(bytes);
    }

    let bytes = data_source::fetch_volume_bytes(&plan.url)
        .map_err(|err| format!("Italy DPC raw {} download failed: {err}", plan.product_type))?;
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&cache_path, &bytes);
    Ok(bytes)
}

fn raw_cache_path(identity: &str) -> PathBuf {
    let root = settings::tile_cache_dir()
        .unwrap_or_else(|| std::env::temp_dir().join("bowecho-cache").join("tiles"));
    root.join("italy-dpc-raw")
        .join(format!("{}.tif", sanitize_cache_segment(identity)))
}

fn sanitize_cache_segment(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}

fn decode_plan_bytes(
    plan: &data_source::grid_products::ItalyDpcDownloadPlan,
    bytes: &[u8],
) -> Result<ItalyDpcRasterFrame, String> {
    let decoded = decode_geotiff_pixels(&plan.product_type, bytes)?;
    let (nx, ny) = (decoded.nx, decoded.ny);
    if nx == 0 || ny == 0 {
        return Err(format!(
            "Italy DPC raw {} GeoTIFF decoded with an empty raster",
            plan.product_type
        ));
    }
    let pixels = decoded.pixels;
    if pixels.len() != nx.saturating_mul(ny) {
        return Err(format!(
            "Italy DPC raw {} pixel count mismatch: got {}, expected {}",
            plan.product_type,
            pixels.len(),
            nx.saturating_mul(ny)
        ));
    }
    let image = egui::ColorImage {
        size: [nx, ny],
        source_size: egui::vec2(nx as f32, ny as f32),
        pixels,
    };

    let georef = parse_geotiff_georef(bytes, &plan.product_type)
        .unwrap_or_else(|| fallback_georef(&plan.product_type));
    let (lat, lon) = build_lat_lon_grid(&georef, nx, ny)?;
    let lut = InverseLut::build_with_shape(&lat, &lon, nx, ny).ok_or_else(|| {
        format!(
            "Italy DPC raw {} GeoTIFF has no usable geolocation",
            plan.product_type
        )
    })?;
    let generation = italy_dpc_source_generation(&plan.product_type, &plan.identity);

    Ok(ItalyDpcRasterFrame {
        product_time_millis: plan.product_time_millis,
        period: plan.period.clone(),
        identity: plan.identity.clone(),
        image: Arc::new(image),
        lut: Arc::new(lut),
        nx,
        ny,
        // GeoTIFF rows are already north-to-south in the decoded image.
        flip_rows: false,
        generation,
    })
}

struct DecodedItalyDpcPixels {
    nx: usize,
    ny: usize,
    pixels: Vec<egui::Color32>,
}

fn decode_geotiff_pixels(
    product_type: &str,
    bytes: &[u8],
) -> Result<DecodedItalyDpcPixels, String> {
    let mut decoder = TiffDecoder::new(Cursor::new(bytes))
        .map_err(|err| format!("Italy DPC raw {product_type} GeoTIFF open failed: {err}"))?;
    let (nx, ny) = decoder
        .dimensions()
        .map_err(|err| format!("Italy DPC raw {product_type} GeoTIFF dimensions failed: {err}"))?;
    let color_type = decoder
        .colortype()
        .map_err(|err| format!("Italy DPC raw {product_type} GeoTIFF color type failed: {err}"))?;
    let samples_per_pixel = usize::from(color_type.num_samples()).max(1);
    let decoded = decoder
        .read_image()
        .map_err(|err| format!("Italy DPC raw {product_type} GeoTIFF decode failed: {err}"))?;
    let values = decoding_result_to_f32(decoded);
    let nx = nx as usize;
    let ny = ny as usize;
    let pixel_count = nx
        .checked_mul(ny)
        .ok_or_else(|| format!("Italy DPC raw {product_type} GeoTIFF dimensions overflow"))?;
    let expected = pixel_count
        .checked_mul(samples_per_pixel)
        .ok_or_else(|| format!("Italy DPC raw {product_type} GeoTIFF sample count overflow"))?;
    if values.len() < expected {
        return Err(format!(
            "Italy DPC raw {product_type} GeoTIFF sample count mismatch: got {}, expected at least {expected}",
            values.len()
        ));
    }

    let alpha_index = match color_type {
        ColorType::GrayA(_) => Some(1),
        ColorType::RGBA(_) => Some(3),
        _ => None,
    };
    let mut pixels = Vec::with_capacity(pixel_count);
    for sample in values.chunks_exact(samples_per_pixel).take(pixel_count) {
        if let Some(alpha_index) = alpha_index
            && sample.get(alpha_index).copied().unwrap_or(1.0) <= 0.0
        {
            pixels.push(egui::Color32::TRANSPARENT);
            continue;
        }
        pixels.push(colorize_value(product_type, sample[0]));
    }
    Ok(DecodedItalyDpcPixels { nx, ny, pixels })
}

fn decoding_result_to_f32(decoded: DecodingResult) -> Vec<f32> {
    match decoded {
        DecodingResult::U8(values) => values.into_iter().map(f32::from).collect(),
        DecodingResult::U16(values) => values.into_iter().map(f32::from).collect(),
        DecodingResult::U32(values) => values.into_iter().map(|value| value as f32).collect(),
        DecodingResult::U64(values) => values.into_iter().map(|value| value as f32).collect(),
        DecodingResult::F16(values) => values.into_iter().map(|value| value.to_f32()).collect(),
        DecodingResult::F32(values) => values,
        DecodingResult::F64(values) => values.into_iter().map(|value| value as f32).collect(),
        DecodingResult::I8(values) => values.into_iter().map(f32::from).collect(),
        DecodingResult::I16(values) => values.into_iter().map(f32::from).collect(),
        DecodingResult::I32(values) => values.into_iter().map(|value| value as f32).collect(),
        DecodingResult::I64(values) => values.into_iter().map(|value| value as f32).collect(),
    }
}

fn colorize_value(product_type: &str, value: f32) -> egui::Color32 {
    if !value.is_finite() || value <= -9990.0 {
        return egui::Color32::TRANSPARENT;
    }
    let product = product_type.trim().to_ascii_uppercase();
    let stops = if matches!(
        product.as_str(),
        "VMI"
            | "CAPPI_1"
            | "CAPPI_2"
            | "CAPPI_3"
            | "CAPPI_4"
            | "CAPPI_5"
            | "CAPPI_6"
            | "CAPPI_7"
            | "CAPPI_8"
            | "CAPPI_9"
            | "CAPPI_10"
    ) {
        REFLECTIVITY_STOPS.as_slice()
    } else if product == "SRI" {
        RAIN_RATE_STOPS.as_slice()
    } else if matches!(
        product.as_str(),
        "SRT1" | "CUM3" | "CUM6" | "CUM12" | "CUM24"
    ) {
        ACCUMULATION_STOPS.as_slice()
    } else if product == "POH" {
        PROBABILITY_STOPS.as_slice()
    } else {
        GENERIC_STOPS.as_slice()
    };
    interpolate_stops(value, stops)
}

const REFLECTIVITY_STOPS: [(f32, [u8; 4]); 9] = [
    (0.0, [0, 0, 0, 0]),
    (5.0, [4, 70, 150, 180]),
    (15.0, [36, 140, 255, 210]),
    (25.0, [35, 210, 80, 225]),
    (35.0, [245, 225, 40, 235]),
    (45.0, [245, 130, 28, 245]),
    (55.0, [220, 35, 35, 250]),
    (65.0, [190, 55, 210, 250]),
    (75.0, [250, 250, 250, 255]),
];

const RAIN_RATE_STOPS: [(f32, [u8; 4]); 9] = [
    (0.0, [0, 0, 0, 0]),
    (0.2, [42, 82, 160, 150]),
    (1.0, [55, 145, 230, 190]),
    (2.0, [50, 210, 80, 215]),
    (5.0, [230, 230, 60, 230]),
    (10.0, [245, 150, 30, 240]),
    (25.0, [225, 45, 45, 245]),
    (50.0, [175, 45, 205, 250]),
    (100.0, [250, 250, 250, 255]),
];

const ACCUMULATION_STOPS: [(f32, [u8; 4]); 10] = [
    (0.0, [0, 0, 0, 0]),
    (0.5, [45, 95, 170, 150]),
    (1.0, [70, 150, 230, 185]),
    (5.0, [45, 205, 95, 215]),
    (10.0, [225, 230, 65, 230]),
    (20.0, [245, 170, 38, 240]),
    (50.0, [225, 55, 45, 245]),
    (100.0, [180, 45, 205, 250]),
    (200.0, [245, 245, 245, 255]),
    (400.0, [170, 170, 170, 255]),
];

const PROBABILITY_STOPS: [(f32, [u8; 4]); 6] = [
    (0.0, [0, 0, 0, 0]),
    (10.0, [60, 135, 220, 180]),
    (30.0, [70, 210, 100, 215]),
    (50.0, [235, 220, 60, 230]),
    (70.0, [240, 120, 40, 240]),
    (100.0, [220, 45, 55, 250]),
];

const GENERIC_STOPS: [(f32, [u8; 4]); 6] = [
    (0.0, [0, 0, 0, 0]),
    (1.0, [65, 120, 215, 180]),
    (5.0, [45, 205, 120, 215]),
    (10.0, [230, 225, 70, 230]),
    (25.0, [240, 120, 45, 240]),
    (50.0, [220, 45, 60, 250]),
];

fn interpolate_stops(value: f32, stops: &[(f32, [u8; 4])]) -> egui::Color32 {
    let Some(&(first_value, first_color)) = stops.first() else {
        return egui::Color32::TRANSPARENT;
    };
    if value <= first_value {
        return rgba(first_color);
    }
    for pair in stops.windows(2) {
        let (left_v, left_c) = pair[0];
        let (right_v, right_c) = pair[1];
        if value <= right_v {
            let t = ((value - left_v) / (right_v - left_v).max(f32::EPSILON)).clamp(0.0, 1.0);
            let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
            return egui::Color32::from_rgba_unmultiplied(
                lerp(left_c[0], right_c[0]),
                lerp(left_c[1], right_c[1]),
                lerp(left_c[2], right_c[2]),
                lerp(left_c[3], right_c[3]),
            );
        }
    }
    rgba(
        stops
            .last()
            .map(|(_, color)| *color)
            .unwrap_or([0, 0, 0, 0]),
    )
}

fn rgba(color: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ItalyDpcCrs {
    Geographic,
    DpcTransverseMercator,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ItalyDpcGeoRef {
    origin_x: f64,
    pixel_x: f64,
    row_x: f64,
    origin_y: f64,
    pixel_y: f64,
    row_y: f64,
    crs: ItalyDpcCrs,
}

impl ItalyDpcGeoRef {
    fn grid_to_lon_lat(self, col: f64, row: f64) -> (f32, f32) {
        let x = self.origin_x + self.pixel_x * col + self.row_x * row;
        let y = self.origin_y + self.pixel_y * col + self.row_y * row;
        match self.crs {
            ItalyDpcCrs::Geographic => (x as f32, y as f32),
            ItalyDpcCrs::DpcTransverseMercator => {
                let (lat, lon) = dpc_transverse_mercator_inverse(y, x);
                (lon as f32, lat as f32)
            }
        }
    }
}

fn build_lat_lon_grid(
    georef: &ItalyDpcGeoRef,
    nx: usize,
    ny: usize,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    let count = nx
        .checked_mul(ny)
        .ok_or_else(|| "Italy DPC GeoTIFF dimensions overflow".to_owned())?;
    let mut lat = Vec::with_capacity(count);
    let mut lon = Vec::with_capacity(count);
    for y in 0..ny {
        for x in 0..nx {
            let (lo, la) = georef.grid_to_lon_lat(x as f64 + 0.5, y as f64 + 0.5);
            lon.push(lo);
            lat.push(la);
        }
    }
    Ok((lat, lon))
}

fn product_crs(product_type: &str) -> ItalyDpcCrs {
    match product_type.trim().to_ascii_uppercase().as_str() {
        "CUM3" | "CUM6" | "CUM12" | "CUM24" => ItalyDpcCrs::Geographic,
        _ => ItalyDpcCrs::DpcTransverseMercator,
    }
}

fn fallback_georef(product_type: &str) -> ItalyDpcGeoRef {
    match product_crs(product_type) {
        ItalyDpcCrs::Geographic => ItalyDpcGeoRef {
            origin_x: 5.6,
            pixel_x: 0.01,
            row_x: 0.0,
            origin_y: 47.58,
            pixel_y: 0.0,
            row_y: -0.01,
            crs: ItalyDpcCrs::Geographic,
        },
        ItalyDpcCrs::DpcTransverseMercator => ItalyDpcGeoRef {
            origin_x: -600_000.0,
            pixel_x: 1_000.0,
            row_x: 0.0,
            origin_y: 650_000.0,
            pixel_y: 0.0,
            row_y: -1_000.0,
            crs: ItalyDpcCrs::DpcTransverseMercator,
        },
    }
}

fn parse_geotiff_georef(bytes: &[u8], product_type: &str) -> Option<ItalyDpcGeoRef> {
    let ifd = TiffIfd::parse(bytes)?;
    let crs = product_crs(product_type);
    if let Some(matrix) = ifd
        .doubles(bytes, 34264)
        .filter(|values| values.len() >= 16)
    {
        return Some(ItalyDpcGeoRef {
            origin_x: matrix[3],
            pixel_x: matrix[0],
            row_x: matrix[1],
            origin_y: matrix[7],
            pixel_y: matrix[4],
            row_y: matrix[5],
            crs,
        });
    }

    let scale = ifd.doubles(bytes, 33550)?;
    let tie = ifd.doubles(bytes, 33922)?;
    if scale.len() < 2 || tie.len() < 6 {
        return None;
    }
    let sx = scale[0];
    let sy = scale[1].abs();
    let tie_i = tie[0];
    let tie_j = tie[1];
    let tie_x = tie[3];
    let tie_y = tie[4];
    Some(ItalyDpcGeoRef {
        origin_x: tie_x - tie_i * sx,
        pixel_x: sx,
        row_x: 0.0,
        origin_y: tie_y + tie_j * sy,
        pixel_y: 0.0,
        row_y: -sy,
        crs,
    })
}

#[derive(Clone, Copy)]
enum TiffEndian {
    Little,
    Big,
}

struct TiffIfd {
    endian: TiffEndian,
    entries: Vec<TiffEntry>,
}

#[derive(Clone, Copy)]
struct TiffEntry {
    tag: u16,
    ty: u16,
    count: u32,
    value_field_offset: usize,
    value_or_offset: u32,
}

impl TiffIfd {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let endian = match &bytes[0..2] {
            b"II" => TiffEndian::Little,
            b"MM" => TiffEndian::Big,
            _ => return None,
        };
        if read_u16(bytes, 2, endian)? != 42 {
            return None;
        }
        let ifd_offset = read_u32(bytes, 4, endian)? as usize;
        let entry_count = read_u16(bytes, ifd_offset, endian)? as usize;
        let mut entries = Vec::with_capacity(entry_count);
        let mut offset = ifd_offset.checked_add(2)?;
        for _ in 0..entry_count {
            if offset.checked_add(12)? > bytes.len() {
                return None;
            }
            entries.push(TiffEntry {
                tag: read_u16(bytes, offset, endian)?,
                ty: read_u16(bytes, offset + 2, endian)?,
                count: read_u32(bytes, offset + 4, endian)?,
                value_field_offset: offset + 8,
                value_or_offset: read_u32(bytes, offset + 8, endian)?,
            });
            offset += 12;
        }
        Some(Self { endian, entries })
    }

    fn doubles(&self, bytes: &[u8], tag: u16) -> Option<Vec<f64>> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.tag == tag && entry.ty == 12)?;
        let count = entry.count as usize;
        let byte_count = count.checked_mul(8)?;
        let offset = entry.value_offset(byte_count);
        if offset.checked_add(byte_count)? > bytes.len() {
            return None;
        }
        (0..count)
            .map(|index| read_f64(bytes, offset + index * 8, self.endian))
            .collect()
    }
}

impl TiffEntry {
    fn value_offset(self, byte_count: usize) -> usize {
        if byte_count <= 4 {
            self.value_field_offset
        } else {
            self.value_or_offset as usize
        }
    }
}

fn read_u16(bytes: &[u8], offset: usize, endian: TiffEndian) -> Option<u16> {
    let chunk: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
    Some(match endian {
        TiffEndian::Little => u16::from_le_bytes(chunk),
        TiffEndian::Big => u16::from_be_bytes(chunk),
    })
}

fn read_u32(bytes: &[u8], offset: usize, endian: TiffEndian) -> Option<u32> {
    let chunk: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(match endian {
        TiffEndian::Little => u32::from_le_bytes(chunk),
        TiffEndian::Big => u32::from_be_bytes(chunk),
    })
}

fn read_f64(bytes: &[u8], offset: usize, endian: TiffEndian) -> Option<f64> {
    let chunk: [u8; 8] = bytes.get(offset..offset + 8)?.try_into().ok()?;
    Some(match endian {
        TiffEndian::Little => f64::from_le_bytes(chunk),
        TiffEndian::Big => f64::from_be_bytes(chunk),
    })
}

/// Inverse of DPC's documented WGS84 Transverse Mercator grid:
/// lat_0=42°, lon_0=12.5°, k=1, false easting/northing=0.
fn dpc_transverse_mercator_inverse(northing_m: f64, easting_m: f64) -> (f64, f64) {
    let a = 6_378_137.0_f64;
    let inv_f = 298.257_223_563_f64;
    let f = 1.0 / inv_f;
    let e2 = f * (2.0 - f);
    let ep2 = e2 / (1.0 - e2);
    let k0 = 1.0_f64;
    let lat0 = 42.0_f64.to_radians();
    let lon0 = 12.5_f64.to_radians();

    let m0 = meridional_arc(a, e2, lat0);
    let m = m0 + northing_m / k0;
    let mu = m / (a * (1.0 - e2 / 4.0 - 3.0 * e2.powi(2) / 64.0 - 5.0 * e2.powi(3) / 256.0));
    let e1 = (1.0 - (1.0 - e2).sqrt()) / (1.0 + (1.0 - e2).sqrt());
    let phi1 = mu
        + (3.0 * e1 / 2.0 - 27.0 * e1.powi(3) / 32.0) * (2.0 * mu).sin()
        + (21.0 * e1.powi(2) / 16.0 - 55.0 * e1.powi(4) / 32.0) * (4.0 * mu).sin()
        + (151.0 * e1.powi(3) / 96.0) * (6.0 * mu).sin()
        + (1097.0 * e1.powi(4) / 512.0) * (8.0 * mu).sin();

    let sin1 = phi1.sin();
    let cos1 = phi1.cos();
    let tan1 = phi1.tan();
    let n1 = a / (1.0 - e2 * sin1 * sin1).sqrt();
    let r1 = a * (1.0 - e2) / (1.0 - e2 * sin1 * sin1).powf(1.5);
    let t1 = tan1 * tan1;
    let c1 = ep2 * cos1 * cos1;
    let d = easting_m / (n1 * k0);

    let lat = phi1
        - (n1 * tan1 / r1)
            * (d.powi(2) / 2.0
                - (5.0 + 3.0 * t1 + 10.0 * c1 - 4.0 * c1.powi(2) - 9.0 * ep2) * d.powi(4) / 24.0
                + (61.0 + 90.0 * t1 + 298.0 * c1 + 45.0 * t1.powi(2)
                    - 252.0 * ep2
                    - 3.0 * c1.powi(2))
                    * d.powi(6)
                    / 720.0);
    let lon = lon0
        + (d - (1.0 + 2.0 * t1 + c1) * d.powi(3) / 6.0
            + (5.0 - 2.0 * c1 + 28.0 * t1 - 3.0 * c1.powi(2) + 8.0 * ep2 + 24.0 * t1.powi(2))
                * d.powi(5)
                / 120.0)
            / cos1;

    (lat.to_degrees(), lon.to_degrees())
}

fn meridional_arc(a: f64, e2: f64, lat: f64) -> f64 {
    a * ((1.0 - e2 / 4.0 - 3.0 * e2.powi(2) / 64.0 - 5.0 * e2.powi(3) / 256.0) * lat
        - (3.0 * e2 / 8.0 + 3.0 * e2.powi(2) / 32.0 + 45.0 * e2.powi(3) / 1024.0)
            * (2.0 * lat).sin()
        + (15.0 * e2.powi(2) / 256.0 + 45.0 * e2.powi(3) / 1024.0) * (4.0 * lat).sin()
        - (35.0 * e2.powi(3) / 3072.0) * (6.0 * lat).sin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_identity_is_product_and_raw_file_specific() {
        let first = italy_dpc_source_key("CUM12", "italy-dpc/dpc-radar/CUM12/foo.tif");
        let second = italy_dpc_source_key("CUM24", "italy-dpc/dpc-radar/CUM24/foo.tif");
        assert_ne!(first, second);
        assert_ne!(
            italy_dpc_source_generation("CUM12", "italy-dpc/dpc-radar/CUM12/foo.tif"),
            italy_dpc_source_generation("CUM24", "italy-dpc/dpc-radar/CUM24/foo.tif")
        );
    }

    #[test]
    fn source_generation_is_zoom_invariant() {
        let identity = "italy-dpc/dpc-radar/VMI/22-09-2025-14-20.tif";
        let base = italy_dpc_source_generation("VMI", identity);
        for _map_scale in [20.0_f32, 52.0, 300.0, 1200.0] {
            assert_eq!(base, italy_dpc_source_generation("VMI", identity));
        }
    }

    #[test]
    fn dpc_transverse_mercator_inverse_matches_documented_grid() {
        let (lat, lon) = dpc_transverse_mercator_inverse(650_000.0, -600_000.0);
        assert!((lat - 47.5709).abs() < 0.03, "lat {lat}");
        assert!((lon - 4.5234).abs() < 0.03, "lon {lon}");

        let (lat, lon) = dpc_transverse_mercator_inverse(-50_000.0, 0.0);
        assert!((lat - 41.5498).abs() < 0.02, "lat {lat}");
        assert!((lon - 12.5).abs() < 0.001, "lon {lon}");
    }

    #[test]
    fn cum_products_use_geographic_grid() {
        let georef = fallback_georef("CUM24");
        assert_eq!(georef.crs, ItalyDpcCrs::Geographic);
        let (lon, lat) = georef.grid_to_lon_lat(0.0, 0.0);
        assert!((lon - 5.6).abs() < 1e-6);
        assert!((lat - 47.58).abs() < 1e-6);
        let (lon, lat) = georef.grid_to_lon_lat(1341.0, 1233.0);
        assert!((lon - 19.01).abs() < 1e-4, "lon {lon}");
        assert!((lat - 35.25).abs() < 1e-4, "lat {lat}");
    }

    #[test]
    fn parses_classic_geotiff_tiepoint_and_pixel_scale() {
        let bytes = sample_tiff_with_scale_and_tiepoint();
        let georef = parse_geotiff_georef(&bytes, "CUM24").expect("georef");
        assert_eq!(georef.crs, ItalyDpcCrs::Geographic);
        assert_eq!(georef.origin_x, 5.6);
        assert_eq!(georef.origin_y, 47.58);
        assert_eq!(georef.pixel_x, 0.01);
        assert_eq!(georef.row_y, -0.01);
    }

    #[test]
    #[ignore = "live Italy DPC endpoint and raw GeoTIFF download"]
    fn live_vmi_raw_geotiff_decodes() {
        let frame = load_latest_frame("VMI").expect("live VMI raw GeoTIFF");
        assert!(frame.nx >= 1000, "nx {}", frame.nx);
        assert!(frame.ny >= 1000, "ny {}", frame.ny);
        let non_empty = frame
            .image
            .pixels
            .iter()
            .filter(|pixel| **pixel != egui::Color32::TRANSPARENT)
            .count();
        assert!(non_empty > 100, "non-empty pixels {non_empty}");
    }

    fn sample_tiff_with_scale_and_tiepoint() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&8u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());

        let scale_offset = 8 + 2 + 2 * 12 + 4;
        let tie_offset = scale_offset + 3 * 8;
        push_ifd_entry(&mut bytes, 33550, 12, 3, scale_offset as u32);
        push_ifd_entry(&mut bytes, 33922, 12, 6, tie_offset as u32);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for value in [0.01_f64, 0.01, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0.0_f64, 0.0, 0.0, 5.6, 47.58, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn push_ifd_entry(bytes: &mut Vec<u8>, tag: u16, ty: u16, count: u32, offset: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&ty.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
    }
}
