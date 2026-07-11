//! NEXRAD Level III Product 48 (VAD Wind Profile) decoder.
//!
//! This is a Rust port of Drew's standalone `nexrad-vwp-parser.js`.  It keeps
//! the tabular-block-first/symbology-fallback behavior, while fixing two
//! timeline bugs in the original utility: NEXRAD Julian day one is
//! 1970-01-01 (not 1970-01-02), and rolling HHMM columns must be anchored to
//! the volume date before they are ordered across midnight.

use std::collections::HashMap;
use std::io::Read;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use flate2::read::ZlibDecoder;

use crate::{NexradError, Result};

const MESSAGE_CODE_VWP: u16 = 48;
const PACKET_WIND_BARB: u16 = 0x0004;
const PACKET_TEXT: u16 = 0x0008;
const PACKET_VECTOR: u16 = 0x000a;
const EFFECTIVE_EARTH_RADIUS_KM: f64 = (4.0 / 3.0) * 6_371.0;
const MAX_BLOCK_BYTES: usize = 500_000;
const MAX_INFLATED_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VwpOperatingMode {
    ClearAir,
    Precipitation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VwpSource {
    Tabular,
    Symbology,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VwpRadar {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub height_ft: i16,
    pub vcp: u16,
    pub mode: VwpOperatingMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VwpScan {
    pub volume_time: DateTime<Utc>,
    pub generation_time: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VwpMetadata {
    pub rms_threshold_kts: Option<f64>,
    pub symmetry_threshold_kts: Option<f64>,
    pub data_points_threshold: Option<u32>,
    pub optimum_slant_range_nm: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VwpLevel {
    pub altitude_km_agl: f64,
    pub altitude_ft_msl: Option<i32>,
    pub direction_deg: f64,
    pub speed_kts: f64,
    pub rms_kts: Option<f64>,
    pub divergence: Option<f64>,
    pub slant_range_nm: Option<f64>,
    pub elevation_angle_deg: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VwpProfile {
    /// Original four-digit UTC label printed in the Level III product.
    pub label_hhmm: String,
    /// Absolute time inferred from the rolling label and volume scan time.
    pub valid_time: DateTime<Utc>,
    pub levels: Vec<VwpLevel>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VwpProduct {
    pub radar: VwpRadar,
    pub scan: VwpScan,
    pub source: VwpSource,
    /// Newest profile first, including correct ordering across midnight.
    pub profiles: Vec<VwpProfile>,
    pub metadata: VwpMetadata,
}

#[derive(Clone, Copy)]
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn len(self) -> usize {
        self.bytes.len()
    }

    fn u16(self, offset: usize) -> Option<u16> {
        let bytes: [u8; 2] = self
            .bytes
            .get(offset..offset.checked_add(2)?)?
            .try_into()
            .ok()?;
        Some(u16::from_be_bytes(bytes))
    }

    fn i16(self, offset: usize) -> Option<i16> {
        let bytes: [u8; 2] = self
            .bytes
            .get(offset..offset.checked_add(2)?)?
            .try_into()
            .ok()?;
        Some(i16::from_be_bytes(bytes))
    }

    fn u32(self, offset: usize) -> Option<u32> {
        let bytes: [u8; 4] = self
            .bytes
            .get(offset..offset.checked_add(4)?)?
            .try_into()
            .ok()?;
        Some(u32::from_be_bytes(bytes))
    }

    fn i32(self, offset: usize) -> Option<i32> {
        let bytes: [u8; 4] = self
            .bytes
            .get(offset..offset.checked_add(4)?)?
            .try_into()
            .ok()?;
        Some(i32::from_be_bytes(bytes))
    }

    fn ascii(self, offset: usize, len: usize) -> Option<String> {
        let bytes = self.bytes.get(offset..offset.checked_add(len)?)?;
        Some(
            bytes
                .iter()
                .map(|byte| {
                    if (0x20..0x7f).contains(byte) {
                        char::from(*byte)
                    } else {
                        ' '
                    }
                })
                .collect(),
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct ProductDescription {
    latitude_deg: f64,
    longitude_deg: f64,
    height_ft: i16,
    operating_mode: u16,
    vcp: u16,
    volume_date: u16,
    volume_seconds: u32,
    generation_date: u16,
    generation_seconds: u32,
}

#[derive(Clone, Debug)]
struct RawWind {
    time: String,
    level: VwpLevel,
}

#[derive(Default)]
struct TabularResult {
    winds: Vec<RawWind>,
    metadata: VwpMetadata,
}

#[derive(Clone, Debug)]
struct TextLabel {
    i: i16,
    j: i16,
    text: String,
}

#[derive(Clone, Debug)]
struct WindBarb {
    color: u16,
    i: i16,
    j: i16,
    direction_deg: u16,
    speed_kts: u16,
}

/// Parse raw NEXRAD Level III Product 48 bytes.
///
/// WMO/AWIPS text headers and zlib-wrapped products are accepted.  The
/// tabular block is preferred because it carries full VAD diagnostics; the
/// symbology wind barbs are used when the tabular block has no wind rows.
pub fn decode_level3_vwp(raw: &[u8]) -> Result<VwpProduct> {
    let normalized = unwrap(raw)?;
    let reader = Reader::new(&normalized);
    if reader.len() < 120 {
        return invalid(
            0,
            format!(
                "buffer too small for Product 48: {} bytes (need at least 120)",
                reader.len()
            ),
        );
    }
    if reader.u16(0) != Some(MESSAGE_CODE_VWP) {
        return invalid(
            0,
            format!(
                "not a VWP product (message code {}, expected 48)",
                reader.u16(0).unwrap_or_default()
            ),
        );
    }

    let pdb = parse_product_description(reader, 18)?;
    let volume_time = nexrad_datetime(pdb.volume_date, pdb.volume_seconds)?;
    let generation_time = nexrad_datetime(pdb.generation_date, pdb.generation_seconds)?;

    let tabular = find_block(reader, 3)
        .map(|offset| parse_tabular(reader, offset))
        .unwrap_or_default();
    let symbology = find_block(reader, 1)
        .map(|offset| parse_symbology(reader, offset, pdb.height_ft))
        .unwrap_or_default();
    let (source, winds) = if tabular.winds.is_empty() {
        (VwpSource::Symbology, symbology)
    } else {
        (VwpSource::Tabular, tabular.winds)
    };

    let profiles = group_profiles(winds, volume_time);
    if profiles.is_empty() {
        return invalid(0, "Product 48 contains no decodable wind levels");
    }

    Ok(VwpProduct {
        radar: VwpRadar {
            latitude_deg: pdb.latitude_deg,
            longitude_deg: pdb.longitude_deg,
            height_ft: pdb.height_ft,
            vcp: pdb.vcp,
            mode: if pdb.operating_mode == 1 {
                VwpOperatingMode::ClearAir
            } else {
                VwpOperatingMode::Precipitation
            },
        },
        scan: VwpScan {
            volume_time,
            generation_time,
        },
        source,
        profiles,
        metadata: tabular.metadata,
    })
}

fn invalid<T>(offset: usize, reason: impl Into<String>) -> Result<T> {
    Err(NexradError::InvalidMessage {
        offset,
        reason: reason.into(),
    })
}

fn unwrap(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.len() < 4 {
        return invalid(0, format!("Product 48 input is only {} bytes", raw.len()));
    }

    let first_header_end = find_text_header_end(raw);
    let mut payload = if first_header_end > 0 {
        raw.get(first_header_end..).unwrap_or_default()
    } else {
        raw
    };
    if payload.is_empty() {
        return invalid(first_header_end, "WMO/AWIPS header has no product payload");
    }

    let mut inflated = Vec::new();
    if is_zlib(payload) {
        let mut decoder = ZlibDecoder::new(payload).take(MAX_INFLATED_BYTES + 1);
        decoder
            .read_to_end(&mut inflated)
            .map_err(|error| NexradError::Compression(format!("Product 48 zlib: {error}")))?;
        if inflated.len() as u64 > MAX_INFLATED_BYTES {
            return Err(NexradError::Compression(format!(
                "Product 48 expands beyond {} bytes",
                MAX_INFLATED_BYTES
            )));
        }
        payload = &inflated;
    }

    if Reader::new(payload).u16(0) == Some(MESSAGE_CODE_VWP) {
        return Ok(payload.to_vec());
    }
    let Some(offset) = find_message_start(payload) else {
        return invalid(0, "could not locate a Product 48 message header");
    };
    Ok(payload[offset..].to_vec())
}

fn is_zlib(bytes: &[u8]) -> bool {
    matches!(bytes, [0x78, 0xda | 0x9c | 0x01, ..])
}

fn find_text_header_end(bytes: &[u8]) -> usize {
    let mut last = 0;
    let limit = bytes.len().saturating_sub(2).min(300);
    for index in 0..limit {
        if bytes.get(index..index + 3) == Some(b"\r\r\n") {
            last = index + 3;
            if matches!(bytes.get(last), Some(0x78 | 0x00)) {
                return last;
            }
        }
    }
    last
}

fn find_message_start(bytes: &[u8]) -> Option<usize> {
    let reader = Reader::new(bytes);
    for offset in 0..bytes.len().saturating_sub(20) {
        if reader.u16(offset) != Some(MESSAGE_CODE_VWP) {
            continue;
        }
        let date = reader.u16(offset + 2)?;
        if (10_001..35_000).contains(&date) && reader.u16(offset + 18) == Some(0xffff) {
            return Some(offset);
        }
    }
    None
}

fn parse_product_description(reader: Reader<'_>, offset: usize) -> Result<ProductDescription> {
    let get_u16 = |relative| {
        reader
            .u16(offset + relative)
            .ok_or_else(|| NexradError::Truncated {
                what: "Product 48 description block",
                offset: offset + relative,
                needed: 2,
                available: reader.len().saturating_sub(offset + relative),
            })
    };
    let get_u32 = |relative| {
        reader
            .u32(offset + relative)
            .ok_or_else(|| NexradError::Truncated {
                what: "Product 48 description block",
                offset: offset + relative,
                needed: 4,
                available: reader.len().saturating_sub(offset + relative),
            })
    };
    let latitude = reader
        .i32(offset + 2)
        .ok_or_else(|| NexradError::Truncated {
            what: "Product 48 radar latitude",
            offset: offset + 2,
            needed: 4,
            available: reader.len().saturating_sub(offset + 2),
        })?;
    let longitude = reader
        .i32(offset + 6)
        .ok_or_else(|| NexradError::Truncated {
            what: "Product 48 radar longitude",
            offset: offset + 6,
            needed: 4,
            available: reader.len().saturating_sub(offset + 6),
        })?;
    let height_ft = reader
        .i16(offset + 10)
        .ok_or_else(|| NexradError::Truncated {
            what: "Product 48 radar height",
            offset: offset + 10,
            needed: 2,
            available: reader.len().saturating_sub(offset + 10),
        })?;

    Ok(ProductDescription {
        latitude_deg: f64::from(latitude) / 1_000.0,
        longitude_deg: f64::from(longitude) / 1_000.0,
        height_ft,
        operating_mode: get_u16(14)?,
        vcp: get_u16(16)?,
        volume_date: get_u16(22)?,
        volume_seconds: get_u32(24)?,
        generation_date: get_u16(28)?,
        generation_seconds: get_u32(30)?,
    })
}

fn find_block(reader: Reader<'_>, block_id: u16) -> Option<usize> {
    for offset in (0..reader.len().saturating_sub(8)).step_by(2) {
        if reader.u16(offset) != Some(0xffff) || reader.u16(offset + 2) != Some(block_id) {
            continue;
        }
        let length = usize::try_from(reader.u32(offset + 4)?).ok()?;
        // The ICD length field is not consistent across old archive
        // generations about whether the eight-byte block header is included.
        // Sub-parsers clamp to the actual buffer, so accept either convention.
        if (1..MAX_BLOCK_BYTES).contains(&length) {
            return Some(offset);
        }
    }
    None
}

fn parse_symbology(reader: Reader<'_>, offset: usize, radar_height_ft: i16) -> Vec<RawWind> {
    let Some(block_length) = reader
        .u32(offset + 4)
        .and_then(|value| usize::try_from(value).ok())
    else {
        return Vec::new();
    };
    let block_end = offset
        .saturating_add(8)
        .saturating_add(block_length)
        .min(reader.len());
    let mut position = offset.saturating_add(16);
    let mut barbs = Vec::new();
    let mut labels = Vec::new();
    let mut vector_packets: Vec<Vec<i16>> = Vec::new();

    while position.saturating_add(4) <= block_end {
        let Some(code) = reader.u16(position) else {
            break;
        };
        let Some(length) = reader.u16(position + 2).map(usize::from) else {
            break;
        };
        let data = position + 4;
        let Some(packet_end) = data.checked_add(length) else {
            break;
        };
        if length == 0 || length > 50_000 || packet_end > block_end {
            break;
        }

        match code {
            PACKET_WIND_BARB if length == 10 => {
                if let (Some(color), Some(i), Some(j), Some(direction_deg), Some(speed_kts)) = (
                    reader.u16(data),
                    reader.i16(data + 2),
                    reader.i16(data + 4),
                    reader.u16(data + 6),
                    reader.u16(data + 8),
                ) {
                    barbs.push(WindBarb {
                        color,
                        i,
                        j,
                        direction_deg,
                        speed_kts,
                    });
                }
            }
            PACKET_TEXT if length >= 8 => {
                if let (Some(i), Some(j), Some(text)) = (
                    reader.i16(data + 2),
                    reader.i16(data + 4),
                    reader.ascii(data + 6, length - 6),
                ) {
                    labels.push(TextLabel {
                        i,
                        j,
                        text: text.trim().to_owned(),
                    });
                }
            }
            PACKET_VECTOR if length >= 2 => {
                let mut starts = Vec::new();
                let mut vector = data + 2;
                while vector.saturating_add(8) <= packet_end {
                    if let Some(j_start) = reader.i16(vector + 2) {
                        starts.push(j_start);
                    }
                    vector += 8;
                }
                vector_packets.push(starts);
            }
            _ => {}
        }
        position = packet_end;
    }

    let altitude_labels: Vec<i32> = labels
        .iter()
        .filter(|label| label.i == 31)
        .filter_map(|label| label.text.parse::<i32>().ok())
        .filter(|altitude| (1..=70).contains(altitude))
        .collect();
    let mut altitude_map = HashMap::<i16, i32>::new();
    if let Some(ticks) = vector_packets.get(1)
        && ticks.len() == altitude_labels.len()
    {
        for (tick, altitude) in ticks.iter().rev().zip(&altitude_labels) {
            altitude_map.insert(*tick, *altitude);
        }
    }
    if altitude_map.is_empty() {
        for label in labels.iter().filter(|label| label.i == 31) {
            if let Ok(altitude) = label.text.parse::<i32>()
                && (1..=70).contains(&altitude)
            {
                altitude_map.insert(label.j, altitude);
            }
        }
    }

    let time_j = labels
        .iter()
        .find(|label| label.text == "TIME")
        .map_or(490, |label| label.j);
    let time_map: HashMap<i16, String> = labels
        .iter()
        .filter(|label| is_hhmm(&label.text) && (i32::from(label.j) - i32::from(time_j)).abs() < 20)
        .map(|label| (label.i, label.text.clone()))
        .collect();
    let time_positions: Vec<i16> = time_map.keys().copied().collect();
    let altitude_positions: Vec<i16> = altitude_map.keys().copied().collect();

    barbs
        .into_iter()
        .filter_map(|barb| {
            let time_position = nearest(&time_positions, barb.i, 30)?;
            let altitude_position = nearest(&altitude_positions, barb.j, 20)?;
            let time = time_map.get(&time_position)?.clone();
            let altitude_kft = *altitude_map.get(&altitude_position)?;
            let altitude_km_agl =
                (f64::from(altitude_kft) - f64::from(radar_height_ft) / 1_000.0) / 3.281;
            Some(RawWind {
                time,
                level: VwpLevel {
                    altitude_km_agl: (altitude_km_agl * 1_000.0).round() / 1_000.0,
                    altitude_ft_msl: Some(altitude_kft * 1_000),
                    direction_deg: f64::from(barb.direction_deg),
                    speed_kts: f64::from(barb.speed_kts),
                    rms_kts: rms_for_color(barb.color),
                    divergence: None,
                    slant_range_nm: None,
                    elevation_angle_deg: None,
                },
            })
        })
        .collect()
}

fn rms_for_color(color: u16) -> Option<f64> {
    match color {
        1 => Some(2.0),
        2 => Some(6.0),
        3 => Some(10.0),
        4 => Some(14.0),
        5 => Some(18.0),
        _ => None,
    }
}

fn nearest(values: &[i16], target: i16, max_distance: i32) -> Option<i16> {
    values
        .iter()
        .copied()
        .min_by_key(|value| (i32::from(*value) - i32::from(target)).abs())
        .filter(|value| (i32::from(*value) - i32::from(target)).abs() <= max_distance)
}

fn parse_tabular(reader: Reader<'_>, offset: usize) -> TabularResult {
    let Some(block_length) = reader
        .u32(offset + 4)
        .and_then(|value| usize::try_from(value).ok())
    else {
        return TabularResult::default();
    };
    let block_end = offset
        .saturating_add(8)
        .saturating_add(block_length)
        .min(reader.len());
    // block header + duplicate message/PDB + divider
    let mut position = offset.saturating_add(130);
    let Some(page_count) = reader.i16(position) else {
        return TabularResult::default();
    };
    position += 2;
    if !(1..=100).contains(&page_count) {
        return TabularResult::default();
    }

    let mut pages = Vec::<Vec<String>>::new();
    let mut page = Vec::new();
    while position.saturating_add(2) <= block_end {
        let Some(character_count) = reader.i16(position) else {
            break;
        };
        position += 2;
        if character_count == -1 {
            if !page.is_empty() {
                pages.push(std::mem::take(&mut page));
            }
            continue;
        }
        if !(1..=200).contains(&character_count) {
            break;
        }
        let count = usize::try_from(character_count).unwrap_or_default();
        let Some(line) = reader.ascii(position, count) else {
            break;
        };
        position += count;
        page.push(line);
    }
    if !page.is_empty() {
        pages.push(page);
    }

    let mut result = TabularResult::default();
    for page in pages {
        if page
            .first()
            .is_some_and(|line| line.contains("VAD Algorithm Output"))
        {
            let time = page
                .first()
                .and_then(|line| hhmm_from_clock_text(line))
                .unwrap_or_else(|| "unknown".to_owned());
            for line in page.iter().skip(3) {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('-') {
                    continue;
                }
                let values: Vec<&str> = trimmed.split_whitespace().collect();
                if values.len() < 10 {
                    continue;
                }
                let (Ok(direction_deg), Ok(speed_kts), Ok(slant_range_nm), Ok(elevation_deg)) = (
                    values[4].parse::<f64>(),
                    values[5].parse::<f64>(),
                    values[8].parse::<f64>(),
                    values[9].parse::<f64>(),
                ) else {
                    continue;
                };
                let slant_km = slant_range_nm * 6_067.1 / 3_281.0;
                let elevation_rad = elevation_deg.to_radians();
                let altitude_km_agl = (EFFECTIVE_EARTH_RADIUS_KM.powi(2)
                    + slant_km.powi(2)
                    + 2.0 * EFFECTIVE_EARTH_RADIUS_KM * slant_km * elevation_rad.sin())
                .sqrt()
                    - EFFECTIVE_EARTH_RADIUS_KM;
                result.winds.push(RawWind {
                    time: time.clone(),
                    level: VwpLevel {
                        altitude_km_agl: (altitude_km_agl * 1_000.0).round() / 1_000.0,
                        altitude_ft_msl: values[0]
                            .parse::<i32>()
                            .ok()
                            .and_then(|value| value.checked_mul(100)),
                        direction_deg,
                        speed_kts,
                        rms_kts: values[6].parse().ok(),
                        divergence: (values[7] != "NA")
                            .then(|| values[7].parse().ok())
                            .flatten(),
                        slant_range_nm: Some(slant_range_nm),
                        elevation_angle_deg: Some(elevation_deg),
                    },
                });
            }
        }

        // Some archive products split a label and its value across adjacent
        // tabular records. Joining matches the logical page represented by the
        // original parser while keeping the bounded record decoder above.
        let upper = page.join("\n").to_ascii_uppercase();
        if let Some(value) = number_after(&upper, "RMS THRESHOLD") {
            result.metadata.rms_threshold_kts = Some(value);
        }
        if let Some(value) = number_after(&upper, "SYMMETRY THRESHOLD") {
            result.metadata.symmetry_threshold_kts = Some(value);
        }
        if let Some(value) = integer_after(&upper, "DATA POINTS THRESHOLD") {
            result.metadata.data_points_threshold = Some(value);
        }
        if let Some(value) = number_after(&upper, "OPTIMUM SLANT RANGE") {
            result.metadata.optimum_slant_range_nm = Some(value);
        }
    }
    result
}

fn number_after(line: &str, phrase: &str) -> Option<f64> {
    line.split_once(phrase)?
        .1
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn integer_after(line: &str, phrase: &str) -> Option<u32> {
    line.split_once(phrase)?
        .1
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn hhmm_from_clock_text(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for index in 0..bytes.len().saturating_sub(4) {
        let candidate = bytes.get(index..index + 5)?;
        if candidate[0].is_ascii_digit()
            && candidate[1].is_ascii_digit()
            && candidate[2] == b':'
            && candidate[3].is_ascii_digit()
            && candidate[4].is_ascii_digit()
        {
            let hhmm = format!(
                "{}{}{}{}",
                char::from(candidate[0]),
                char::from(candidate[1]),
                char::from(candidate[3]),
                char::from(candidate[4])
            );
            if is_hhmm(&hhmm) {
                return Some(hhmm);
            }
        }
    }
    None
}

fn is_hhmm(text: &str) -> bool {
    if text.len() != 4 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let hour = text[0..2].parse::<u32>().ok();
    let minute = text[2..4].parse::<u32>().ok();
    matches!((hour, minute), (Some(0..=23), Some(0..=59)))
}

fn group_profiles(winds: Vec<RawWind>, volume_time: DateTime<Utc>) -> Vec<VwpProfile> {
    let mut grouped = HashMap::<String, Vec<VwpLevel>>::new();
    for wind in winds {
        grouped.entry(wind.time).or_default().push(wind.level);
    }
    let mut profiles: Vec<_> = grouped
        .into_iter()
        .filter_map(|(label_hhmm, mut levels)| {
            let valid_time = anchor_hhmm(&label_hhmm, volume_time)?;
            levels.sort_by(|left, right| left.altitude_km_agl.total_cmp(&right.altitude_km_agl));
            Some(VwpProfile {
                label_hhmm,
                valid_time,
                levels,
            })
        })
        .collect();
    profiles.sort_by(|left, right| {
        right
            .valid_time
            .cmp(&left.valid_time)
            .then_with(|| right.label_hhmm.cmp(&left.label_hhmm))
    });
    profiles
}

fn anchor_hhmm(label: &str, reference: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if !is_hhmm(label) {
        return None;
    }
    let hour = label[0..2].parse().ok()?;
    let minute = label[2..4].parse().ok()?;
    let base = reference.date_naive();
    [-1_i64, 0, 1]
        .into_iter()
        .filter_map(|offset| base.checked_add_signed(Duration::days(offset)))
        .filter_map(|date| date.and_hms_opt(hour, minute, 0))
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
        .min_by_key(|candidate| {
            let distance = candidate
                .signed_duration_since(reference)
                .num_seconds()
                .unsigned_abs();
            // Prefer the non-future candidate for an exact-distance tie.
            (distance, u8::from(*candidate > reference))
        })
}

fn nexrad_datetime(julian_date: u16, seconds: u32) -> Result<DateTime<Utc>> {
    if julian_date == 0 || seconds >= 86_400 {
        return invalid(
            0,
            format!("invalid NEXRAD date/seconds pair {julian_date}/{seconds}"),
        );
    }
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .expect("Unix epoch is valid");
    let naive = epoch
        .checked_add_signed(Duration::days(i64::from(julian_date) - 1))
        .and_then(|date| date.checked_add_signed(Duration::seconds(i64::from(seconds))))
        .ok_or_else(|| NexradError::InvalidMessage {
            offset: 0,
            reason: format!("NEXRAD timestamp {julian_date}/{seconds} is out of range"),
        })?;
    Utc.from_local_datetime(&naive)
        .single()
        .ok_or_else(|| NexradError::InvalidMessage {
            offset: 0,
            reason: format!("NEXRAD timestamp {julian_date}/{seconds} is ambiguous"),
        })
}

#[cfg(test)]
mod tests {
    use chrono::{Datelike, Timelike};

    use super::*;

    const KBMX_19980416: &[u8] =
        include_bytes!("../tests/data/nexrad_vwp/KBMX_SDUS54_NVWBMX_199804160006");

    #[test]
    fn decodes_real_kbmx_product_48_and_corrects_timeline() {
        let product = decode_level3_vwp(KBMX_19980416).expect("real KBMX Product 48");

        assert!((product.radar.latitude_deg - 33.172).abs() < 0.0001);
        assert!((product.radar.longitude_deg + 86.770).abs() < 0.0001);
        assert_eq!(product.radar.height_ft, 759);
        assert_eq!(product.radar.vcp, 11);
        assert_eq!(product.radar.mode, VwpOperatingMode::Precipitation);
        assert_eq!(product.source, VwpSource::Symbology);

        let volume = product.scan.volume_time;
        assert_eq!((volume.year(), volume.month(), volume.day()), (1998, 4, 16));
        assert_eq!(
            (volume.hour(), volume.minute(), volume.second()),
            (0, 6, 45)
        );
        let generation = product.scan.generation_time;
        assert_eq!(
            (generation.year(), generation.month(), generation.day()),
            (1998, 4, 16)
        );
        assert_eq!(
            (generation.hour(), generation.minute(), generation.second()),
            (0, 11, 19)
        );

        let labels: Vec<&str> = product
            .profiles
            .iter()
            .map(|profile| profile.label_hhmm.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "0006", "0001", "2355", "2350", "2345", "2340", "2335", "2330", "2325", "2319",
                "2314"
            ]
        );
        assert_eq!(product.profiles.len(), 11);
        assert_eq!(
            product
                .profiles
                .iter()
                .map(|profile| profile.levels.len())
                .sum::<usize>(),
            60
        );
        assert_eq!(product.profiles[0].levels.len(), 5);
        assert_eq!(product.profiles[1].levels.len(), 5);
        assert_eq!(product.profiles[2].levels.len(), 5);
        assert_eq!(product.profiles.last().unwrap().levels.len(), 6);
        assert_eq!(product.profiles[0].valid_time.day(), 16);
        assert_eq!(product.profiles[2].valid_time.day(), 15);

        assert_eq!(product.metadata.rms_threshold_kts, Some(9.7));
        assert_eq!(product.metadata.symmetry_threshold_kts, Some(13.6));
        assert_eq!(product.metadata.data_points_threshold, Some(25));
        assert_eq!(product.metadata.optimum_slant_range_nm, Some(16.2));

        let oldest = product.profiles.last().unwrap();
        let first = &oldest.levels[0];
        assert!((first.altitude_km_agl - 0.073).abs() < 0.001);
        assert_eq!(first.altitude_ft_msl, Some(1_000));
        assert_eq!(first.direction_deg, 182.0);
        assert_eq!(first.speed_kts, 12.0);
        assert_eq!(first.rms_kts, Some(6.0));
    }

    #[test]
    fn julian_day_one_is_unix_epoch_not_the_following_day() {
        let time = nexrad_datetime(1, 0).unwrap();
        assert_eq!((time.year(), time.month(), time.day()), (1970, 1, 1));
    }

    #[test]
    fn rolling_hhmm_labels_anchor_across_midnight() {
        let reference = Utc.with_ymd_and_hms(1998, 4, 16, 0, 6, 45).unwrap();
        assert_eq!(anchor_hhmm("0006", reference).unwrap().day(), 16);
        assert_eq!(anchor_hhmm("2355", reference).unwrap().day(), 15);
    }

    #[test]
    fn rejects_non_vwp_input() {
        let error = decode_level3_vwp(&[0; 120]).unwrap_err();
        assert!(error.to_string().contains("Product 48"));
    }
}
