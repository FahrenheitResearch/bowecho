//! NEXRAD Archive II / Level II decoder entry points.
//!
//! This first slice focuses on the modern Message Type 31 radial format and
//! keeps unsupported records non-fatal so an app can inspect partially decoded
//! volumes while the edge-case corpus grows.

pub mod cfradial;
pub mod dorade;
pub mod hdf5lite;
pub mod mobile_archive;
pub mod netcdf3;
pub mod odim;

use std::cell::UnsafeCell;
use std::collections::btree_map::Entry;
use std::fs;
use std::io::{Cursor, Read};
use std::mem::MaybeUninit;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use bzip2::bufread::BzDecoder;
use chrono::{DateTime, TimeZone, Utc};
use flate2::read::GzDecoder;
use radar_core::{
    GateRange, MomentGrid, MomentType, RadarSite, RadarVolume, Radial, RadialStatus, VcpInfo,
};
use rayon::prelude::*;
use thiserror::Error;

const VOLUME_HEADER_LEN: usize = 24;
const CONTROL_WORD_LEN: usize = 12;
const MESSAGE_HEADER_LEN: usize = 16;
const RECORD_BYTES: usize = 2432;
const MSG_31_HEADER_LEN: usize = 72;
const GENERIC_DATA_BLOCK_LEN: usize = 28;
const VOLUME_CONSTANT_BLOCK_LEN: usize = 44;
const RADIAL_CONSTANT_BLOCK_LEN: usize = 20;
const HALF_DEGREE_RADIALS_PER_CUT: usize = 720;
const ONE_DEGREE_RADIALS_PER_CUT: usize = 360;
const FALLBACK_RADIALS_PER_CUT: usize = 760;
const MAX_MESSAGE_31_MOMENTS: usize = 10;
const BZIP_BLOCK_DECODE_CAPACITY_HINT: usize = RECORD_BYTES * 102;
const GZIP_TRAILER_LEN: usize = 8;
const MAX_GZIP_PREALLOC_RATIO: usize = 128;

pub type Result<T> = std::result::Result<T, NexradError>;

#[derive(Debug, Error)]
pub enum NexradError {
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("input is too short for an Archive II volume header: {actual} bytes")]
    ShortVolumeHeader { actual: usize },
    #[error("truncated {what} at offset {offset}: need {needed} bytes, have {available}")]
    Truncated {
        what: &'static str,
        offset: usize,
        needed: usize,
        available: usize,
    },
    #[error("unsupported or corrupt compression wrapper: {0}")]
    Compression(String),
    #[error("invalid message at offset {offset}: {reason}")]
    InvalidMessage { offset: usize, reason: String },
    #[error("moment grid error: {0}")]
    MomentGrid(#[from] radar_core::MomentGridError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveCompression {
    Gzip,
    Bzip2WholeFile,
    Bzip2Blocks,
    Uncompressed,
}

impl ArchiveCompression {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Bzip2WholeFile => "bzip2-whole-file",
            Self::Bzip2Blocks => "bzip2-blocks",
            Self::Uncompressed => "uncompressed",
        }
    }
}

/// Decode a local Archive II / Level II file into the shared radar model.
pub fn decode_volume_from_path(path: &Path) -> Result<RadarVolume> {
    let bytes = fs::read(path).map_err(|source| NexradError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut volume = decode_volume_from_bytes(&bytes)?;
    volume.metadata.source_path = Some(path.display().to_string());
    Ok(volume)
}

/// Decode a byte slice. This is public to support fixtures and embedded tests.
pub fn decode_volume_from_bytes(bytes: &[u8]) -> Result<RadarVolume> {
    if bytes.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader {
            actual: bytes.len(),
        });
    }
    if !bytes.starts_with(&[0x1f, 0x8b])
        && !bytes.starts_with(b"BZh")
        && let Some(blocks) = collect_bzip_block_slices(bytes)?
    {
        return decode_bzip_blocks_pipelined(bytes, blocks, None, false, |_| {})
            .map(|outcome| outcome.volume);
    }

    let (bytes, compression) = normalize_archive_bytes(bytes)?;
    decode_normalized_volume_bytes(&bytes, compression)
}

pub fn decode_gzip_volume_from_reader(reader: impl Read) -> Result<RadarVolume> {
    let mut decoder = GzDecoder::new(reader);
    decode_volume_from_stream_until(&mut decoder, ArchiveCompression::Gzip, None).map(|result| {
        debug_assert!(!result.stopped_at_preview);
        result.volume
    })
}

pub fn decode_gzip_volume_from_bytes_with_preview<F>(
    raw: &[u8],
    min_displayable_radials: usize,
    on_preview: F,
) -> Result<RadarVolume>
where
    F: FnMut(RadarVolume),
{
    if raw.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader { actual: raw.len() });
    }
    if !raw.starts_with(&[0x1f, 0x8b]) {
        return decode_volume_from_bytes(raw);
    }

    let mut decoder = GzDecoder::new(raw);
    decode_volume_from_stream(
        &mut decoder,
        ArchiveCompression::Gzip,
        Some(min_displayable_radials),
        false,
        on_preview,
    )
    .map(|result| {
        debug_assert!(!result.stopped_at_preview);
        result.volume
    })
}

pub fn decode_gzip_preview_from_bytes(
    raw: &[u8],
    min_displayable_radials: usize,
) -> Result<Option<RadarVolume>> {
    if raw.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader { actual: raw.len() });
    }
    if !raw.starts_with(&[0x1f, 0x8b]) {
        return Ok(None);
    }

    let mut decoder = GzDecoder::new(raw);
    let result = decode_volume_from_stream_until(
        &mut decoder,
        ArchiveCompression::Gzip,
        Some(min_displayable_radials),
    )?;
    Ok(result.stopped_at_preview.then_some(result.volume))
}

/// Decode a completed first displayable cut from NEXRAD block-bzip Level II bytes.
///
/// This is intended for UI preview on low-core machines: it returns `None` for
/// gzip, whole-file bzip, uncompressed, or malformed block-bzip inputs, and it
/// never substitutes for the final full-volume decode.
pub fn decode_bzip_block_preview_from_bytes(
    raw: &[u8],
    min_displayable_radials: usize,
) -> Result<Option<RadarVolume>> {
    if raw.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader { actual: raw.len() });
    }

    let Some(blocks) = collect_bzip_block_slices(raw)? else {
        return Ok(None);
    };

    let outcome =
        decode_bzip_blocks_pipelined(raw, blocks, Some(min_displayable_radials), true, |_| {})?;
    Ok(outcome.stopped_at_preview.then_some(outcome.volume))
}

/// Decode a full volume while optionally emitting an early completed first-cut preview.
///
/// For block-bzip Level II files, parsing streams behind the parallel block
/// decompression, so the preview is emitted as soon as the first displayable
/// cut completes — without decompressing or parsing anything twice. Other
/// compression formats fall back to a normal full decode.
pub fn decode_volume_from_bytes_with_bzip_preview<F>(
    raw: &[u8],
    min_displayable_radials: usize,
    mut on_preview: F,
) -> Result<RadarVolume>
where
    F: FnMut(RadarVolume),
{
    if raw.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader { actual: raw.len() });
    }

    let Some(blocks) = collect_bzip_block_slices(raw)? else {
        return decode_volume_from_bytes(raw);
    };

    let outcome = decode_bzip_blocks_pipelined(
        raw,
        blocks,
        Some(min_displayable_radials),
        false,
        |preview| {
            on_preview(preview.clone());
        },
    )?;
    Ok(outcome.volume)
}

/// Decompress or normalize an Archive II byte slice before Level II parsing.
pub fn normalize_archive_bytes(raw: &[u8]) -> Result<(Vec<u8>, ArchiveCompression)> {
    if raw.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader { actual: raw.len() });
    }

    if raw.starts_with(&[0x1f, 0x8b]) {
        let decoded = decompress_gzip_bytes(raw)?;
        return Ok((decoded, ArchiveCompression::Gzip));
    }

    if raw.starts_with(b"BZh") {
        let mut decoded = Vec::new();
        BzDecoder::new(Cursor::new(raw))
            .read_to_end(&mut decoded)
            .map_err(|err| NexradError::Compression(err.to_string()))?;
        return Ok((decoded, ArchiveCompression::Bzip2WholeFile));
    }

    if let Some(decoded) = try_decode_bzip_blocks(raw)? {
        return Ok((decoded, ArchiveCompression::Bzip2Blocks));
    }

    Ok((raw.to_vec(), ArchiveCompression::Uncompressed))
}

fn gzip_decoded_capacity_hint(raw: &[u8]) -> Option<usize> {
    let trailer = raw.get(raw.len().checked_sub(GZIP_TRAILER_LEN)?..)?;
    let isize = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]) as usize;
    let max_reasonable = raw.len().saturating_mul(MAX_GZIP_PREALLOC_RATIO);
    (isize <= max_reasonable).then_some(isize)
}

fn decompress_gzip_bytes(raw: &[u8]) -> Result<Vec<u8>> {
    if let Some(expected_len) = gzip_decoded_capacity_hint(raw)
        && let Some(decoded) = decompress_gzip_bytes_libdeflate(raw, expected_len)
    {
        return Ok(decoded);
    }

    let mut decoded = Vec::with_capacity(gzip_decoded_capacity_hint(raw).unwrap_or(0));
    GzDecoder::new(raw)
        .read_to_end(&mut decoded)
        .map_err(|err| NexradError::Compression(err.to_string()))?;
    Ok(decoded)
}

struct LibdeflateDecompressor {
    ptr: NonNull<libdeflate_sys::libdeflate_decompressor>,
}

thread_local! {
    static LIBDEFLATE_DECOMPRESSOR: Option<LibdeflateDecompressor> =
        LibdeflateDecompressor::new();
}

