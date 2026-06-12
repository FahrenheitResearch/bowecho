//! Native DORADE sweepfile (`swp.*`) decoder for mobile research radars
//! (DOW6/DOW7/DOW8, COW, RaXPol, and other CSWR/OU sweepfile producers).
//!
//! Decodes directly into [`radar_core::RadarVolume`] with no intermediate
//! volume model: each sweepfile contributes one [`radar_core::ElevationCut`]
//! whose moments live in compact [`radar_core::MomentGrid`] storage (16-bit
//! DORADE integers stay 16-bit, shifted into unsigned space so the grid's
//! `(raw - offset) / scale` matches DORADE's `(raw - bias) / scale`).
//!
//! Format references:
//! - R. Oye and M. Case, "DORADE Data Format" (NCAR/ATD, 1995; revised
//!   2003/2010 by W.-C. Lee, NCAR/EOL) — block layouts and semantics.
//! - lrose-core `DoradeData.hh` (NCAR/EOL) — authoritative struct offsets.
//! - HRD run-length encoding from the NOAA Hurricane Research Division as
//!   described in the DORADE document and implemented in `soloii`/Radx.
//!
//! This is a lift-and-improve of the reference implementation in
//! `gurt-rs/src/dorade.rs` (rustwx work tree, 2026-05-31). Divergences:
//! - **CSFD support**: COW2 and RaXPol sweepfiles carry gate geometry in the
//!   `CSFD` (cell spacing format descriptor) block, not `CELV`; the reference
//!   only parsed `CELV` and silently fell back to extended-PARM fields, which
//!   the 1995-format 104-byte PARM (DOW7) does not have.
//! - **VOLD date offsets fixed**: the reference read the volume date at
//!   offset 32; the standard layout puts `year` at 36 (verified against real
//!   COW2 bytes). SSWB remains the primary time source.
//! - **Transition-ray filtering**: rays with RYIB `ray_status != 0` (antenna
//!   moving between fixed angles) are dropped. A real DOW7 Goshen sweepfile
//!   is 42% transition rays spanning 0.5°-11.4° inside a "0.5°" sweep; the
//!   reference kept them, smearing the PPI. Sweeps whose rays are *all*
//!   flagged in-transition (a writer quirk in the same corpus) keep their
//!   rays instead of erroring.
//! - **RADD layout**: the standard 1995 layout (lat/lon/alt at 80/84/88,
//!   `data_compress` at 68) is parsed directly; the reference parsed a
//!   shifted legacy layout first and patched it afterwards.
//! - **CFAC corrections**: azimuth/elevation/range-delay/lat/lon correction
//!   factors are applied when present (all-zero in the observed corpus, but
//!   cheap and correct; Radx applies them unconditionally too).
//! - **Per-ray times**: RYIB julian day + h/m/s/ms become
//!   `Radial::time_offset_ms`; the reference dropped ray times.
//! - **Binary formats**: 8-bit int, 16-bit int, 32-bit int, and 32-bit float
//!   PARM data are supported; the reference assumed 16-bit everywhere.
//! - **Staggered-PRT Nyquist**: the extended unambiguous velocity falls back
//!   to `λ / (4·(T2 − T1))` (Zrnić and Mahapatra 1985, IEEE Trans. AES-21;
//!   Torres, Dubel, and Zrnić 2004, J. Atmos. Oceanic Technol. 21,
//!   1389–1399) when RADD `eff_unamb_vel` is missing; the reference used
//!   `m·Va_short`, which is only correct for `n − m = 1` stagger ratios.
//!
//! Known limitations (documented, not silent):
//! - Multi-segment CSFD range geometry is flattened to the first segment's
//!   spacing because [`radar_core::GateRange`] models uniform gates only.
//! - Per-ray platform georeferencing (`ASIB`) is ignored: DOW/COW/RaXPol
//!   deployments are parked, so the RADD site position applies to the whole
//!   sweep. Airborne tail radars would need ASIB handling.
//! - RHI sweeps decode as cuts ordered by their fixed angle (the AZIMUTH for
//!   an RHI — `ElevationCut::elevation_deg` holds it); the RADD `scan_mode`
//!   is surfaced as [`radar_core::ScanMode`] in the volume metadata so
//!   displays can render a range-height panel instead of a plan view.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use radar_core::{
    GateRange, MomentGrid, MomentRow, MomentType, RadarSite, RadarVolume, Radial, ScanMode,
};

use crate::{NexradError, Result};

const BLOCK_HEADER_LEN: usize = 8;
const DORADE_BAD_F32: f32 = -9999.0;
/// DORADE altitude fields are kilometres MSL.
const KM_TO_M: f64 = 1000.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn i16(self, bytes: &[u8], offset: usize) -> i16 {
        let raw = [bytes[offset], bytes[offset + 1]];
        match self {
            Self::Little => i16::from_le_bytes(raw),
            Self::Big => i16::from_be_bytes(raw),
        }
    }

    fn i32(self, bytes: &[u8], offset: usize) -> i32 {
        let raw = [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ];
        match self {
            Self::Little => i32::from_le_bytes(raw),
            Self::Big => i32::from_be_bytes(raw),
        }
    }

    fn f32(self, bytes: &[u8], offset: usize) -> f32 {
        f32::from_bits(self.i32(bytes, offset) as u32)
    }
}

/// Cheap header peek used to group sweepfiles into volume scans without a
/// full decode. Parsing stops at the first ray.
#[derive(Clone, Debug, PartialEq)]
pub struct DoradeSweepHeader {
    pub instrument: String,
    pub volume_number: i32,
    pub sweep_number: i32,
    pub fixed_angle_deg: f32,
    pub start_time: Option<DateTime<Utc>>,
    pub latitude_deg: f32,
    pub longitude_deg: f32,
    pub altitude_m: f32,
}

/// `true` when the buffer starts with a plausible DORADE descriptor block.
///
/// Sweepfiles written by solo/Radx begin with `COMM`, `SSWB`, or `VOLD`;
/// the 4-byte length that follows must be valid in at least one byte order.
pub fn looks_like_dorade_bytes(bytes: &[u8]) -> bool {
    if bytes.len() < BLOCK_HEADER_LEN {
        return false;
    }
    if !matches!(&bytes[..4], b"COMM" | b"SSWB" | b"VOLD" | b"RADD") {
        return false;
    }
    detect_endian(bytes).is_ok()
}

/// `true` when the file name uses the `swp.*` sweepfile convention.
pub fn looks_like_dorade_name(name: &str) -> bool {
    let file_name = name
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    file_name.starts_with("swp.") || file_name.ends_with(".swp") || file_name.ends_with(".dorade")
}

/// Convenience: path-based variant of [`looks_like_dorade_name`].
pub fn looks_like_dorade_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(looks_like_dorade_name)
}

/// Parse only the descriptor blocks (everything before the first ray).
pub fn peek_dorade_sweep(bytes: &[u8]) -> Result<DoradeSweepHeader> {
    let mut parse = SweepParse::new(detect_endian(bytes)?);
    parse.run(bytes, true)?;
    Ok(DoradeSweepHeader {
        instrument: parse.instrument.clone(),
        volume_number: parse.volume_number,
        sweep_number: parse.sweep_number,
        fixed_angle_deg: parse.fixed_angle_deg,
        start_time: parse.start_time,
        latitude_deg: parse.site_latitude_deg(),
        longitude_deg: parse.site_longitude_deg(),
        altitude_m: parse.site_altitude_m(),
    })
}

/// Decode one sweepfile into a fresh single-cut volume.
pub fn decode_dorade_sweep_volume(bytes: &[u8]) -> Result<RadarVolume> {
    let mut volume = RadarVolume::default();
    append_dorade_sweep(bytes, &mut volume)?;
    finalize_dorade_volume(&mut volume);
    Ok(volume)
}

/// Decode a set of sweepfiles forming one volume scan.
///
/// Cuts are appended in input order and then sorted by elevation (ties keep
/// input order, which the callers arrange to be scan time). The site
/// position comes from the first sweep's RADD block — mobile radars move
/// between deployments, so the coordinates always come from the file.
pub fn decode_dorade_volume_from_slices<S: AsRef<[u8]>>(sweeps: &[S]) -> Result<RadarVolume> {
    if sweeps.is_empty() {
        return Err(invalid(0, "no DORADE sweeps to decode"));
    }
    let mut volume = RadarVolume::default();
    for sweep in sweeps {
        append_dorade_sweep(sweep.as_ref(), &mut volume)?;
    }
    finalize_dorade_volume(&mut volume);
    Ok(volume)
}

/// Decode a set of sweepfile paths forming one volume scan.
pub fn decode_dorade_volume_from_paths<P: AsRef<Path>>(paths: &[P]) -> Result<RadarVolume> {
    if paths.is_empty() {
        return Err(invalid(0, "no DORADE sweep paths to decode"));
    }
    let mut volume = RadarVolume::default();
    for path in paths {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| NexradError::Io {
            path: path.display().to_string(),
            source,
        })?;
        append_dorade_sweep(&bytes, &mut volume)?;
    }
    volume.metadata.source_path = Some(paths[0].as_ref().display().to_string());
    finalize_dorade_volume(&mut volume);
    Ok(volume)
}

/// Decode one sweepfile and append it as a cut on `volume`.
///
/// The first appended sweep populates the site, volume time, and metadata;
/// later sweeps must come from the same instrument.
pub fn append_dorade_sweep(bytes: &[u8], volume: &mut RadarVolume) -> Result<()> {
    let mut parse = SweepParse::new(detect_endian(bytes)?);
    parse.run(bytes, false)?;
    parse.finish_into(volume)
}

/// Sort cuts by elevation and refresh volume-level bookkeeping. Called once
/// after the last [`append_dorade_sweep`].
pub fn finalize_dorade_volume(volume: &mut RadarVolume) {
    // Stable sort: same-elevation cuts (single-tilt COW2 sequences) keep
    // their scan-time order.
    volume
        .cuts
        .sort_by(|left, right| left.elevation_deg.total_cmp(&right.elevation_deg));
    volume.metadata.decoded_radial_count = volume.cuts.iter().map(|cut| cut.radials.len()).sum();
}

fn detect_endian(bytes: &[u8]) -> Result<Endian> {
    if bytes.len() < BLOCK_HEADER_LEN {
        return Err(NexradError::Truncated {
            what: "DORADE block header",
            offset: 0,
            needed: BLOCK_HEADER_LEN,
            available: bytes.len(),
        });
    }
    let le = Endian::Little.i32(bytes, 4);
    let be = Endian::Big.i32(bytes, 4);
    let len = bytes.len() as i64;
    let le_ok = le as i64 >= BLOCK_HEADER_LEN as i64 && le as i64 <= len;
    let be_ok = be as i64 >= BLOCK_HEADER_LEN as i64 && be as i64 <= len;
    match (le_ok, be_ok) {
        (true, false) => Ok(Endian::Little),
        (false, true) => Ok(Endian::Big),
        // Network byte order is the DORADE default; prefer it on a tie.
        (true, true) => Ok(Endian::Big),
        (false, false) => Err(invalid(4, "cannot determine DORADE byte order")),
    }
}

/// One PARM descriptor plus the moment grid it feeds.
struct ParamState {
    name: String,
    scale: f32,
    bias: f32,
    bad_data: i32,
    /// DORADE `binary_format`: 1 = i8, 2 = i16, 3 = i32, 4 = f32.
    binary_format: i16,
    /// Extended (1997+) PARM gate metadata; the 104-byte 1995 PARM lacks it.
    number_cells: Option<usize>,
    first_cell_m: Option<f32>,
    cell_spacing_m: Option<f32>,
    moment: MomentType,
    grid: Option<MomentGrid>,
    /// Decoded row for the in-flight ray, if any.
    pending_row: Option<MomentRow>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Cfac {
    azimuth_deg: f32,
    elevation_deg: f32,
    range_delay_m: f32,
    longitude_deg: f32,
    latitude_deg: f32,
    radar_altitude_km: f32,
}

#[derive(Clone, Copy, Debug)]
struct PendingRay {
    azimuth_deg: f32,
    elevation_deg: f32,
    /// RYIB `ray_status`: 0 = normal, 1 = in transition, 2 = bad.
    status: i32,
    time: Option<DateTime<Utc>>,
}

struct SweepParse {
    endian: Endian,
    instrument: String,
    volume_number: i32,
    sweep_number: i32,
    fixed_angle_deg: f32,
    scan_mode: i16,
    compression: i16,
    radd_longitude_deg: f32,
    radd_latitude_deg: f32,
    radd_altitude_km: f32,
    eff_unamb_vel_mps: Option<f32>,
    frequency_ghz: Option<f32>,
    prt1_ms: Option<f32>,
    prt2_ms: Option<f32>,
    num_ipps_trans: Option<i16>,
    cfac: Cfac,
    start_time: Option<DateTime<Utc>>,
    vold_date: Option<NaiveDate>,
    params: Vec<ParamState>,
    /// CELV per-cell ranges or CSFD-derived uniform axis.
    range_first_m: Option<f32>,
    range_spacing_m: Option<f32>,
    range_gate_count: Option<usize>,
    rays: Vec<(PendingRay, Vec<(usize, MomentRow)>)>,
    /// Antenna-transition rays, kept aside so an all-transition sweep (seen
    /// in the 2009 Goshen DOW7 corpus) can still decode instead of erroring.
    transition_rays: Vec<(PendingRay, Vec<(usize, MomentRow)>)>,
    current_ray: Option<PendingRay>,
    skipped_field_blocks: usize,
}

impl SweepParse {
    fn new(endian: Endian) -> Self {
        Self {
            endian,
            instrument: String::new(),
            volume_number: 0,
            sweep_number: 0,
            fixed_angle_deg: f32::NAN,
            scan_mode: 8,
            compression: 0,
            radd_longitude_deg: f32::NAN,
            radd_latitude_deg: f32::NAN,
            radd_altitude_km: f32::NAN,
            eff_unamb_vel_mps: None,
            frequency_ghz: None,
            prt1_ms: None,
            prt2_ms: None,
            num_ipps_trans: None,
            cfac: Cfac::default(),
            start_time: None,
            vold_date: None,
            params: Vec::new(),
            range_first_m: None,
            range_spacing_m: None,
            range_gate_count: None,
            rays: Vec::new(),
            transition_rays: Vec::new(),
            current_ray: None,
            skipped_field_blocks: 0,
        }
    }