impl LibdeflateDecompressor {
    fn new() -> Option<Self> {
        NonNull::new(unsafe { libdeflate_sys::libdeflate_alloc_decompressor() })
            .map(|ptr| Self { ptr })
    }
}

impl Drop for LibdeflateDecompressor {
    fn drop(&mut self) {
        unsafe {
            libdeflate_sys::libdeflate_free_decompressor(self.ptr.as_ptr());
        }
    }
}

fn decompress_gzip_bytes_libdeflate(raw: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    let mut decoded = Vec::<MaybeUninit<u8>>::with_capacity(expected_len);
    let mut actual_len = 0usize;
    let result = LIBDEFLATE_DECOMPRESSOR.with(|decompressor| {
        let decompressor = decompressor.as_ref()?;
        Some(unsafe {
            libdeflate_sys::libdeflate_gzip_decompress(
                decompressor.ptr.as_ptr(),
                raw.as_ptr().cast(),
                raw.len(),
                decoded.as_mut_ptr().cast(),
                expected_len,
                &mut actual_len,
            )
        })
    })?;
    if result != libdeflate_sys::libdeflate_result_LIBDEFLATE_SUCCESS || actual_len > expected_len {
        return None;
    }

    let ptr = decoded.as_mut_ptr().cast::<u8>();
    let capacity = decoded.capacity();
    std::mem::forget(decoded);
    Some(unsafe { Vec::from_raw_parts(ptr, actual_len, capacity) })
}

fn read_record_prefix<R: Read>(reader: &mut R, buffer: &mut [u8], offset: usize) -> Result<bool> {
    let mut read = 0;
    while read < buffer.len() {
        let count = reader
            .read(&mut buffer[read..])
            .map_err(|err| NexradError::Compression(err.to_string()))?;
        if count == 0 {
            if read == 0 {
                return Ok(false);
            }
            return Err(NexradError::Truncated {
                what: "record prefix",
                offset,
                needed: buffer.len(),
                available: read,
            });
        }
        read += count;
    }
    Ok(true)
}

fn read_exact_required<R: Read>(
    reader: &mut R,
    buffer: &mut [u8],
    what: &'static str,
    offset: usize,
) -> Result<()> {
    let mut read = 0;
    while read < buffer.len() {
        let count = reader
            .read(&mut buffer[read..])
            .map_err(|err| NexradError::Compression(err.to_string()))?;
        if count == 0 {
            return Err(NexradError::Truncated {
                what,
                offset,
                needed: buffer.len(),
                available: read,
            });
        }
        read += count;
    }
    Ok(())
}

fn read_exact_into_buffer<R: Read>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    len: usize,
    what: &'static str,
    offset: usize,
) -> Result<()> {
    buffer.clear();
    if buffer.capacity() < len {
        buffer.reserve_exact(len);
    }
    let spare = buffer.spare_capacity_mut();
    let target = &mut spare[..len];
    // SAFETY: u8 has no invalid bit patterns, and the slice is within spare capacity.
    let target = unsafe { std::slice::from_raw_parts_mut(target.as_mut_ptr().cast::<u8>(), len) };
    read_exact_required(reader, target, what, offset)?;
    // SAFETY: read_exact_required returned Ok, so every byte in target was initialized.
    unsafe {
        buffer.set_len(len);
    }
    Ok(())
}

fn skip_record_padding<R: Read>(
    reader: &mut R,
    record_len: usize,
    consumed: usize,
    record_offset: usize,
) -> Result<()> {
    let padding = record_len.saturating_sub(consumed);
    skip_exact(reader, padding, "record padding", record_offset + consumed)
}

fn skip_exact<R: Read>(
    reader: &mut R,
    mut bytes: usize,
    what: &'static str,
    offset: usize,
) -> Result<()> {
    let mut buffer = [0; 8192];
    let mut skipped = 0;
    while bytes > 0 {
        let chunk = bytes.min(buffer.len());
        let target = &mut buffer[..chunk];
        read_exact_required(reader, target, what, offset + skipped)?;
        bytes -= chunk;
        skipped += chunk;
    }
    Ok(())
}

/// Parse already-normalized Archive II bytes.
pub fn decode_normalized_volume_bytes(
    bytes: &[u8],
    compression: ArchiveCompression,
) -> Result<RadarVolume> {
    let volume_header = parse_volume_header(bytes)?;
    let mut volume = RadarVolume::new(
        RadarSite::new(volume_header.icao.clone()),
        volume_header.volume_time,
    );
    volume.metadata.archive_version = Some(volume_header.archive_version);
    volume.metadata.compression = Some(compression.as_str().to_owned());

    let mut cursor = VOLUME_HEADER_LEN;
    let mut record_index = 0usize;
    // GR2-style ".msg31" exports keep the AR2V header but carry only a few
    // metadata records before variable-framed message 31s — well before the
    // standard 134 fixed records. Detected once at the first early message
    // 31 and latched for the rest of the file.
    let mut early_variable_msg31 = false;
    while cursor + CONTROL_WORD_LEN + MESSAGE_HEADER_LEN <= bytes.len() {
        let header_offset = cursor + CONTROL_WORD_LEN;
        let header =
            parse_message_header_bytes(&bytes[header_offset..header_offset + MESSAGE_HEADER_LEN]);

        if header.size_halfwords == 0 && record_index < 134 {
            volume.metadata.skipped_message_count += 1;
            cursor = cursor.saturating_add(RECORD_BYTES);
            record_index += 1;
            continue;
        } else if header.size_halfwords == 0 {
            break;
        }

        let message_total_len = usize::from(header.size_halfwords) * 2;
        if message_total_len < MESSAGE_HEADER_LEN {
            return Err(NexradError::InvalidMessage {
                offset: header_offset,
                reason: "message size is smaller than message header".to_owned(),
            });
        }

        volume.metadata.message_count += 1;
        match header.message_type {
            31 => {
                let message_end = header_offset + message_total_len;
                if message_end > bytes.len() {
                    if volume.metadata.decoded_radial_count > 0 {
                        volume.metadata.skipped_message_count += 1;
                        break;
                    }
                    return Err(NexradError::Truncated {
                        what: "message 31 body",
                        offset: header_offset,
                        needed: message_total_len,
                        available: bytes.len().saturating_sub(header_offset),
                    });
                }
                let body = &bytes[header_offset + MESSAGE_HEADER_LEN..message_end];
                parse_message_31(body, &header, &mut volume)?;
            }
            5 => {
                let body_offset = header_offset + MESSAGE_HEADER_LEN;
                let fixed_record_end = cursor.saturating_add(RECORD_BYTES).min(bytes.len());
                let message_end = header_offset.saturating_add(message_total_len);
                let body_end = message_end.min(fixed_record_end);
                if body_offset < body_end {
                    parse_message_5(&bytes[body_offset..body_end], &mut volume);
                }
            }
            _ => volume.metadata.skipped_message_count += 1,
        }

        let record_len = if header.message_type != 31 {
            RECORD_BYTES
        } else if record_index >= 134 || early_variable_msg31 {
            message_total_len + CONTROL_WORD_LEN
        } else if message31_uses_variable_framing(bytes, cursor, message_total_len) {
            early_variable_msg31 = true;
            message_total_len + CONTROL_WORD_LEN
        } else {
            RECORD_BYTES
        };
        cursor = cursor.saturating_add(record_len);
        record_index += 1;
    }

    Ok(volume)
}

/// Decide the framing of a message 31 seen before the standard 134 metadata
/// records. Real Archive II volumes never place message 31 that early, but
/// GR2-style ".msg31" exports (DOW/COW/RaXPol Level II twins) do, packing
/// them back to back with no fixed-record padding. Returns `true` when the
/// bytes directly after this message hold another message 31 (or the file
/// ends exactly there), which fixed 2432-byte framing cannot produce.
fn message31_uses_variable_framing(bytes: &[u8], cursor: usize, message_total_len: usize) -> bool {
    let variable_next = cursor + CONTROL_WORD_LEN + message_total_len;
    if variable_next == bytes.len() {
        return true;
    }
    let header_offset = variable_next + CONTROL_WORD_LEN;
    let Some(header_bytes) = bytes.get(header_offset..header_offset + MESSAGE_HEADER_LEN) else {
        return false;
    };
    let header = parse_message_header_bytes(header_bytes);
    header.message_type == 31
        && usize::from(header.size_halfwords) * 2 >= MESSAGE_HEADER_LEN + MSG_31_HEADER_LEN
}

struct StreamDecodeResult {
    volume: RadarVolume,
    stopped_at_preview: bool,
}

fn decode_volume_from_stream_until<R: Read>(
    reader: &mut R,
    compression: ArchiveCompression,
    preview_min_radials: Option<usize>,
) -> Result<StreamDecodeResult> {
    decode_volume_from_stream(reader, compression, preview_min_radials, true, |_| {})
}