    fn site_latitude_deg(&self) -> f32 {
        self.radd_latitude_deg + self.cfac.latitude_deg
    }

    fn site_longitude_deg(&self) -> f32 {
        self.radd_longitude_deg + self.cfac.longitude_deg
    }

    fn site_altitude_m(&self) -> f32 {
        ((self.radd_altitude_km + self.cfac.radar_altitude_km) as f64 * KM_TO_M) as f32
    }

    fn run(&mut self, bytes: &[u8], stop_at_first_ray: bool) -> Result<()> {
        let mut pos = 0usize;
        while pos + BLOCK_HEADER_LEN <= bytes.len() {
            let id: [u8; 4] = bytes[pos..pos + 4].try_into().expect("4-byte block id");
            let nbytes = self.endian.i32(bytes, pos + 4);
            if nbytes < BLOCK_HEADER_LEN as i32 {
                // NULL terminator blocks or padding: stop cleanly at a
                // recognizable end marker, error otherwise.
                if &id == b"NULL" || id == [0; 4] {
                    break;
                }
                return Err(invalid(pos, format!("invalid DORADE block size {nbytes}")));
            }
            let end = pos + nbytes as usize;
            if end > bytes.len() {
                // Tolerate a truncated trailing ray (partial downloads); the
                // descriptor blocks must be complete for a usable sweep.
                if !self.rays.is_empty() {
                    break;
                }
                return Err(NexradError::Truncated {
                    what: "DORADE block",
                    offset: pos,
                    needed: nbytes as usize,
                    available: bytes.len() - pos,
                });
            }
            let block = &bytes[pos..end];
            match &id {
                b"VOLD" => self.parse_vold(block, pos)?,
                b"RADD" => self.parse_radd(block, pos)?,
                b"CFAC" => self.parse_cfac(block),
                b"PARM" => self.parse_parm(block, pos)?,
                b"CELV" => self.parse_celv(block, pos)?,
                b"CSFD" => self.parse_csfd(block, pos)?,
                b"SWIB" => self.parse_swib(block, pos)?,
                b"SSWB" => self.parse_sswb(block, pos)?,
                b"RYIB" => {
                    if stop_at_first_ray {
                        return Ok(());
                    }
                    self.finish_current_ray();
                    self.current_ray = Some(self.parse_ryib(block, pos)?);
                }
                b"RDAT" => self.parse_rdat(block, pos)?,
                // COMM, ASIB, XSTF, RKTB, SEDS, FRIB, FRAD, WAVE, ...: skipped.
                _ => {}
            }
            pos = end;
        }
        self.finish_current_ray();
        Ok(())
    }

    fn parse_vold(&mut self, block: &[u8], offset: usize) -> Result<()> {
        require(block, 48, offset, "VOLD")?;
        self.volume_number = i32::from(self.endian.i16(block, 10));
        // Standard layout: proj_name[20] at 16, then year at 36 (the
        // reference read offset 32, which lands inside proj_name).
        let year = i32::from(self.endian.i16(block, 36));
        let month = self.endian.i16(block, 38);
        let day = self.endian.i16(block, 40);
        let hour = self.endian.i16(block, 42);
        let minute = self.endian.i16(block, 44);
        let second = self.endian.i16(block, 46);
        if let Some(date) = NaiveDate::from_ymd_opt(year, month.max(0) as u32, day.max(0) as u32) {
            self.vold_date = Some(date);
            if self.start_time.is_none() {
                self.start_time = date
                    .and_hms_opt(
                        hour.max(0) as u32,
                        minute.max(0) as u32,
                        second.max(0) as u32,
                    )
                    .map(|naive| Utc.from_utc_datetime(&naive));
            }
        }
        Ok(())
    }

    fn parse_radd(&mut self, block: &[u8], offset: usize) -> Result<()> {
        // The standard 1995 RADD is 144 bytes; Radx writes a 300-byte
        // extended version with identical leading offsets.
        require(block, 144, offset, "RADD")?;
        self.instrument = text(&block[8..16]);
        self.scan_mode = self.endian.i16(block, 50);
        self.compression = self.endian.i16(block, 68);
        self.radd_longitude_deg = self.endian.f32(block, 80);
        self.radd_latitude_deg = self.endian.f32(block, 84);
        self.radd_altitude_km = self.endian.f32(block, 88);
        self.eff_unamb_vel_mps = valid_dorade_f32(self.endian.f32(block, 92));
        self.num_ipps_trans = Some(self.endian.i16(block, 102));
        self.frequency_ghz = valid_dorade_f32(self.endian.f32(block, 104))
            .filter(|freq| (0.1..=300.0).contains(freq));
        self.prt1_ms = valid_dorade_f32(self.endian.f32(block, 124));
        self.prt2_ms = valid_dorade_f32(self.endian.f32(block, 128));
        Ok(())
    }

    fn parse_cfac(&mut self, block: &[u8]) {
        // CFAC: nine correction floats starting at offset 8 (azimuth,
        // elevation, range delay, longitude, latitude, pressure alt, radar
        // alt, EW ground speed, NS ground speed, ...).
        if block.len() < 36 {
            return;
        }
        self.cfac = Cfac {
            azimuth_deg: self.endian.f32(block, 8),
            elevation_deg: self.endian.f32(block, 12),
            range_delay_m: self.endian.f32(block, 16),
            longitude_deg: self.endian.f32(block, 20),
            latitude_deg: self.endian.f32(block, 24),
            radar_altitude_km: self.endian.f32(block, 32),
        };
    }

    fn parse_parm(&mut self, block: &[u8], offset: usize) -> Result<()> {
        require(block, 104, offset, "PARM")?;
        let name = text(&block[8..16]);
        let binary_format = self.endian.i16(block, 78);
        let scale = self.endian.f32(block, 92);
        let bias = self.endian.f32(block, 96);
        let bad_data = self.endian.i32(block, 100);
        // 1997+ extended PARM (216 bytes) carries per-field gate geometry.
        let (number_cells, first_cell_m, cell_spacing_m) = if block.len() >= 212 {
            (
                Some(self.endian.i32(block, 200).max(0) as usize),
                Some(self.endian.f32(block, 204)),
                Some(self.endian.f32(block, 208)),
            )
        } else {
            (None, None, None)
        };
        self.params.push(ParamState {
            name,
            scale: if scale.abs() > 1.0e-6 { scale } else { 1.0 },
            bias,
            bad_data,
            binary_format,
            number_cells,
            first_cell_m,
            cell_spacing_m,
            moment: MomentType::Unknown(String::new()),
            grid: None,
            pending_row: None,
        });
        Ok(())
    }

    fn parse_celv(&mut self, block: &[u8], offset: usize) -> Result<()> {
        require(block, 16, offset, "CELV")?;
        let cells = self.endian.i32(block, 8).max(0) as usize;
        let available = (block.len() - 12) / 4;
        let count = cells.min(available);
        if count == 0 {
            return Ok(());
        }
        let first = self.endian.f32(block, 12);
        let spacing = if count >= 2 {
            // CELV lists every cell range; radar_core models uniform gates,
            // so use the lead spacing (uniform in the observed corpus).
            self.endian.f32(block, 16) - first
        } else {
            0.0
        };
        self.range_first_m = Some(first);
        self.range_spacing_m = Some(spacing);
        self.range_gate_count = Some(count);
        Ok(())
    }