fn decode_volume_from_stream<R: Read, F>(
    reader: &mut R,
    compression: ArchiveCompression,
    preview_min_radials: Option<usize>,
    stop_at_preview: bool,
    mut on_preview: F,
) -> Result<StreamDecodeResult>
where
    F: FnMut(RadarVolume),
{
    let mut volume_header_bytes = [0; VOLUME_HEADER_LEN];
    read_exact_required(reader, &mut volume_header_bytes, "volume header", 0)?;
    let volume_header = parse_volume_header(&volume_header_bytes)?;
    let mut volume = RadarVolume::new(
        RadarSite::new(volume_header.icao.clone()),
        volume_header.volume_time,
    );
    volume.metadata.archive_version = Some(volume_header.archive_version);
    volume.metadata.compression = Some(compression.as_str().to_owned());

    let mut cursor = VOLUME_HEADER_LEN;
    let mut record_index = 0usize;
    let mut prefix = [0; CONTROL_WORD_LEN + MESSAGE_HEADER_LEN];
    let mut body_buffer = Vec::with_capacity(RECORD_BYTES);
    let mut preview_emitted = false;
    while read_record_prefix(reader, &mut prefix, cursor)? {
        let header_offset = cursor + CONTROL_WORD_LEN;
        let header = parse_message_header_bytes(&prefix[CONTROL_WORD_LEN..]);

        if header.size_halfwords == 0 && record_index < 134 {
            volume.metadata.skipped_message_count += 1;
            skip_exact(
                reader,
                RECORD_BYTES - prefix.len(),
                "empty fixed record",
                cursor + prefix.len(),
            )?;
            cursor = cursor.saturating_add(RECORD_BYTES);
            record_index += 1;
            continue;
        } else if header.size_halfwords == 0 {
            break;
        }

        let message_total_len = usize::from(header.size_halfwords) * 2;
        if message_total_len < MESSAGE_HEADER_LEN {
            return Err(NexradError::InvalidMessage {
                offset: header_offset,
                reason: "message size is smaller than message header".to_owned(),
            });
        }

        let record_len = if record_index < 134 || header.message_type != 31 {
            RECORD_BYTES
        } else {
            message_total_len + CONTROL_WORD_LEN
        };
        let body_len = message_total_len - MESSAGE_HEADER_LEN;
        volume.metadata.message_count += 1;

        match header.message_type {
            31 => {
                if let Err(err) = read_exact_into_buffer(
                    reader,
                    &mut body_buffer,
                    body_len,
                    "message 31 body",
                    header_offset,
                ) {
                    if volume.metadata.decoded_radial_count > 0 {
                        volume.metadata.skipped_message_count += 1;
                        break;
                    }
                    return Err(err);
                }
                parse_message_31(&body_buffer, &header, &mut volume)?;
                skip_record_padding(reader, record_len, prefix.len() + body_len, cursor)?;
                if let Some(min_radials) = preview_min_radials
                    && !preview_emitted
                    && has_complete_displayable_cut(&volume, min_radials)
                {
                    preview_emitted = true;
                    if stop_at_preview {
                        return Ok(StreamDecodeResult {
                            volume,
                            stopped_at_preview: true,
                        });
                    }
                    on_preview(volume.clone());
                }
            }
            5 => {
                let fixed_body_len = RECORD_BYTES.saturating_sub(prefix.len());
                let body_read_len = body_len.min(fixed_body_len);
                read_exact_into_buffer(
                    reader,
                    &mut body_buffer,
                    body_read_len,
                    "message 5 body",
                    header_offset,
                )?;
                parse_message_5(&body_buffer, &mut volume);
                skip_record_padding(reader, record_len, prefix.len() + body_read_len, cursor)?;
            }
            _ => {
                volume.metadata.skipped_message_count += 1;
                skip_record_padding(reader, record_len, prefix.len(), cursor)?;
            }
        }

        cursor = cursor.saturating_add(record_len);
        record_index += 1;
    }

    Ok(StreamDecodeResult {
        volume,
        stopped_at_preview: false,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VolumeHeader {
    archive_version: String,
    volume_time: DateTime<Utc>,
    icao: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageHeader {
    pub size_halfwords: u16,
    pub channels: u8,
    pub message_type: u8,
    pub sequence_id: u16,
    pub date: u16,
    pub milliseconds: u32,
    pub segments: u16,
    pub segment_number: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message31Header {
    pub collect_ms: u32,
    pub collect_date: u16,
    pub azimuth_number: u16,
    pub azimuth_angle: f32,
    pub radial_length: u16,
    pub azimuth_resolution: u8,
    pub radial_status: RadialStatus,
    pub elevation_number: u8,
    pub cut_sector: u8,
    pub elevation_angle: f32,
    pub block_pointers: [usize; 10],
}

#[derive(Clone, Debug, PartialEq)]
struct MomentBlock<'a> {
    moment: MomentType,
    gate_range: GateRange,
    scale: f32,
    offset: f32,
    row: MomentPayload<'a>,
}

#[derive(Clone, Debug, PartialEq)]
enum MomentPayload<'a> {
    U8(&'a [u8]),
    U16(&'a [u8]),
}

const BLOCK_PENDING: u8 = 0;
const BLOCK_READY: u8 = 1;
const BLOCK_FAILED: u8 = 2;

/// Slot store connecting parallel LDM-block decompression workers to the
/// in-order streaming parser.
///
/// Indices are claimed in parse order through `next_claim`, so each slot has
/// exactly one writer. A slot is published with a `Release` store on its state
/// flag and readers dereference it only after an `Acquire` load observes
/// `BLOCK_READY`, after which the slot is never written again.
struct BlockSlots<'a> {
    compressed: Vec<&'a [u8]>,
    slots: Box<[UnsafeCell<Vec<u8>>]>,
    states: Box<[AtomicU8]>,
    errors: Mutex<Vec<Option<String>>>,
    next_claim: AtomicUsize,
    canceled: AtomicBool,
    wakeup: Mutex<()>,
    published: Condvar,
}

// SAFETY: each `UnsafeCell` slot is written by exactly one thread (the unique
// claimant of its index) before being published via the matching `AtomicU8`
// with Release/Acquire ordering; every other field is already `Sync`.
unsafe impl Sync for BlockSlots<'_> {}

impl<'a> BlockSlots<'a> {
    fn new(compressed: Vec<&'a [u8]>) -> Self {
        let len = compressed.len();
        Self {
            compressed,
            slots: (0..len).map(|_| UnsafeCell::new(Vec::new())).collect(),
            states: (0..len).map(|_| AtomicU8::new(BLOCK_PENDING)).collect(),
            errors: Mutex::new(vec![None; len]),
            next_claim: AtomicUsize::new(0),
            canceled: AtomicBool::new(false),
            wakeup: Mutex::new(()),
            published: Condvar::new(),
        }
    }

    fn len(&self) -> usize {
        self.compressed.len()
    }

    fn cancel(&self) {
        self.canceled.store(true, Ordering::Relaxed);
    }

    fn run_worker(&self) {
        while !self.canceled.load(Ordering::Relaxed) {
            let index = self.next_claim.fetch_add(1, Ordering::Relaxed);
            if index >= self.len() {
                break;
            }
            self.decompress_index(index);
        }
    }

    fn decompress_index(&self, index: usize) {
        let state = match decompress_bzip_block(self.compressed[index]) {
            Ok(decoded) => {
                // SAFETY: `index` was claimed exactly once via `next_claim`,
                // so this thread is the slot's unique writer; readers wait for
                // the Release store below before touching it.
                unsafe {
                    *self.slots[index].get() = decoded;
                }
                BLOCK_READY
            }
            Err(err) => {
                self.errors.lock().unwrap()[index] = Some(err.to_string());
                BLOCK_FAILED
            }
        };
        self.states[index].store(state, Ordering::Release);
        // Take the wakeup lock so a parser that has checked the state but not
        // yet parked cannot miss this notification.
        drop(self.wakeup.lock().unwrap());
        self.published.notify_all();
    }

    /// Block until the decompressed contents of `index` are available.
    ///
    /// The caller participates in decompression while it waits (claims advance
    /// in parse order), so the pipeline makes progress even when no rayon
    /// worker ever runs — e.g. on a single-threaded pool.
    fn wait_block(&self, index: usize) -> Result<&[u8]> {
        loop {
            match self.states[index].load(Ordering::Acquire) {
                BLOCK_READY => {
                    // SAFETY: published with Release by the unique writer and
                    // never written again; the slot box itself is pre-sized
                    // and never reallocated.
                    return Ok(unsafe { (*self.slots[index].get()).as_slice() });
                }
                BLOCK_FAILED => {
                    let message = self.errors.lock().unwrap()[index]
                        .clone()
                        .unwrap_or_else(|| "bzip2 block decompression failed".to_owned());
                    return Err(NexradError::Compression(message));
                }
                _ => {}
            }
            let claimed = self.next_claim.fetch_add(1, Ordering::Relaxed);
            if claimed < self.len() {
                self.decompress_index(claimed);
                continue;
            }
            // Everything is claimed, so `index` is in flight on another
            // thread; park until the next publish.
            let mut guard = self.wakeup.lock().unwrap();
            while self.states[index].load(Ordering::Acquire) == BLOCK_PENDING {
                guard = self.published.wait(guard).unwrap();
            }
        }
    }
}

struct BzipBlockCursor<'a> {
    volume_header: &'a [u8],
    blocks: &'a BlockSlots<'a>,
    chunk_index: usize,
    chunk_offset: usize,
    absolute_offset: usize,
    current: Option<&'a [u8]>,
}

impl<'a> BzipBlockCursor<'a> {
    fn new(volume_header: &'a [u8], blocks: &'a BlockSlots<'a>) -> Self {
        Self {
            volume_header,
            blocks,
            chunk_index: 0,
            chunk_offset: 0,
            absolute_offset: 0,
            current: None,
        }
    }

    fn current_chunk(&mut self) -> Result<Option<&'a [u8]>> {
        if let Some(chunk) = self.current {
            return Ok(Some(chunk));
        }
        let chunk = match self.chunk_index {
            0 => Some(self.volume_header),
            index if index - 1 < self.blocks.len() => Some(self.blocks.wait_block(index - 1)?),
            _ => None,
        };
        self.current = chunk;
        Ok(chunk)
    }

    fn advance_chunk(&mut self) {
        self.chunk_index += 1;
        self.chunk_offset = 0;
        self.current = None;
    }

    fn skip_empty_chunks(&mut self) -> Result<()> {
        while let Some(chunk) = self.current_chunk()? {
            if self.chunk_offset < chunk.len() {
                break;
            }
            self.advance_chunk();
        }
        Ok(())
    }

    fn read_exact_into(
        &mut self,
        mut output: &mut [u8],
        what: &'static str,
        offset: usize,
    ) -> Result<()> {
        let mut written = 0;
        while !output.is_empty() {
            self.skip_empty_chunks()?;
            let Some(chunk) = self.current_chunk()? else {
                return Err(NexradError::Truncated {
                    what,
                    offset,
                    needed: written + output.len(),
                    available: written,
                });
            };
            let available = &chunk[self.chunk_offset..];
            let count = available.len().min(output.len());
            output[..count].copy_from_slice(&available[..count]);
            self.chunk_offset += count;
            self.absolute_offset += count;
            let (_, rest) = output.split_at_mut(count);
            output = rest;
            written += count;
        }
        Ok(())
    }

    fn read_optional_prefix(&mut self, output: &mut [u8], offset: usize) -> Result<bool> {
        self.skip_empty_chunks()?;
        if self.current_chunk()?.is_none() {
            return Ok(false);
        }
        self.read_exact_into(output, "record prefix", offset)?;
        Ok(true)
    }

    fn read_slice_or_copy<'b>(
        &'b mut self,
        scratch: &'b mut Vec<u8>,
        len: usize,
        what: &'static str,
        offset: usize,
    ) -> Result<&'b [u8]> {
        scratch.clear();
        if len == 0 {
            return Ok(&[]);
        }
        self.skip_empty_chunks()?;
        let Some(chunk) = self.current_chunk()? else {
            return Err(NexradError::Truncated {
                what,
                offset,
                needed: len,
                available: 0,
            });
        };
        if self.chunk_offset + len <= chunk.len() {
            let start = self.chunk_offset;
            self.chunk_offset += len;
            self.absolute_offset += len;
            return Ok(&chunk[start..start + len]);
        }

        if scratch.capacity() < len {
            scratch.reserve_exact(len - scratch.capacity());
        }
        let mut remaining = len;
        while remaining > 0 {
            self.skip_empty_chunks()?;
            let Some(chunk) = self.current_chunk()? else {
                return Err(NexradError::Truncated {
                    what,
                    offset,
                    needed: len,
                    available: scratch.len(),
                });
            };
            let available = &chunk[self.chunk_offset..];
            let count = available.len().min(remaining);
            scratch.extend_from_slice(&available[..count]);
            self.chunk_offset += count;
            self.absolute_offset += count;
            remaining -= count;
        }
        Ok(scratch.as_slice())
    }

    fn skip_exact(&mut self, len: usize, what: &'static str, offset: usize) -> Result<()> {
        let mut skipped = 0;
        while skipped < len {
            self.skip_empty_chunks()?;
            let Some(chunk) = self.current_chunk()? else {
                return Err(NexradError::Truncated {
                    what,
                    offset,
                    needed: len,
                    available: skipped,
                });
            };
            let count = (len - skipped).min(chunk.len() - self.chunk_offset);
            self.chunk_offset += count;
            self.absolute_offset += count;
            skipped += count;
        }
        Ok(())
    }
}

struct BlockParseOutcome {
    volume: RadarVolume,
    stopped_at_preview: bool,
}

/// Decode a block-bzip volume by parsing in lockstep with the parallel block
/// decompression: rayon workers fill `BlockSlots` while this thread parses
/// blocks in order, waiting (or stealing decompression work) only when the
/// next block is not ready yet. Total wall time is the decompression wall time
/// instead of decompression followed by a serial parse.
fn decode_bzip_blocks_pipelined(
    raw: &[u8],
    blocks: Vec<&[u8]>,
    min_displayable_radials: Option<usize>,
    stop_at_preview: bool,
    on_preview: impl FnMut(&RadarVolume),
) -> Result<BlockParseOutcome> {
    let slots = BlockSlots::new(blocks);
    rayon::in_place_scope(|scope| {
        // Leave one hardware thread for the parsing thread below; it also
        // steals decompression work whenever it would otherwise wait.
        let workers = rayon::current_num_threads()
            .saturating_sub(1)
            .max(1)
            .min(slots.len());
        for _ in 0..workers {
            scope.spawn(|_| slots.run_worker());
        }
        let outcome = parse_bzip_block_volume(
            &raw[..VOLUME_HEADER_LEN],
            &slots,
            min_displayable_radials,
            stop_at_preview,
            on_preview,
        );
        // Stop idle claims if the parse returned early (preview-only or error).
        slots.cancel();
        outcome
    })
}

fn parse_bzip_block_volume(
    volume_header: &[u8],
    blocks: &BlockSlots<'_>,
    min_displayable_radials: Option<usize>,
    stop_at_preview: bool,
    mut on_preview: impl FnMut(&RadarVolume),
) -> Result<BlockParseOutcome> {
    let mut preview_pending = min_displayable_radials;
    let mut cursor_reader = BzipBlockCursor::new(volume_header, blocks);
    let mut volume_header_buffer = Vec::new();
    let volume_header_bytes = cursor_reader.read_slice_or_copy(
        &mut volume_header_buffer,
        VOLUME_HEADER_LEN,
        "volume header",
        0,
    )?;
    let volume_header = parse_volume_header(volume_header_bytes)?;
    let mut volume = RadarVolume::new(
        RadarSite::new(volume_header.icao.clone()),
        volume_header.volume_time,
    );
    volume.metadata.archive_version = Some(volume_header.archive_version);
    volume.metadata.compression = Some(ArchiveCompression::Bzip2Blocks.as_str().to_owned());

    let mut cursor = VOLUME_HEADER_LEN;
    let mut record_index = 0usize;
    let mut prefix = [0; CONTROL_WORD_LEN + MESSAGE_HEADER_LEN];
    let mut body_buffer = Vec::with_capacity(RECORD_BYTES);
    loop {
        match cursor_reader.read_optional_prefix(&mut prefix, cursor) {
            Ok(true) => {}
            Ok(false) => break,
            // A damaged block after at least one decoded radial degrades to a
            // partial volume, mirroring the message-31 body handling below.
            Err(err) => {
                if volume.metadata.decoded_radial_count > 0 {
                    volume.metadata.skipped_message_count += 1;
                    break;
                }
                return Err(err);
            }
        }
        let header_offset = cursor + CONTROL_WORD_LEN;
        let header = parse_message_header_bytes(&prefix[CONTROL_WORD_LEN..]);

        if header.size_halfwords == 0 && record_index < 134 {
            volume.metadata.skipped_message_count += 1;
            cursor_reader.skip_exact(
                RECORD_BYTES - prefix.len(),
                "empty fixed record",
                cursor + prefix.len(),
            )?;
            cursor = cursor.saturating_add(RECORD_BYTES);
            record_index += 1;
            continue;
        } else if header.size_halfwords == 0 {
            break;
        }

        let message_total_len = usize::from(header.size_halfwords) * 2;
        if message_total_len < MESSAGE_HEADER_LEN {
            return Err(NexradError::InvalidMessage {
                offset: header_offset,
                reason: "message size is smaller than message header".to_owned(),
            });
        }

        let record_len = if record_index < 134 || header.message_type != 31 {
            RECORD_BYTES
        } else {
            message_total_len + CONTROL_WORD_LEN
        };
        let body_len = message_total_len - MESSAGE_HEADER_LEN;
        volume.metadata.message_count += 1;

        match header.message_type {
            31 => {
                let body = match cursor_reader.read_slice_or_copy(
                    &mut body_buffer,
                    body_len,
                    "message 31 body",
                    header_offset,
                ) {
                    Ok(body) => body,
                    Err(err) => {
                        if volume.metadata.decoded_radial_count > 0 {
                            volume.metadata.skipped_message_count += 1;
                            break;
                        }
                        return Err(err);
                    }
                };
                parse_message_31(body, &header, &mut volume)?;
                if let Some(min_radials) = preview_pending
                    && has_complete_displayable_cut(&volume, min_radials)
                {
                    preview_pending = None;
                    if stop_at_preview {
                        return Ok(BlockParseOutcome {
                            volume,
                            stopped_at_preview: true,
                        });
                    }
                    on_preview(&volume);
                }
                cursor_reader.skip_exact(
                    record_len.saturating_sub(prefix.len() + body_len),
                    "record padding",
                    cursor + prefix.len() + body_len,
                )?;
            }
            5 => {
                let fixed_body_len = RECORD_BYTES.saturating_sub(prefix.len());
                let body_read_len = body_len.min(fixed_body_len);
                let body = cursor_reader.read_slice_or_copy(
                    &mut body_buffer,
                    body_read_len,
                    "message 5 body",
                    header_offset,
                )?;
                parse_message_5(body, &mut volume);
                cursor_reader.skip_exact(
                    record_len.saturating_sub(prefix.len() + body_read_len),
                    "record padding",
                    cursor + prefix.len() + body_read_len,
                )?;
            }
            _ => {
                volume.metadata.skipped_message_count += 1;
                cursor_reader.skip_exact(
                    record_len.saturating_sub(prefix.len()),
                    "record padding",
                    cursor + prefix.len(),
                )?;
            }
        }

        cursor = cursor.saturating_add(record_len);
        record_index += 1;
    }

    Ok(BlockParseOutcome {
        volume,
        stopped_at_preview: false,
    })
}

fn has_complete_displayable_cut(volume: &RadarVolume, min_displayable_radials: usize) -> bool {
    volume.cuts.iter().enumerate().any(|(index, cut)| {
        if cut.radials.len() < min_displayable_radials {
            return false;
        }
        let has_displayable_moment = cut
            .moments
            .values()
            .any(|grid| grid.radial_count() >= min_displayable_radials);
        if !has_displayable_moment {
            return false;
        }
        let ended = cut.radials.last().is_some_and(|radial| {
            matches!(
                radial.radial_status,
                Some(RadialStatus::EndElevation | RadialStatus::EndVolume)
            )
        });
        ended || index + 1 < volume.cuts.len()
    })
}