    fn parse_csfd(&mut self, block: &[u8], offset: usize) -> Result<()> {
        // CSFD: num_segments (i32 at 8), dist_to_first (f32 at 12),
        // spacing[8] (f32 at 16), num_cells[8] (i16 at 48). 64 bytes.
        require(block, 64, offset, "CSFD")?;
        let segments = self.endian.i32(block, 8).clamp(0, 8) as usize;
        if segments == 0 {
            return Ok(());
        }
        let first = self.endian.f32(block, 12);
        let spacing = self.endian.f32(block, 16);
        let mut total_cells = 0usize;
        for segment in 0..segments {
            total_cells += self.endian.i16(block, 48 + segment * 2).max(0) as usize;
        }
        if total_cells == 0 {
            return Ok(());
        }
        // Multi-segment geometry flattens to the first segment's spacing;
        // see module docs.
        self.range_first_m = Some(first);
        self.range_spacing_m = Some(spacing);
        self.range_gate_count = Some(total_cells);
        Ok(())
    }

    fn parse_swib(&mut self, block: &[u8], offset: usize) -> Result<()> {
        require(block, 36, offset, "SWIB")?;
        self.sweep_number = self.endian.i32(block, 16);
        self.fixed_angle_deg = self.endian.f32(block, 32);
        Ok(())
    }

    fn parse_sswb(&mut self, block: &[u8], offset: usize) -> Result<()> {
        require(block, 20, offset, "SSWB")?;
        let start = self.endian.i32(block, 12);
        if start > 0 {
            self.start_time = DateTime::<Utc>::from_timestamp(i64::from(start), 0);
        }
        Ok(())
    }

    fn parse_ryib(&mut self, block: &[u8], offset: usize) -> Result<PendingRay> {
        require(block, 44, offset, "RYIB")?;
        let julian_day = self.endian.i32(block, 12);
        let hour = self.endian.i16(block, 16);
        let minute = self.endian.i16(block, 18);
        let second = self.endian.i16(block, 20);
        let millisecond = self.endian.i16(block, 22);
        let time = self.ray_time(julian_day, hour, minute, second, millisecond);
        Ok(PendingRay {
            azimuth_deg: self.endian.f32(block, 24) + self.cfac.azimuth_deg,
            elevation_deg: self.endian.f32(block, 28) + self.cfac.elevation_deg,
            status: self.endian.i32(block, 40),
            time,
        })
    }

    fn ray_time(
        &self,
        julian_day: i32,
        hour: i16,
        minute: i16,
        second: i16,
        millisecond: i16,
    ) -> Option<DateTime<Utc>> {
        let base_year = self
            .start_time
            .map(|time| time.date_naive())
            .or(self.vold_date)?
            .year();
        if !(1..=366).contains(&julian_day) {
            return None;
        }
        let date = NaiveDate::from_yo_opt(base_year, julian_day as u32)?;
        let naive = date.and_hms_milli_opt(
            hour.clamp(0, 23) as u32,
            minute.clamp(0, 59) as u32,
            second.clamp(0, 59) as u32,
            millisecond.clamp(0, 999) as u32,
        )?;
        let mut time = Utc.from_utc_datetime(&naive);
        // Year rollover: a sweep started Dec 31 can have rays on Jan 1.
        if let Some(start) = self.start_time {
            if time < start - Duration::days(180) {
                let next = NaiveDate::from_yo_opt(base_year + 1, julian_day as u32)?;
                time = Utc.from_utc_datetime(&next.and_time(naive.time()));
            } else if time > start + Duration::days(180) {
                let previous = NaiveDate::from_yo_opt(base_year - 1, julian_day as u32)?;
                time = Utc.from_utc_datetime(&previous.and_time(naive.time()));
            }
        }
        Some(time)
    }

    fn parse_rdat(&mut self, block: &[u8], offset: usize) -> Result<()> {
        if self.current_ray.is_none() {
            return Ok(());
        }
        require(block, 16, offset, "RDAT")?;
        let name = text(&block[8..16]);
        let Some(param_index) = self.params.iter().position(|param| param.name == name) else {
            self.skipped_field_blocks += 1;
            return Ok(());
        };
        let payload = &block[16..];
        let endian = self.endian;
        let compressed = self.compression == 1;
        let gate_count = self.gate_count_for_param(param_index);
        let param = &mut self.params[param_index];
        let row = match param.binary_format {
            1 => {
                // i8 → u8 storage; +128 keeps (raw − offset)/scale intact.
                let row = payload
                    .iter()
                    .map(|byte| (*byte as i8 as i16 + 128) as u8)
                    .collect();
                MomentRow::U8(row)
            }
            2 => {
                let words: Vec<i16> = payload
                    .chunks_exact(2)
                    .map(|pair| match endian {
                        Endian::Little => i16::from_le_bytes([pair[0], pair[1]]),
                        Endian::Big => i16::from_be_bytes([pair[0], pair[1]]),
                    })
                    .collect();
                let words = if compressed {
                    let gates = gate_count.ok_or_else(|| {
                        invalid(
                            offset,
                            format!("no gate count for compressed DORADE field '{name}'"),
                        )
                    })?;
                    decode_hrd_rle(&words, gates, param.bad_data as i16)
                } else {
                    words
                };
                // i16 → u16 storage; +32768 keeps (raw − offset)/scale intact.
                MomentRow::U16(
                    words
                        .into_iter()
                        .map(|word| (i32::from(word) + 32768) as u16)
                        .collect(),
                )
            }
            3 => MomentRow::F32(
                payload
                    .chunks_exact(4)
                    .map(|quad| {
                        let raw = endian.i32(quad, 0);
                        if raw == param.bad_data {
                            f32::NAN
                        } else {
                            (raw as f32 - param.bias) / param.scale
                        }
                    })
                    .collect(),
            ),
            4 => MomentRow::F32(
                payload
                    .chunks_exact(4)
                    .map(|quad| {
                        let raw = endian.f32(quad, 0);
                        if raw == param.bad_data as f32 || raw <= DORADE_BAD_F32 {
                            f32::NAN
                        } else {
                            (raw - param.bias) / param.scale
                        }
                    })
                    .collect(),
            ),
            _ => {
                // 16-bit float (format 5) is unobserved in the wild corpus;
                // skip the field rather than failing the sweep.
                self.skipped_field_blocks += 1;
                return Ok(());
            }
        };
        param.pending_row = Some(row);
        Ok(())
    }

    fn gate_count_for_param(&self, param_index: usize) -> Option<usize> {
        self.range_gate_count
            .or_else(|| self.params[param_index].number_cells)
    }