fn try_decode_bzip_blocks(raw: &[u8]) -> Result<Option<Vec<u8>>> {
    let Some(decoded_blocks) = try_decompress_bzip_blocks(raw)? else {
        return Ok(None);
    };

    let decoded_len = decoded_blocks.iter().map(Vec::len).sum::<usize>();
    let mut output = Vec::with_capacity(VOLUME_HEADER_LEN + decoded_len);
    output.extend_from_slice(&raw[..VOLUME_HEADER_LEN]);
    for block in decoded_blocks {
        output.extend(block);
    }

    Ok(Some(output))
}

fn try_decompress_bzip_blocks(raw: &[u8]) -> Result<Option<Vec<Vec<u8>>>> {
    let Some(blocks) = collect_bzip_block_slices(raw)? else {
        return Ok(None);
    };

    let decoded_blocks = blocks
        .par_iter()
        .map(|compressed| decompress_bzip_block(compressed))
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(decoded_blocks))
}

fn collect_bzip_block_slices(raw: &[u8]) -> Result<Option<Vec<&[u8]>>> {
    if raw.len() < VOLUME_HEADER_LEN + 4 {
        return Ok(None);
    }

    let mut cursor = VOLUME_HEADER_LEN;
    let mut blocks = Vec::new();

    while cursor + 4 <= raw.len() {
        let signed_block_size = i32_at(raw, cursor)?;
        if signed_block_size == -1 && cursor + 4 == raw.len() {
            break;
        }
        if signed_block_size == 0 {
            return Ok(None);
        }

        cursor += 4;
        let is_last_block = signed_block_size < 0;
        let block_size = usize::try_from(signed_block_size.unsigned_abs())
            .map_err(|_| NexradError::Compression("bzip2 block size overflow".to_owned()))?;
        if cursor + block_size > raw.len() {
            return Ok(None);
        }

        let compressed = &raw[cursor..cursor + block_size];
        if !compressed.starts_with(b"BZh") {
            return Ok(None);
        }

        blocks.push(compressed);
        cursor += block_size;
        if is_last_block {
            break;
        }
    }

    if blocks.is_empty() {
        return Ok(None);
    }

    Ok(Some(blocks))
}

fn decompress_bzip_block(compressed: &[u8]) -> Result<Vec<u8>> {
    let mut decoded =
        Vec::with_capacity(BZIP_BLOCK_DECODE_CAPACITY_HINT.max(compressed.len().saturating_mul(2)));
    BzDecoder::new(Cursor::new(compressed))
        .read_to_end(&mut decoded)
        .map_err(|err| NexradError::Compression(err.to_string()))?;
    Ok(decoded)
}

fn parse_volume_header(bytes: &[u8]) -> Result<VolumeHeader> {
    require_len(bytes, 0, VOLUME_HEADER_LEN, "volume header")?;
    let tape = ascii_trim(&bytes[0..9]);
    let extension = ascii_trim(&bytes[9..12]);
    let date = u32_at(bytes, 12)?;
    let milliseconds = u32_at(bytes, 16)?;
    let icao = ascii_trim(&bytes[20..24]);

    Ok(VolumeHeader {
        archive_version: format!("{tape}{extension}"),
        volume_time: nexrad_date_ms_to_datetime(date, milliseconds),
        icao,
    })
}

pub fn parse_message_header(bytes: &[u8], offset: usize) -> Result<MessageHeader> {
    require_len(bytes, offset, MESSAGE_HEADER_LEN, "message header")?;
    Ok(parse_message_header_bytes(
        &bytes[offset..offset + MESSAGE_HEADER_LEN],
    ))
}

fn parse_message_header_bytes(bytes: &[u8]) -> MessageHeader {
    debug_assert!(bytes.len() >= MESSAGE_HEADER_LEN);
    MessageHeader {
        size_halfwords: be_u16(bytes, 0),
        channels: bytes[2],
        message_type: bytes[3],
        sequence_id: be_u16(bytes, 4),
        date: be_u16(bytes, 6),
        milliseconds: be_u32(bytes, 8),
        segments: be_u16(bytes, 12),
        segment_number: be_u16(bytes, 14),
    }
}

fn parse_message_5(body: &[u8], volume: &mut RadarVolume) {
    if body.len() >= 6 {
        let pattern = u16::from_be_bytes([body[4], body[5]]);
        if pattern != 0 {
            volume.vcp = Some(VcpInfo { pattern });
        }
    }
}

fn parse_message_31(
    body: &[u8],
    _message_header: &MessageHeader,
    volume: &mut RadarVolume,
) -> Result<()> {
    let header = parse_message_31_header(body, 0)?;
    let expected_radials = expected_radials_for_azimuth_resolution(header.azimuth_resolution);

    // GR2-style ".msg31" exports write a nonstandard volume-header date, so
    // the volume time parses as the epoch; recover it from the first
    // radial's collection time instead.
    if volume.volume_time == DateTime::<Utc>::UNIX_EPOCH && header.collect_date > 0 {
        volume.volume_time =
            nexrad_date_ms_to_datetime(u32::from(header.collect_date), header.collect_ms);
    }

    let mut nyquist_velocity_mps = None;
    let mut moments: [Option<MomentBlock<'_>>; MAX_MESSAGE_31_MOMENTS] =
        std::array::from_fn(|_| None);
    let mut moment_count = 0;
    let needs_volume_constants = volume_needs_constant_block(volume);

    for pointer in &header.block_pointers {
        if *pointer == 0 {
            continue;
        }
        let pointer = *pointer;
        if pointer > body.len().saturating_sub(4) {
            continue;
        }

        match body[pointer] {
            b'R' if &body[pointer + 1..pointer + 4] == b"VOL" => {
                if needs_volume_constants {
                    parse_volume_constant_block(body, pointer, volume)?;
                }
            }
            b'R' if &body[pointer + 1..pointer + 4] == b"RAD" => {
                nyquist_velocity_mps = parse_radial_constant_block(body, pointer)?;
            }
            b'D' if moment_count < moments.len() => {
                moments[moment_count] = Some(parse_generic_moment_block(body, pointer)?);
                moment_count += 1;
            }
            _ => {}
        }
    }

    let gate_range = moments[..moment_count]
        .iter()
        .flatten()
        .next()
        .map(|moment| moment.gate_range.clone())
        .unwrap_or(GateRange {
            first_gate_m: 0,
            gate_spacing_m: 0,
            gate_count: 0,
        });
    let radial = Radial {
        azimuth_deg: header.azimuth_angle,
        elevation_deg: header.elevation_angle,
        time_offset_ms: header.collect_ms as i32,
        gate_range,
        nyquist_velocity_mps,
        radial_status: Some(header.radial_status),
    };

    let starts_elevation = matches!(
        header.radial_status,
        RadialStatus::StartElevation
            | RadialStatus::StartVolume
            | RadialStatus::StartElevationLastCut
    );
    let last_cut_has_radials = volume
        .cuts
        .last()
        .is_some_and(|cut| !cut.radials.is_empty());
    let last_cut_matches = volume.cuts.last().is_some_and(|cut| {
        cut.elevation_number == Some(header.elevation_number)
            || (cut.elevation_deg - header.elevation_angle).abs() <= 0.05
    });
    let cut = if starts_elevation && last_cut_has_radials {
        volume.push_cut(header.elevation_angle, Some(header.elevation_number))
    } else if last_cut_matches {
        volume
            .cuts
            .last_mut()
            .expect("last cut existence was checked before borrowing")
    } else {
        volume.find_or_insert_cut(header.elevation_angle, Some(header.elevation_number))
    };
    if cut.radials.is_empty() {
        cut.radials.reserve(expected_radials);
    }
    let radial_index = cut.radials.len();
    cut.radials.push(radial);

    for moment in moments.into_iter().take(moment_count).flatten() {
        let MomentBlock {
            moment,
            gate_range,
            scale,
            offset,
            row,
        } = moment;
        let grid = match cut.moments.entry(moment) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => match &row {
                MomentPayload::U8(_) => {
                    let mut grid = MomentGrid::new_u8(
                        entry.key().clone(),
                        gate_range.clone(),
                        scale,
                        offset,
                        Some(0),
                        Some(1),
                    );
                    grid.reserve_rows(expected_radials);
                    entry.insert(grid)
                }
                MomentPayload::U16(_) => {
                    let mut grid = MomentGrid::new_u16(
                        entry.key().clone(),
                        gate_range.clone(),
                        scale,
                        offset,
                        Some(0),
                        Some(1),
                    );
                    grid.reserve_rows(expected_radials);
                    entry.insert(grid)
                }
            },
        };
        match row {
            MomentPayload::U8(row) => grid.push_u8_row_slice(radial_index, row)?,
            MomentPayload::U16(row) => grid.push_u16_be_row_bytes(radial_index, row)?,
        }
    }

    volume.metadata.decoded_radial_count += 1;
    Ok(())
}

fn expected_radials_for_azimuth_resolution(azimuth_resolution: u8) -> usize {
    match azimuth_resolution {
        1 => HALF_DEGREE_RADIALS_PER_CUT,
        2 => ONE_DEGREE_RADIALS_PER_CUT,
        _ => FALLBACK_RADIALS_PER_CUT,
    }
}

fn volume_needs_constant_block(volume: &RadarVolume) -> bool {
    volume.site.latitude_deg.is_none()
        || volume.site.longitude_deg.is_none()
        || volume.site.elevation_m.is_none()
        || volume.vcp.is_none()
}

pub fn parse_message_31_header(bytes: &[u8], offset: usize) -> Result<Message31Header> {
    require_len(bytes, offset, MSG_31_HEADER_LEN, "message 31 header")?;
    let bytes = &bytes[offset..offset + MSG_31_HEADER_LEN];
    if bytes[..4]
        .iter()
        .all(|byte| *byte == 0 || byte.is_ascii_whitespace())
    {
        return Err(NexradError::InvalidMessage {
            offset,
            reason: "empty message 31 id".to_owned(),
        });
    }

    let mut block_pointers = [0; 10];
    for (index, pointer) in block_pointers.iter_mut().enumerate() {
        *pointer = be_u32(bytes, 32 + index * 4) as usize;
    }

    Ok(Message31Header {
        collect_ms: be_u32(bytes, 4),
        collect_date: be_u16(bytes, 8),
        azimuth_number: be_u16(bytes, 10),
        azimuth_angle: be_f32(bytes, 12),
        radial_length: be_u16(bytes, 18),
        azimuth_resolution: bytes[20],
        radial_status: RadialStatus::from(bytes[21]),
        elevation_number: bytes[22],
        cut_sector: bytes[23],
        elevation_angle: be_f32(bytes, 24),
        block_pointers,
    })
}

fn parse_volume_constant_block(
    bytes: &[u8],
    offset: usize,
    volume: &mut RadarVolume,
) -> Result<()> {
    require_len(
        bytes,
        offset,
        VOLUME_CONSTANT_BLOCK_LEN,
        "volume constant block",
    )?;
    let bytes = &bytes[offset..offset + VOLUME_CONSTANT_BLOCK_LEN];
    volume.site.latitude_deg = Some(be_f32(bytes, 8));
    volume.site.longitude_deg = Some(be_f32(bytes, 12));

    let tower_height_m = be_i16(bytes, 16) as f32;
    let feedhorn_height_m = be_u16(bytes, 18) as f32;
    volume.site.elevation_m = Some(tower_height_m + feedhorn_height_m);

    let vcp = be_u16(bytes, 40);
    if vcp != 0 {
        volume.vcp = Some(VcpInfo { pattern: vcp });
    }
    Ok(())
}

fn parse_radial_constant_block(bytes: &[u8], offset: usize) -> Result<Option<f32>> {
    require_len(
        bytes,
        offset,
        RADIAL_CONSTANT_BLOCK_LEN,
        "radial constant block",
    )?;
    let raw = be_i16(&bytes[offset..offset + RADIAL_CONSTANT_BLOCK_LEN], 16);
    Ok((raw > 0).then_some(raw as f32 / 100.0))
}

fn parse_generic_moment_block(bytes: &[u8], offset: usize) -> Result<MomentBlock<'_>> {
    require_len(
        bytes,
        offset,
        GENERIC_DATA_BLOCK_LEN,
        "generic moment block",
    )?;
    let header = &bytes[offset..offset + GENERIC_DATA_BLOCK_LEN];
    let moment = MomentType::from_nexrad_bytes(&header[1..4]);
    let gate_count = usize::from(be_u16(header, 8));
    let first_gate_m = i32::from(be_i16(header, 10));
    let gate_spacing_m = i32::from(be_i16(header, 12));
    let word_size = header[19];
    let scale = be_f32(header, 20);
    let offset_value = be_f32(header, 24);
    let data_offset = offset + GENERIC_DATA_BLOCK_LEN;

    let row = match word_size {
        8 => {
            require_len(bytes, data_offset, gate_count, "8-bit moment gates")?;
            MomentPayload::U8(&bytes[data_offset..data_offset + gate_count])
        }
        16 => {
            let byte_count = gate_count
                .checked_mul(2)
                .ok_or(NexradError::InvalidMessage {
                    offset,
                    reason: "16-bit moment gate count overflow".to_owned(),
                })?;
            require_len(bytes, data_offset, byte_count, "16-bit moment gates")?;
            MomentPayload::U16(&bytes[data_offset..data_offset + byte_count])
        }
        other => {
            return Err(NexradError::InvalidMessage {
                offset,
                reason: format!("unsupported moment word size {other}"),
            });
        }
    };

    Ok(MomentBlock {
        moment,
        gate_range: GateRange {
            first_gate_m,
            gate_spacing_m,
            gate_count,
        },
        scale,
        offset: offset_value,
        row,
    })
}