    fn finish_current_ray(&mut self) {
        let Some(ray) = self.current_ray.take() else {
            return;
        };
        let rows: Vec<(usize, MomentRow)> = self
            .params
            .iter_mut()
            .enumerate()
            .filter_map(|(index, param)| param.pending_row.take().map(|row| (index, row)))
            .collect();
        // ray_status: 0 = normal, 1 = antenna in transition, 2 = bad.
        if ray.status != 0 {
            self.transition_rays.push((ray, rows));
        } else {
            self.rays.push((ray, rows));
        }
    }

    fn gate_range(&self) -> Result<GateRange> {
        if let (Some(first), Some(spacing), Some(count)) = (
            self.range_first_m,
            self.range_spacing_m,
            self.range_gate_count,
        ) {
            return Ok(GateRange {
                first_gate_m: (first + self.cfac.range_delay_m).round() as i32,
                gate_spacing_m: spacing.round().max(1.0) as i32,
                gate_count: count,
            });
        }
        let param = self
            .params
            .iter()
            .find(|param| param.number_cells.unwrap_or(0) > 0)
            .ok_or_else(|| invalid(0, "DORADE sweep has no CELV/CSFD/PARM range metadata"))?;
        Ok(GateRange {
            first_gate_m: (param.first_cell_m.unwrap_or(0.0) + self.cfac.range_delay_m).round()
                as i32,
            gate_spacing_m: param.cell_spacing_m.unwrap_or(1000.0).round().max(1.0) as i32,
            gate_count: param.number_cells.unwrap_or(0),
        })
    }

    /// Effective Nyquist (fold) velocity for the recorded velocity field.
    ///
    /// RADD `eff_unamb_vel` is authoritative when present: for staggered-PRT
    /// systems it already holds the extended unambiguous velocity the radar
    /// dealiased to. Otherwise fall back to the wavelength/PRT relations
    /// (Doviak and Zrnić 1993, eq. 3.17; Torres, Dubel, and Zrnić 2004 for
    /// the staggered extension λ/(4·(T2 − T1))).
    fn nyquist_velocity_mps(&self) -> Option<f32> {
        if let Some(value) = self.eff_unamb_vel_mps.filter(|value| *value > 0.0) {
            return Some(value);
        }
        let wavelength_m = 299_792_458.0f32 / (self.frequency_ghz? * 1.0e9);
        let mut prts: Vec<f32> = [self.prt1_ms, self.prt2_ms]
            .into_iter()
            .flatten()
            .filter(|prt| *prt > 0.0)
            .map(|prt| prt / 1000.0)
            .collect();
        prts.sort_by(f32::total_cmp);
        match prts.as_slice() {
            [] => None,
            [short] => Some(wavelength_m / (4.0 * short)),
            [short, long, ..] => {
                if self.num_ipps_trans.unwrap_or(1) >= 2 && (long - short) > f32::EPSILON {
                    Some(wavelength_m / (4.0 * (long - short)))
                } else {
                    Some(wavelength_m / (4.0 * short))
                }
            }
        }
    }

    fn finish_into(mut self, volume: &mut RadarVolume) -> Result<()> {
        let mut skipped_transition_rays = self.transition_rays.len();
        if self.rays.is_empty() {
            if self.transition_rays.is_empty() {
                return Err(invalid(0, "DORADE sweep contains no rays"));
            }
            // All-transition sweep (e.g. 2009 Goshen DOW7 v4): the status
            // flag is the only thing wrong with the data, so keep it rather
            // than failing the whole volume/archive.
            self.rays = std::mem::take(&mut self.transition_rays);
            skipped_transition_rays = 0;
        }
        if self.instrument.is_empty() {
            self.instrument = "DORADE".to_owned();
        }
        if volume.site.id.is_empty() {
            volume.site = RadarSite {
                id: self.instrument.clone(),
                name: Some(format!("{} (mobile)", self.instrument)),
                latitude_deg: finite(self.site_latitude_deg()),
                longitude_deg: finite(self.site_longitude_deg()),
                elevation_m: finite(self.site_altitude_m()),
            };
            volume.metadata.archive_version = Some("DORADE".to_owned());
            volume.metadata.compression = Some(
                if self.compression == 1 {
                    "dorade-hrd-rle"
                } else {
                    "dorade-uncompressed"
                }
                .to_owned(),
            );
            volume.metadata.scan_mode = Some(scan_mode_from_radd(self.scan_mode));
        } else if volume.site.id != self.instrument {
            return Err(invalid(
                0,
                format!(
                    "DORADE sweep instrument '{}' does not match volume '{}'",
                    self.instrument, volume.site.id
                ),
            ));
        }
        let sweep_start = self.start_time;
        if let Some(start) = sweep_start
            && (volume.cuts.is_empty() || start < volume.volume_time)
        {
            volume.volume_time = start;
        }

        let gate_range = self.gate_range()?;
        let nyquist = self.nyquist_velocity_mps();
        let fixed_angle = if self.fixed_angle_deg.is_finite() {
            self.fixed_angle_deg
        } else {
            let sum: f32 = self.rays.iter().map(|(ray, _)| ray.elevation_deg).sum();
            sum / self.rays.len() as f32
        };

        // Map params to canonical moments; first match per type wins, later
        // duplicates (e.g. DOW corrected fields DCZ/VC next to DZ/VE) keep
        // their DORADE name as MomentType::Unknown so nothing is dropped.
        let mut taken: BTreeSet<MomentType> = BTreeSet::new();
        for param in &mut self.params {
            let canonical = canonical_moment(&param.name);
            param.moment = match canonical {
                Some(moment) if !taken.contains(&moment) => {
                    taken.insert(moment.clone());
                    moment
                }
                _ => MomentType::Unknown(param.name.clone()),
            };
            param.grid = Some(new_grid(param, gate_range.clone()));
        }

        let elevation_number = u8::try_from(self.sweep_number.clamp(0, 255)).ok();
        let cut = volume.push_cut(fixed_angle, elevation_number);
        cut.radials.reserve(self.rays.len());
        let rays = std::mem::take(&mut self.rays);
        for (ray, rows) in rays {
            let radial_index = cut.radials.len();
            let time_offset_ms = match (ray.time, sweep_start) {
                (Some(time), Some(start)) => (time - start)
                    .num_milliseconds()
                    .clamp(i64::from(i32::MIN), i64::from(i32::MAX))
                    as i32,
                _ => 0,
            };
            cut.radials.push(Radial {
                azimuth_deg: normalize_azimuth(ray.azimuth_deg),
                elevation_deg: ray.elevation_deg,
                time_offset_ms,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: nyquist,
                radial_status: None,
            });
            for (param_index, row) in rows {
                let param = &mut self.params[param_index];
                if let Some(grid) = param.grid.as_mut() {
                    grid.push_row(radial_index, row)?;
                }
            }
        }
        for param in &mut self.params {
            if let Some(grid) = param.grid.take()
                && grid.radial_count() > 0
            {
                cut.moments.insert(grid.moment.clone(), grid);
            }
        }

        volume.metadata.message_count += 1;
        volume.metadata.skipped_message_count +=
            skipped_transition_rays + self.skipped_field_blocks;
        Ok(())
    }
}

fn new_grid(param: &ParamState, gate_range: GateRange) -> MomentGrid {
    match param.binary_format {
        1 => MomentGrid::new_u8(
            param.moment.clone(),
            gate_range,
            param.scale,
            param.bias + 128.0,
            i32_to_u8_sentinel(param.bad_data),
            None,
        ),
        2 => MomentGrid::new_u16(
            param.moment.clone(),
            gate_range,
            param.scale,
            param.bias + 32768.0,
            i32_to_u16_sentinel(param.bad_data),
            None,
        ),
        _ => MomentGrid {
            moment: param.moment.clone(),
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: Vec::new(),
            storage: radar_core::MomentStorage::F32(Vec::new()),
        },
    }
}

fn i32_to_u8_sentinel(bad_data: i32) -> Option<u8> {
    u8::try_from(bad_data + 128).ok()
}

fn i32_to_u16_sentinel(bad_data: i32) -> Option<u16> {
    u16::try_from(i64::from(bad_data) + 32768).ok()
}

/// Map a DORADE parameter name onto the canonical moment set.
///
/// Names come from three generations of writers: solo-era two-letter codes
/// (DZ/VE/SW), RaXPol long names (DBZ/VEL/WIDTH/RHOHV), and Radx `_F`
/// (filtered) / polarization suffixed names (DBZHC_F/VEL_F/ZDR_F/RHOHV_F).
/// Suffixes are stripped iteratively until a stem matches or no suffix
/// remains, so `DBZHC_F` → `DBZHC` → `DBZ`. CfRadial field names follow the
/// same lineage (Radx writes both), so the CfRadial decoder shares this map.
pub(crate) fn canonical_moment(name: &str) -> Option<MomentType> {
    let normalized = name.trim().to_ascii_uppercase();
    let mut stem = normalized.as_str();
    loop {
        if let Some(moment) = match_moment_stem(stem) {
            return Some(moment);
        }
        let next = ["_F", "_HC", "_VC", "HC", "_V", "_H"]
            .iter()
            .find_map(|suffix| stem.strip_suffix(suffix).filter(|rest| !rest.is_empty()));
        match next {
            Some(shorter) => stem = shorter,
            None => return None,
        }
    }
}

fn match_moment_stem(stem: &str) -> Option<MomentType> {
    match stem {
        "DBZ" | "DZ" | "DBZH" | "DBZV" | "REF" | "CZ" | "UZ" => Some(MomentType::Reflectivity),
        "VR" | "VE" | "VEL" | "VU" | "VG" | "VT" => Some(MomentType::Velocity),
        "SW" | "WIDTH" | "SPW" | "SPECTRUM_WIDTH" => Some(MomentType::SpectrumWidth),
        "ZDR" | "ZD" | "UZDR" => Some(MomentType::DifferentialReflectivity),
        "RHOHV" | "RHO" | "RH" | "ROHV" => Some(MomentType::CorrelationCoefficient),
        "PHIDP" | "PHI" | "PH" | "UPHIDP" => Some(MomentType::DifferentialPhase),
        "KDP" | "KD" => Some(MomentType::SpecificDifferentialPhase),
        _ => None,
    }
}

/// Map the DORADE RADD `scan_mode` code onto the shared scan-mode enum.
///
/// Code values per the DORADE format document (R. Oye and M. Case, "DORADE
/// Data Format", NCAR/ATD 1995; revised by W.-C. Lee, NCAR/EOL) and the
/// authoritative lrose-core `DoradeData.hh` enum: 0 = CAL (calibration),
/// 1 = PPI (sector), 2 = COP (coplane), 3 = RHI, 4 = VER (vertical
/// pointing), 5 = TAR (target/stationary), 6 = MAN (manual), 7 = IDL (idle),
/// 8 = SUR (360° surveillance), 9 = AIR (airborne), 10 = HOR (horizontal).
fn scan_mode_from_radd(code: i16) -> ScanMode {
    match code {
        1 | 8 => ScanMode::Ppi,
        3 => ScanMode::Rhi,
        4 => ScanMode::VerticalPointing,
        _ => ScanMode::Other,
    }
}

/// Decompress one HRD run-length-encoded 16-bit field row.
///
/// Marker word semantics (DORADE document, "compression scheme" appendix):
/// high bit set → `count` data words follow verbatim; high bit clear →
/// `count` gates of missing data; a bare `1` terminates the row.
fn decode_hrd_rle(words: &[i16], gates: usize, bad_data: i16) -> Vec<i16> {
    let mut out = vec![bad_data; gates];
    let mut input = 0usize;
    let mut output = 0usize;
    while input < words.len() && output < gates {
        let marker = words[input] as u16;
        input += 1;
        let count = (marker & 0x7fff) as usize;
        if marker == 1 {
            // End-of-row sentinel.
            break;
        }
        if count == 0 {
            continue;
        }
        if marker & 0x8000 != 0 {
            let take = count
                .min(gates - output)
                .min(words.len().saturating_sub(input));
            out[output..output + take].copy_from_slice(&words[input..input + take]);
            input += count.min(words.len().saturating_sub(input));
            output += take;
        } else {
            output += count.min(gates - output);
        }
    }
    out
}

fn normalize_azimuth(azimuth_deg: f32) -> f32 {
    let normalized = azimuth_deg.rem_euclid(360.0);
    if normalized.is_finite() {
        normalized
    } else {
        0.0
    }
}

fn valid_dorade_f32(value: f32) -> Option<f32> {
    (value.is_finite() && value > DORADE_BAD_F32 && value != 0.0).then_some(value)
}

fn finite(value: f32) -> Option<f32> {
    value.is_finite().then_some(value)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_matches(char::from(0))
        .trim()
        .to_owned()
}

fn require(block: &[u8], needed: usize, offset: usize, what: &'static str) -> Result<()> {
    if block.len() < needed {
        return Err(NexradError::Truncated {
            what,
            offset,
            needed,
            available: block.len(),
        });
    }
    Ok(())
}

fn invalid(offset: usize, reason: impl Into<String>) -> NexradError {
    NexradError::InvalidMessage {
        offset,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::MomentStorage;

    fn put_i16(block: &mut [u8], offset: usize, value: i16, endian: Endian) {
        let bytes = match endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        block[offset..offset + 2].copy_from_slice(&bytes);
    }

    fn put_i32(block: &mut [u8], offset: usize, value: i32, endian: Endian) {
        let bytes = match endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        block[offset..offset + 4].copy_from_slice(&bytes);
    }

    fn put_f32(block: &mut [u8], offset: usize, value: f32, endian: Endian) {
        put_i32(block, offset, value.to_bits() as i32, endian);
    }

    fn base_block(id: &[u8; 4], len: usize, endian: Endian) -> Vec<u8> {
        let mut block = vec![0u8; len];
        block[..4].copy_from_slice(id);
        put_i32(&mut block, 4, len as i32, endian);
        block
    }

    struct Synth {
        endian: Endian,
        compressed: bool,
    }

    impl Synth {
        fn build(&self, rays: &[(f32, f32, i32, &[i16])]) -> Vec<u8> {
            let endian = self.endian;
            let mut bytes = Vec::new();

            let mut sswb = base_block(b"SSWB", 200, endian);
            put_i32(&mut sswb, 12, 1_779_404_114, endian); // 2026-05-21T22:55:14Z
            put_i32(&mut sswb, 16, 1_779_404_126, endian);
            bytes.extend(sswb);

            let mut vold = base_block(b"VOLD", 72, endian);
            put_i16(&mut vold, 10, 42, endian); // volume number
            put_i16(&mut vold, 36, 2026, endian);
            put_i16(&mut vold, 38, 5, endian);
            put_i16(&mut vold, 40, 21, endian);
            put_i16(&mut vold, 42, 22, endian);
            put_i16(&mut vold, 44, 55, endian);
            put_i16(&mut vold, 46, 14, endian);
            bytes.extend(vold);

            let mut radd = base_block(b"RADD", 144, endian);
            radd[8..12].copy_from_slice(b"TST1");
            put_i16(&mut radd, 50, 8, endian); // scan mode SUR
            put_i16(&mut radd, 68, if self.compressed { 1 } else { 0 }, endian);
            put_f32(&mut radd, 80, -103.2927, endian); // lon
            put_f32(&mut radd, 84, 39.74, endian); // lat
            put_f32(&mut radd, 88, 1.519, endian); // alt km
            put_f32(&mut radd, 92, 68.76, endian); // eff unamb vel
            put_i16(&mut radd, 102, 2, endian); // num ipps
            put_f32(&mut radd, 104, 5.45, endian); // freq GHz
            put_f32(&mut radd, 124, 0.4, endian); // prt1 ms
            put_f32(&mut radd, 128, 0.6, endian); // prt2 ms
            bytes.extend(radd);

            let mut parm = base_block(b"PARM", 216, endian);
            parm[8..11].copy_from_slice(b"DBZ");
            put_i16(&mut parm, 78, 2, endian); // 16-bit
            put_f32(&mut parm, 92, 100.0, endian); // scale
            put_f32(&mut parm, 96, 0.0, endian); // bias
            put_i32(&mut parm, 100, -32768, endian); // bad
            put_i32(&mut parm, 200, 4, endian); // cells
            put_f32(&mut parm, 204, 50.0, endian);
            put_f32(&mut parm, 208, 100.0, endian);
            bytes.extend(parm);

            let mut csfd = base_block(b"CSFD", 64, endian);
            put_i32(&mut csfd, 8, 1, endian); // one segment
            put_f32(&mut csfd, 12, 50.0, endian); // first cell m
            put_f32(&mut csfd, 16, 100.0, endian); // spacing m
            put_i16(&mut csfd, 48, 4, endian); // cells
            bytes.extend(csfd);

            let mut swib = base_block(b"SWIB", 40, endian);
            put_i32(&mut swib, 16, 6, endian); // sweep number
            put_i32(&mut swib, 20, rays.len() as i32, endian);
            put_f32(&mut swib, 32, 1.0, endian); // fixed angle
            bytes.extend(swib);

            for (azimuth, elevation, status, gates) in rays {
                let mut ryib = base_block(b"RYIB", 44, self.endian);
                put_i32(&mut ryib, 8, 6, endian); // sweep number
                put_i32(&mut ryib, 12, 141, endian); // julian day (May 21)
                put_i16(&mut ryib, 16, 22, endian);
                put_i16(&mut ryib, 18, 55, endian);
                put_i16(&mut ryib, 20, 15, endian);
                put_i16(&mut ryib, 22, 250, endian);
                put_f32(&mut ryib, 24, *azimuth, endian);
                put_f32(&mut ryib, 28, *elevation, endian);
                put_i32(&mut ryib, 40, *status, endian);
                bytes.extend(ryib);

                let words: Vec<i16> = if self.compressed {
                    let mut encoded = Vec::new();
                    encoded.push((0x8000u16 | gates.len() as u16) as i16);
                    encoded.extend_from_slice(gates);
                    encoded.push(1); // end sentinel
                    encoded
                } else {
                    gates.to_vec()
                };
                let mut rdat = base_block(b"RDAT", 16 + words.len() * 2, endian);
                rdat[8..11].copy_from_slice(b"DBZ");
                for (index, word) in words.iter().enumerate() {
                    put_i16(&mut rdat, 16 + index * 2, *word, endian);
                }
                bytes.extend(rdat);
            }
            bytes
        }
    }

    fn synth_rays() -> Vec<(f32, f32, i32, &'static [i16])> {
        vec![
            (45.0, 1.0, 0, &[1000, 2000, -32768, 500][..]),
            (46.0, 1.0, 0, &[1500, -32768, 700, 800][..]),
            (47.0, 9.5, 1, &[1, 2, 3, 4][..]), // transition ray
        ]
    }

    #[test]
    fn decodes_big_endian_synthetic_sweep() {
        let bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&synth_rays());
        assert!(looks_like_dorade_bytes(&bytes));

        let volume = decode_dorade_sweep_volume(&bytes).expect("decode");
        assert_eq!(volume.site.id, "TST1");
        // RADD scan mode 8 (SUR) maps to the shared PPI mode.
        assert_eq!(volume.metadata.scan_mode, Some(ScanMode::Ppi));
        assert_eq!(volume.site.latitude_deg, Some(39.74));
        assert_eq!(volume.site.longitude_deg, Some(-103.2927));
        assert!((volume.site.elevation_m.unwrap() - 1519.0).abs() < 0.5);
        assert_eq!(
            volume.volume_time,
            Utc.with_ymd_and_hms(2026, 5, 21, 22, 55, 14).unwrap()
        );
        assert_eq!(volume.cuts.len(), 1);

        let cut = &volume.cuts[0];
        assert_eq!(cut.elevation_deg, 1.0);
        // Transition ray dropped.
        assert_eq!(cut.radials.len(), 2);
        assert_eq!(cut.radials[0].azimuth_deg, 45.0);
        assert_eq!(cut.radials[0].nyquist_velocity_mps, Some(68.76));
        assert_eq!(cut.radials[0].gate_range.first_gate_m, 50);
        assert_eq!(cut.radials[0].gate_range.gate_spacing_m, 100);
        assert_eq!(cut.radials[0].gate_range.gate_count, 4);
        // RYIB time 22:55:15.250 − SSWB start 22:55:14 = 1250 ms.
        assert_eq!(cut.radials[0].time_offset_ms, 1250);

        let grid = cut.moments.get(&MomentType::Reflectivity).expect("DBZ");
        assert_eq!(grid.radial_count(), 2);
        assert_eq!(grid.scaled_value(0, 0), Some(10.0));
        assert_eq!(grid.scaled_value(0, 1), Some(20.0));
        assert_eq!(grid.scaled_value(0, 2), None); // bad gate
        assert_eq!(grid.scaled_value(1, 2), Some(7.0));
    }

    #[test]
    fn decodes_little_endian_rle_sweep() {
        let bytes = Synth {
            endian: Endian::Little,
            compressed: true,
        }
        .build(&synth_rays());
        assert!(looks_like_dorade_bytes(&bytes));

        let volume = decode_dorade_sweep_volume(&bytes).expect("decode");
        let cut = &volume.cuts[0];
        let grid = cut.moments.get(&MomentType::Reflectivity).expect("DBZ");
        assert_eq!(grid.scaled_value(0, 0), Some(10.0));
        assert_eq!(grid.scaled_value(0, 3), Some(5.0));
        assert_eq!(grid.scaled_value(1, 1), None);
    }

    #[test]
    fn rle_run_of_missing_gates_pads_with_bad() {
        // 2 missing gates, then 2 literal words, end sentinel.
        let words = [2i16, (0x8000u16 | 2) as i16, 700, 800, 1];
        let out = decode_hrd_rle(&words, 6, -32768);
        assert_eq!(out, vec![-32768, -32768, 700, 800, -32768, -32768]);
    }

    #[test]
    fn peek_reads_grouping_metadata_without_rays() {
        let bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&synth_rays());
        let header = peek_dorade_sweep(&bytes).expect("peek");
        assert_eq!(header.instrument, "TST1");
        assert_eq!(header.volume_number, 42);
        assert_eq!(header.sweep_number, 6);
        assert_eq!(header.fixed_angle_deg, 1.0);
        assert_eq!(
            header.start_time,
            Some(Utc.with_ymd_and_hms(2026, 5, 21, 22, 55, 14).unwrap())
        );
        assert!((header.latitude_deg - 39.74).abs() < 1e-5);
    }