fn nexrad_date_ms_to_datetime(date: u32, milliseconds: u32) -> DateTime<Utc> {
    let days = i64::from(date.saturating_sub(1));
    let seconds = days * 86_400 + i64::from(milliseconds / 1000);
    let nanos = (milliseconds % 1000) * 1_000_000;
    Utc.timestamp_opt(seconds, nanos)
        .single()
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

fn ascii_trim(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_matches(char::from(0))
        .trim()
        .to_owned()
}

fn require_len(bytes: &[u8], offset: usize, needed: usize, what: &'static str) -> Result<()> {
    let available = bytes.len().saturating_sub(offset);
    if available < needed {
        Err(NexradError::Truncated {
            what,
            offset,
            needed,
            available,
        })
    } else {
        Ok(())
    }
}

fn be_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn be_i16(bytes: &[u8], offset: usize) -> i16 {
    i16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn be_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn be_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_bits(be_u32(bytes, offset))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    require_len(bytes, offset, 4, "u32")?;
    Ok(u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn i32_at(bytes: &[u8], offset: usize) -> Result<i32> {
    require_len(bytes, offset, 4, "i32")?;
    Ok(i32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzip2::write::BzEncoder;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    #[test]
    fn parses_archive_volume_header() {
        let bytes = synthetic_archive(false);
        let header = parse_volume_header(&bytes).unwrap();

        assert_eq!(header.archive_version, "AR2V000001");
        assert_eq!(header.icao, "KTLX");
        assert_eq!(
            header.volume_time,
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 1).unwrap()
        );
    }

    #[test]
    fn gzip_capacity_hint_reads_isize_footer() {
        let mut bytes = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0, 1, 2];
        bytes.extend_from_slice(&0xfeed_beefu32.to_le_bytes());
        bytes.extend_from_slice(&1_024u32.to_le_bytes());

        assert_eq!(gzip_decoded_capacity_hint(&bytes), Some(1_024));
    }

    #[test]
    fn gzip_capacity_hint_rejects_wildly_large_trailer() {
        let mut bytes = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());

        assert_eq!(gzip_decoded_capacity_hint(&bytes), None);
    }

    #[test]
    fn parses_message_header() {
        let bytes = synthetic_archive(false);
        let header = parse_message_header(&bytes, VOLUME_HEADER_LEN + CONTROL_WORD_LEN).unwrap();

        assert_eq!(header.message_type, 31);
        assert_eq!(header.sequence_id, 7);
        assert!(usize::from(header.size_halfwords) * 2 >= MESSAGE_HEADER_LEN + MSG_31_HEADER_LEN);
    }

    #[test]
    fn parses_message_31_header() {
        let body = synthetic_message_31_body(false);
        let header = parse_message_31_header(&body, 0).unwrap();

        assert_eq!(header.azimuth_number, 1);
        assert_eq!(header.azimuth_angle, 180.5);
        assert_eq!(header.elevation_angle, 0.5);
        assert_eq!(header.radial_status, RadialStatus::StartVolume);
        assert_eq!(header.block_pointers[0], 72);
        assert_eq!(header.block_pointers[3], 136);
    }

    #[test]
    fn decodes_synthetic_message_31_volume() {
        let bytes = synthetic_archive(false);
        let volume = decode_volume_from_bytes(&bytes).unwrap();

        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.site.latitude_deg, Some(35.333));
        assert_eq!(volume.vcp, Some(VcpInfo { pattern: 212 }));
        assert_eq!(volume.cuts.len(), 1);
        assert_eq!(volume.cuts[0].radials.len(), 1);

        let reflectivity = volume.cuts[0]
            .moments
            .get(&MomentType::Reflectivity)
            .unwrap();
        assert_eq!(reflectivity.radial_count(), 1);
        assert_eq!(reflectivity.scaled_value(0, 1), Some(0.0));
        assert_eq!(reflectivity.scaled_value(0, 2), Some(7.0));
    }

    #[test]
    fn decodes_gzip_stream_without_normalized_buffer() {
        let bytes = synthetic_archive(false);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();

        let volume = decode_volume_from_bytes(&compressed).unwrap();

        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.metadata.compression, Some("gzip".to_owned()));
        assert_eq!(volume.metadata.decoded_radial_count, 1);
        assert!(volume.cuts[0].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn gzip_preview_waits_for_complete_displayable_cut() {
        let bytes = synthetic_archive(false);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();

        let preview = decode_gzip_preview_from_bytes(&compressed, 1).unwrap();

        assert!(preview.is_none());
    }

    #[test]
    fn gzip_preview_returns_completed_displayable_cut() {
        let mut bytes = synthetic_archive(false);
        set_first_synthetic_radial_status(&mut bytes, RadialStatus::EndElevation);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();

        let preview = decode_gzip_preview_from_bytes(&compressed, 1)
            .unwrap()
            .expect("completed first cut preview");

        assert_eq!(preview.site.id, "KTLX");
        assert_eq!(preview.metadata.compression, Some("gzip".to_owned()));
        assert_eq!(preview.cuts.len(), 1);
        assert_eq!(preview.cuts[0].radials.len(), 1);
        assert!(preview.cuts[0].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn gzip_preview_callback_continues_to_full_volume() {
        let mut bytes = synthetic_archive(false);
        set_first_synthetic_radial_status(&mut bytes, RadialStatus::EndElevation);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut preview_radials = None;

        let volume = decode_gzip_volume_from_bytes_with_preview(&compressed, 1, |preview| {
            preview_radials = Some(preview.metadata.decoded_radial_count);
        })
        .unwrap();

        assert_eq!(preview_radials, Some(1));
        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.metadata.compression, Some("gzip".to_owned()));
        assert_eq!(volume.metadata.decoded_radial_count, 1);
        assert!(volume.cuts[0].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn decodes_bzip_blocks_without_concatenated_normalized_buffer() {
        let bytes = synthetic_archive(false);
        let compressed = synthetic_bzip_block_archive(&bytes);

        let volume = decode_volume_from_bytes(&compressed).unwrap();

        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.metadata.compression, Some("bzip2-blocks".to_owned()));
        assert_eq!(volume.metadata.decoded_radial_count, 1);
        assert!(volume.cuts[0].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn bzip_preview_waits_for_complete_displayable_cut() {
        let bytes = synthetic_archive(false);
        let compressed = synthetic_bzip_block_archive(&bytes);

        let preview = decode_bzip_block_preview_from_bytes(&compressed, 1).unwrap();

        assert!(preview.is_none());
    }

    #[test]
    fn bzip_preview_returns_completed_displayable_cut() {
        let mut bytes = synthetic_archive(false);
        set_first_synthetic_radial_status(&mut bytes, RadialStatus::EndElevation);
        let compressed = synthetic_bzip_block_archive(&bytes);

        let preview = decode_bzip_block_preview_from_bytes(&compressed, 1)
            .unwrap()
            .expect("completed first cut preview");

        assert_eq!(preview.site.id, "KTLX");
        assert_eq!(
            preview.metadata.compression,
            Some("bzip2-blocks".to_owned())
        );
        assert_eq!(preview.cuts.len(), 1);
        assert_eq!(preview.cuts[0].radials.len(), 1);
        assert!(
            preview.cuts[0]
                .moments
                .contains_key(&MomentType::Reflectivity)
        );
    }

    #[test]
    fn bzip_preview_full_decode_reuses_path_and_returns_full_volume() {
        let mut bytes = synthetic_archive(false);
        set_first_synthetic_radial_status(&mut bytes, RadialStatus::EndElevation);
        let compressed = synthetic_bzip_block_archive(&bytes);
        let mut preview_radials = None;

        let volume = decode_volume_from_bytes_with_bzip_preview(&compressed, 1, |preview| {
            preview_radials = Some(preview.metadata.decoded_radial_count);
        })
        .unwrap();

        assert_eq!(preview_radials, Some(1));
        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.metadata.compression, Some("bzip2-blocks".to_owned()));
        assert_eq!(volume.metadata.decoded_radial_count, 1);
        assert!(volume.cuts[0].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn multi_block_bzip_decode_matches_uncompressed_reference() {
        let radials = [
            (1, 1, RadialStatus::StartVolume),
            (2, 1, RadialStatus::Intermediate),
            (3, 1, RadialStatus::EndElevation),
            (1, 2, RadialStatus::StartElevation),
            (2, 2, RadialStatus::Intermediate),
            (3, 2, RadialStatus::EndVolume),
        ];
        let archive = synthetic_multi_radial_archive(&radials);
        let reference = decode_volume_from_bytes(&archive).unwrap();

        let payload = &archive[VOLUME_HEADER_LEN..];
        let chunks: Vec<&[u8]> = payload.chunks(RECORD_BYTES * 2).collect();
        let compressed = synthetic_bzip_blocks_from_chunks(&archive, &chunks);
        let volume = decode_volume_from_bytes(&compressed).unwrap();

        assert_eq!(volume.site, reference.site);
        assert_eq!(volume.cuts, reference.cuts);
        assert_eq!(
            volume.metadata.decoded_radial_count,
            reference.metadata.decoded_radial_count
        );
    }

    #[test]
    fn bzip_preview_fires_past_legacy_block_window() {
        // First cut only completes at the 16th record, one bzip block per
        // record: more blocks than the old fixed preview scan window, so the
        // preview must come from the streaming parse, not a block prefix.
        let mut radials: Vec<(u16, u8, RadialStatus)> = vec![(1, 1, RadialStatus::StartVolume)];
        for az in 2..=15 {
            radials.push((az, 1, RadialStatus::Intermediate));
        }
        radials.push((16, 1, RadialStatus::EndElevation));
        radials.push((1, 2, RadialStatus::StartElevation));
        radials.push((2, 2, RadialStatus::EndVolume));
        let archive = synthetic_multi_radial_archive(&radials);
        let payload = &archive[VOLUME_HEADER_LEN..];
        let chunks: Vec<&[u8]> = payload.chunks(RECORD_BYTES).collect();
        let compressed = synthetic_bzip_blocks_from_chunks(&archive, &chunks);

        let mut preview_radials = None;
        let volume = decode_volume_from_bytes_with_bzip_preview(&compressed, 16, |preview| {
            preview_radials = Some(preview.metadata.decoded_radial_count);
        })
        .unwrap();

        assert_eq!(preview_radials, Some(16));
        assert_eq!(volume.metadata.decoded_radial_count, 18);
        assert_eq!(volume.cuts.len(), 2);
    }

    #[test]
    fn corrupt_trailing_bzip_block_yields_partial_volume() {
        let radials = [
            (1, 1, RadialStatus::StartVolume),
            (2, 1, RadialStatus::Intermediate),
            (3, 1, RadialStatus::Intermediate),
            (4, 1, RadialStatus::EndVolume),
        ];
        let archive = synthetic_multi_radial_archive(&radials);
        let payload = &archive[VOLUME_HEADER_LEN..];
        // Records 1-2 plus the third record's prefix land in the good block;
        // the third radial's message body crosses into the corrupted block.
        let split = 2 * RECORD_BYTES + CONTROL_WORD_LEN + MESSAGE_HEADER_LEN + 2;
        let good = bzip_compress(&payload[..split]);
        let mut bad = bzip_compress(&payload[split..]);
        for byte in bad.iter_mut().skip(8) {
            *byte = 0;
        }
        let mut compressed = archive[..VOLUME_HEADER_LEN].to_vec();
        compressed.extend_from_slice(&i32::try_from(good.len()).unwrap().to_be_bytes());
        compressed.extend_from_slice(&good);
        compressed.extend_from_slice(&(-i32::try_from(bad.len()).unwrap()).to_be_bytes());
        compressed.extend_from_slice(&bad);

        let volume = decode_volume_from_bytes(&compressed).unwrap();

        assert_eq!(volume.metadata.decoded_radial_count, 2);
        assert!(volume.metadata.skipped_message_count >= 1);
    }

    #[test]
    fn corrupt_first_bzip_block_is_a_hard_error() {
        let archive = synthetic_archive(false);
        let payload = &archive[VOLUME_HEADER_LEN..];
        let mut bad = bzip_compress(payload);
        for byte in bad.iter_mut().skip(8) {
            *byte = 0;
        }
        let mut compressed = archive[..VOLUME_HEADER_LEN].to_vec();
        compressed.extend_from_slice(&(-i32::try_from(bad.len()).unwrap()).to_be_bytes());
        compressed.extend_from_slice(&bad);

        assert!(decode_volume_from_bytes(&compressed).is_err());
    }

    #[test]
    fn pipelined_decode_works_on_single_thread_rayon_pool() {
        let radials = [
            (1, 1, RadialStatus::StartVolume),
            (2, 1, RadialStatus::Intermediate),
            (3, 1, RadialStatus::Intermediate),
            (4, 1, RadialStatus::EndVolume),
        ];
        let archive = synthetic_multi_radial_archive(&radials);
        let payload = &archive[VOLUME_HEADER_LEN..];
        let chunks: Vec<&[u8]> = payload.chunks(RECORD_BYTES).collect();
        let compressed = synthetic_bzip_blocks_from_chunks(&archive, &chunks);

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let volume = pool
            .install(|| decode_volume_from_bytes(&compressed))
            .unwrap();

        assert_eq!(volume.metadata.decoded_radial_count, 4);
    }

    #[test]
    fn decodes_synthetic_16_bit_moment() {
        let bytes = synthetic_archive(true);
        let volume = decode_volume_from_bytes(&bytes).unwrap();
        let phi = volume.cuts[0]
            .moments
            .get(&MomentType::DifferentialPhase)
            .unwrap();

        assert_eq!(phi.storage.word_size_bits(), 16);
        assert_eq!(phi.scaled_value(0, 1), Some(20.0));
    }

    #[test]
    fn expected_radials_follow_message31_azimuth_resolution_code() {
        assert_eq!(expected_radials_for_azimuth_resolution(1), 720);
        assert_eq!(expected_radials_for_azimuth_resolution(2), 360);
        assert_eq!(
            expected_radials_for_azimuth_resolution(0),
            FALLBACK_RADIALS_PER_CUT
        );
    }

    #[test]
    fn decodes_gr2_style_variable_framed_msg31_records() {
        // GR2 ".msg31" exports: AR2V header, then message 31 records packed
        // back to back (no 2432-byte fixed-record padding, no 134 metadata
        // records).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"AR2V00000");
        bytes.extend_from_slice(b"1  ");
        bytes.extend_from_slice(&19_724u32.to_be_bytes());
        bytes.extend_from_slice(&1_000u32.to_be_bytes());
        bytes.extend_from_slice(b"COW2");
        for (azimuth_number, status) in [
            (1u16, RadialStatus::StartVolume),
            (2, RadialStatus::Intermediate),
            (3, RadialStatus::EndVolume),
        ] {
            bytes.extend_from_slice(&[0u8; CONTROL_WORD_LEN]);
            let mut body = synthetic_message_31_body(false);
            body[10..12].copy_from_slice(&azimuth_number.to_be_bytes());
            body[21] = radial_status_code(status);
            let message_size = u16::try_from((MESSAGE_HEADER_LEN + body.len()) / 2).unwrap();
            bytes.extend_from_slice(&message_size.to_be_bytes());
            bytes.push(0);
            bytes.push(31);
            bytes.extend_from_slice(&7u16.to_be_bytes());
            bytes.extend_from_slice(&19_724u16.to_be_bytes());
            bytes.extend_from_slice(&1_000u32.to_be_bytes());
            bytes.extend_from_slice(&1u16.to_be_bytes());
            bytes.extend_from_slice(&1u16.to_be_bytes());
            bytes.extend_from_slice(&body);
        }

        let volume = decode_volume_from_bytes(&bytes).unwrap();

        assert_eq!(volume.site.id, "COW2");
        assert_eq!(volume.metadata.decoded_radial_count, 3);
        assert_eq!(volume.cuts[0].radials.len(), 3);
    }

    #[ignore = "set NEXRAD_LEVEL2_SAMPLE to a public Archive II file path to run manually"]
    #[test]
    fn decodes_real_public_level2_file_from_env() {
        let path = std::env::var("NEXRAD_LEVEL2_SAMPLE").expect("NEXRAD_LEVEL2_SAMPLE is not set");
        let volume = decode_volume_from_path(Path::new(&path)).unwrap();

        assert!(!volume.site.id.is_empty());
        assert!(
            !volume.cuts.is_empty(),
            "expected at least one decoded elevation cut"
        );
    }

    fn synthetic_archive(include_phi_16: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"AR2V00000");
        bytes.extend_from_slice(b"1  ");
        bytes.extend_from_slice(&19_724u32.to_be_bytes());
        bytes.extend_from_slice(&1_000u32.to_be_bytes());
        bytes.extend_from_slice(b"KTLX");

        bytes.extend_from_slice(&[0u8; CONTROL_WORD_LEN]);
        let body = synthetic_message_31_body(include_phi_16);
        let message_size = u16::try_from((MESSAGE_HEADER_LEN + body.len()) / 2).unwrap();
        bytes.extend_from_slice(&message_size.to_be_bytes());
        bytes.push(0);
        bytes.push(31);
        bytes.extend_from_slice(&7u16.to_be_bytes());
        bytes.extend_from_slice(&19_724u16.to_be_bytes());
        bytes.extend_from_slice(&1_000u32.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&body);
        bytes.resize(VOLUME_HEADER_LEN + RECORD_BYTES, 0);
        bytes
    }

    fn radial_status_code(status: RadialStatus) -> u8 {
        match status {
            RadialStatus::StartElevation => 0,
            RadialStatus::Intermediate => 1,
            RadialStatus::EndElevation => 2,
            RadialStatus::StartVolume => 3,
            RadialStatus::EndVolume => 4,
            RadialStatus::StartElevationLastCut => 5,
            RadialStatus::Unknown(value) => value,
        }
    }

    fn set_first_synthetic_radial_status(bytes: &mut [u8], status: RadialStatus) {
        let offset = VOLUME_HEADER_LEN + CONTROL_WORD_LEN + MESSAGE_HEADER_LEN + 21;
        bytes[offset] = radial_status_code(status);
    }

    /// One fixed-length record per radial: (azimuth_number, elevation_number, status).
    fn synthetic_multi_radial_archive(radials: &[(u16, u8, RadialStatus)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"AR2V00000");
        bytes.extend_from_slice(b"1  ");
        bytes.extend_from_slice(&19_724u32.to_be_bytes());
        bytes.extend_from_slice(&1_000u32.to_be_bytes());
        bytes.extend_from_slice(b"KTLX");

        for (azimuth_number, elevation_number, status) in radials {
            let record_start = bytes.len();
            bytes.extend_from_slice(&[0u8; CONTROL_WORD_LEN]);
            let mut body = synthetic_message_31_body(false);
            body[10..12].copy_from_slice(&azimuth_number.to_be_bytes());
            let azimuth_deg = f32::from(*azimuth_number) * 0.5;
            body[12..16].copy_from_slice(&azimuth_deg.to_bits().to_be_bytes());
            body[21] = radial_status_code(*status);
            body[22] = *elevation_number;
            let message_size = u16::try_from((MESSAGE_HEADER_LEN + body.len()) / 2).unwrap();
            bytes.extend_from_slice(&message_size.to_be_bytes());
            bytes.push(0);
            bytes.push(31);
            bytes.extend_from_slice(&7u16.to_be_bytes());
            bytes.extend_from_slice(&19_724u16.to_be_bytes());
            bytes.extend_from_slice(&1_000u32.to_be_bytes());
            bytes.extend_from_slice(&1u16.to_be_bytes());
            bytes.extend_from_slice(&1u16.to_be_bytes());
            bytes.extend_from_slice(&body);
            bytes.resize(record_start + RECORD_BYTES, 0);
        }
        bytes
    }

    fn bzip_compress(payload: &[u8]) -> Vec<u8> {
        let mut encoder = BzEncoder::new(Vec::new(), bzip2::Compression::default());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    /// Assemble an LDM block-bzip archive from pre-split payload chunks, with
    /// the real-file convention of a negative size on the final block.
    fn synthetic_bzip_blocks_from_chunks(archive: &[u8], chunks: &[&[u8]]) -> Vec<u8> {
        let mut bytes = archive[..VOLUME_HEADER_LEN].to_vec();
        for (index, chunk) in chunks.iter().enumerate() {
            let compressed = bzip_compress(chunk);
            let len = i32::try_from(compressed.len()).expect("compressed block length fits");
            let signed = if index + 1 == chunks.len() { -len } else { len };
            bytes.extend_from_slice(&signed.to_be_bytes());
            bytes.extend_from_slice(&compressed);
        }
        bytes
    }

    fn synthetic_bzip_block_archive(normalized: &[u8]) -> Vec<u8> {
        let mut encoder = BzEncoder::new(Vec::new(), bzip2::Compression::default());
        encoder.write_all(&normalized[VOLUME_HEADER_LEN..]).unwrap();
        let compressed_block = encoder.finish().unwrap();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&normalized[..VOLUME_HEADER_LEN]);
        bytes.extend_from_slice(
            &i32::try_from(compressed_block.len())
                .expect("compressed block length fits")
                .to_be_bytes(),
        );
        bytes.extend_from_slice(&compressed_block);
        bytes.extend_from_slice(&(-1_i32).to_be_bytes());
        bytes
    }

    fn synthetic_message_31_body(include_phi_16: bool) -> Vec<u8> {
        let mut body = vec![0u8; MSG_31_HEADER_LEN];
        body[0..4].copy_from_slice(b"AR2V");
        body[4..8].copy_from_slice(&1_000u32.to_be_bytes());
        body[8..10].copy_from_slice(&19_724u16.to_be_bytes());
        body[10..12].copy_from_slice(&1u16.to_be_bytes());
        body[12..16].copy_from_slice(&180.5f32.to_bits().to_be_bytes());
        body[18..20].copy_from_slice(&1u16.to_be_bytes());
        body[20] = 2;
        body[21] = 3;
        body[22] = 1;
        body[23] = 1;
        body[24..28].copy_from_slice(&0.5f32.to_bits().to_be_bytes());
        body[30..32].copy_from_slice(&(if include_phi_16 { 5u16 } else { 4u16 }).to_be_bytes());

        let vol_pointer = body.len();
        push_volume_block(&mut body);
        let rad_pointer = body.len();
        push_radial_block(&mut body);
        let ref_pointer = body.len();
        push_u8_moment(&mut body, b"DREF", &[0, 66, 80]);
        let vel_pointer = body.len();
        push_u8_moment(&mut body, b"DVEL", &[129, 139, 119]);
        let phi_pointer = body.len();
        if include_phi_16 {
            push_u16_moment(&mut body, b"DPHI", &[0, 20, 40]);
        }

        set_pointer(&mut body, 0, vol_pointer);
        set_pointer(&mut body, 2, rad_pointer);
        set_pointer(&mut body, 3, ref_pointer);
        set_pointer(&mut body, 4, vel_pointer);
        if include_phi_16 {
            set_pointer(&mut body, 7, phi_pointer);
        }
        body
    }

    fn push_volume_block(body: &mut Vec<u8>) {
        body.extend_from_slice(b"RVOL");
        body.extend_from_slice(&1u16.to_be_bytes());
        body.push(1);
        body.push(0);
        body.extend_from_slice(&35.333f32.to_bits().to_be_bytes());
        body.extend_from_slice(&(-97.277f32).to_bits().to_be_bytes());
        body.extend_from_slice(&370i16.to_be_bytes());
        body.extend_from_slice(&20u16.to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&212u16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
    }

    fn push_radial_block(body: &mut Vec<u8>) {
        body.extend_from_slice(b"RRAD");
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&2_500i16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
    }

    fn push_u8_moment(body: &mut Vec<u8>, id: &[u8; 4], gates: &[u8]) {
        body.extend_from_slice(id);
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(gates.len() as u16).to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&250i16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.push(0);
        body.push(8);
        body.extend_from_slice(&2.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&66.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(gates);
        if !body.len().is_multiple_of(2) {
            body.push(0);
        }
    }

    fn push_u16_moment(body: &mut Vec<u8>, id: &[u8; 4], gates: &[u16]) {
        body.extend_from_slice(id);
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(gates.len() as u16).to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&250i16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.push(0);
        body.push(16);
        body.extend_from_slice(&1.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        for gate in gates {
            body.extend_from_slice(&gate.to_be_bytes());
        }
    }

    fn set_pointer(body: &mut [u8], pointer_index: usize, value: usize) {
        let offset = 32 + pointer_index * 4;
        body[offset..offset + 4].copy_from_slice(&(value as u32).to_be_bytes());
    }
}