    #[test]
    fn multi_sweep_volume_sorts_cuts_by_elevation() {
        let synth = Synth {
            endian: Endian::Big,
            compressed: false,
        };
        let high = {
            let mut rays = synth_rays();
            for ray in &mut rays {
                ray.1 = 2.4;
            }
            let mut bytes = synth.build(&rays);
            // Rewrite SWIB fixed angle (offset of SWIB block in the byte
            // stream is stable for the synth builder).
            let swib_pos = find_block(&bytes, b"SWIB");
            put_f32(&mut bytes[swib_pos..], 32, 2.4, Endian::Big);
            bytes
        };
        let low = synth.build(&synth_rays());
        let volume = decode_dorade_volume_from_slices(&[high, low]).expect("decode");
        assert_eq!(volume.cuts.len(), 2);
        assert!(volume.cuts[0].elevation_deg < volume.cuts[1].elevation_deg);
        assert_eq!(volume.metadata.decoded_radial_count, 4);
    }

    #[test]
    fn mismatched_instruments_are_rejected() {
        let synth = Synth {
            endian: Endian::Big,
            compressed: false,
        };
        let first = synth.build(&synth_rays());
        let mut second = synth.build(&synth_rays());
        let radd_pos = find_block(&second, b"RADD");
        second[radd_pos + 8..radd_pos + 12].copy_from_slice(b"TST2");
        let err = decode_dorade_volume_from_slices(&[first, second]).unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[test]
    fn rhi_scan_mode_is_detected_from_radd() {
        // An RHI sweep: RADD scan mode 3, fixed azimuth, elevation-swept rays.
        let rays: Vec<(f32, f32, i32, &[i16])> = vec![
            (271.0, 0.5, 0, &[1000, 2000, 1500, 500][..]),
            (271.0, 1.5, 0, &[1500, 1200, 700, 800][..]),
            (271.0, 2.5, 0, &[900, 1100, 600, 400][..]),
        ];
        let mut bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&rays);
        let radd_pos = find_block(&bytes, b"RADD");
        put_i16(&mut bytes[radd_pos..], 50, 3, Endian::Big); // RHI per DORADE doc
        let volume = decode_dorade_sweep_volume(&bytes).expect("decode");
        assert_eq!(volume.metadata.scan_mode, Some(ScanMode::Rhi));
        // Per-radial elevations carry the sweep; azimuth is fixed.
        let cut = &volume.cuts[0];
        assert_eq!(cut.radials.len(), 3);
        assert!(cut.radials.iter().all(|r| r.azimuth_deg == 271.0));
        assert_eq!(cut.radials[0].elevation_deg, 0.5);
        assert_eq!(cut.radials[2].elevation_deg, 2.5);
    }

    #[test]
    fn radd_scan_mode_codes_map_to_shared_enum() {
        // Codes per Oye & Case 1995 / lrose DoradeData.hh.
        assert_eq!(scan_mode_from_radd(1), ScanMode::Ppi); // PPI sector
        assert_eq!(scan_mode_from_radd(8), ScanMode::Ppi); // SUR
        assert_eq!(scan_mode_from_radd(3), ScanMode::Rhi);
        assert_eq!(scan_mode_from_radd(4), ScanMode::VerticalPointing);
        for other in [0i16, 2, 5, 6, 7, 9, 10, 99] {
            assert_eq!(scan_mode_from_radd(other), ScanMode::Other);
        }
    }

    #[test]
    fn canonical_moment_maps_observed_corpus_names() {
        // COW2 (Radx _F names), RaXPol, DOW7 solo-era names.
        assert_eq!(canonical_moment("DBZHC_F"), Some(MomentType::Reflectivity));
        assert_eq!(canonical_moment("VEL_F"), Some(MomentType::Velocity));
        assert_eq!(
            canonical_moment("ZDR_F"),
            Some(MomentType::DifferentialReflectivity)
        );
        assert_eq!(
            canonical_moment("RHOHV_F"),
            Some(MomentType::CorrelationCoefficient)
        );
        assert_eq!(canonical_moment("DBZ"), Some(MomentType::Reflectivity));
        assert_eq!(canonical_moment("WIDTH"), Some(MomentType::SpectrumWidth));
        assert_eq!(canonical_moment("DZ"), Some(MomentType::Reflectivity));
        assert_eq!(canonical_moment("VE"), Some(MomentType::Velocity));
        assert_eq!(canonical_moment("SW"), Some(MomentType::SpectrumWidth));
        assert_eq!(canonical_moment("NCP"), None);
        assert_eq!(canonical_moment("DM"), None);
    }

    #[test]
    fn duplicate_canonical_names_keep_original_field() {
        // DOW7 carries DZ (raw) and DCZ/VC (corrected); first match wins and
        // later candidates stay addressable under their DORADE names.
        let mut taken = BTreeSet::new();
        let mut resolved = Vec::new();
        for name in ["DZ", "DCZ", "VE", "VC"] {
            let canonical = canonical_moment(name);
            let moment = match canonical {
                Some(moment) if !taken.contains(&moment) => {
                    taken.insert(moment.clone());
                    moment
                }
                _ => MomentType::Unknown(name.to_owned()),
            };
            resolved.push(moment);
        }
        assert_eq!(resolved[0], MomentType::Reflectivity);
        assert_eq!(resolved[1], MomentType::Unknown("DCZ".to_owned()));
        assert_eq!(resolved[2], MomentType::Velocity);
        assert_eq!(resolved[3], MomentType::Unknown("VC".to_owned()));
    }

    #[test]
    fn u16_grids_preserve_dorade_scaling() {
        let bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&synth_rays());
        let volume = decode_dorade_sweep_volume(&bytes).expect("decode");
        let grid = volume.cuts[0]
            .moments
            .get(&MomentType::Reflectivity)
            .unwrap();
        assert!(matches!(grid.storage, MomentStorage::U16(_)));
        assert_eq!(grid.scale, 100.0);
        assert_eq!(grid.offset, 32768.0);
        assert_eq!(grid.nodata, Some(0));
    }

    fn find_block(bytes: &[u8], id: &[u8; 4]) -> usize {
        bytes
            .windows(4)
            .position(|window| window == id)
            .expect("block present")
    }
}
