//! Export a [`radar_core::RadarVolume`] as a CfRadial-1 file so simulated
//! radar built from WRF (see [`crate::wrf_radar`]) can be SHARED as ordinary
//! radar files.
//!
//! Container: classic NetCDF-3 with 64-bit offsets (CDF-2), written through
//! rw-store's dependency-free [`rw_store::netcdf3::Nc3Writer`] (the same
//! writer behind the model-hour export). Layout follows CfRadial 1.x
//! (M. Dixon and W.-C. Lee, "CfRadial Data File Format", NCAR/EOL, v1.4,
//! 2016): dimensions `time`/`range`/`sweep`, per-ray `azimuth`/`elevation`/
//! `time`/`nyquist_velocity`, per-sweep `fixed_angle`/`sweep_number`/
//! `sweep_start_ray_index`/`sweep_end_ray_index`, scalar `latitude`/
//! `longitude`/`altitude`, and one `(time, range)` float variable per
//! moment, `_FillValue`-masked with a finite sentinel (py-ART and friends
//! mishandle NaN fills).
//!
//! The acceptance oracle is OUR OWN decoder: everything written here is
//! shaped so `nexrad_io::cfradial::decode_cfradial1_volume` reproduces the
//! source volume — gate geometry EXACTLY ([`GateRange`] integers round-trip
//! through the gate-center `range` coordinate), moment values bit-identical
//! f32 where finite, NaN where the source had gaps or a shorter ray was
//! padded. The round-trip tests below enforce that contract.
//!
//! Writer constraints inherited from `Nc3Writer` (a PINNED dep — solved
//! app-side, never patched): every variable is `NC_FLOAT`, so the CfRadial
//! `sweep_mode(sweep, string_length)` and `prt_mode(sweep, string_length)`
//! char matrices are OMITTED rather than being misrepresented as floats. Our
//! reader tolerates their absence (scan mode defaults to unknown); strict
//! consumers may require them. Numeric `prt(time)` is still written when it
//! is known.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use radar_core::{ElevationCut, GateRange, MomentType, RadarVolume, ScanLegMetadata};
use rw_store::netcdf3::{Nc3Attr, Nc3Dim, Nc3VarDef, Nc3Writer};

/// Finite no-data sentinel written to field variables and flagged via the
/// `_FillValue` attribute. The CfRadial reader masks it back to NaN, which is
/// exactly what the in-app F32 moment convention expects.
pub(crate) const CFRADIAL_FILL: f32 = -9999.0;

/// CfRadial field naming for the canonical moments: `(var name, units,
/// long_name)`. Arbitrary `Unknown` moments are not exported; the explicitly
/// stable simulated-radar attenuation/correction ids are whitelisted.
fn moment_field_spec(moment: &MomentType) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match moment {
        MomentType::Reflectivity => ("REF", "dBZ", "equivalent reflectivity factor"),
        MomentType::Velocity => (
            "VEL",
            "m/s",
            "radial velocity of scatterers away from instrument",
        ),
        MomentType::SpectrumWidth => ("SW", "m/s", "doppler spectrum width"),
        MomentType::DifferentialReflectivity => ("ZDR", "dB", "log differential reflectivity h/v"),
        MomentType::CorrelationCoefficient => ("RHOHV", "unitless", "cross correlation ratio h/v"),
        MomentType::DifferentialPhase => ("PHIDP", "degrees", "differential phase h/v"),
        MomentType::SpecificDifferentialPhase => {
            ("KDP", "degrees/km", "specific differential phase h/v")
        }
        MomentType::Unknown(name) => match name.as_str() {
            "AH" => (
                "AH",
                "dB/km",
                "specific attenuation at horizontal polarization",
            ),
            "PIA" => (
                "PIA",
                "dB",
                "path integrated attenuation at horizontal polarization",
            ),
            "REFC" => (
                "REFC",
                "dBZ",
                "attenuation corrected horizontal reflectivity",
            ),
            "ADP" => ("ADP", "dB/km", "specific differential attenuation"),
            "PIDA" => ("PIDA", "dB", "path integrated differential attenuation"),
            "ZDRC" => (
                "ZDRC",
                "dB",
                "attenuation corrected differential reflectivity",
            ),
            _ => return None,
        },
    })
}

/// Default export file name: `{site}_{YYYYMMDD_HHMMSS}_simwrf.nc` (site id
/// squeezed to filesystem-safe characters).
// Reached from the native-dialog export UI; tests cover all platforms.
#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn export_file_name(volume: &RadarVolume) -> String {
    let site: String = volume
        .site
        .id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    let site = if site.is_empty() {
        "SIMWRF".to_owned()
    } else {
        site
    };
    format!(
        "{site}_{}_simwrf.nc",
        volume.volume_time.format("%Y%m%d_%H%M%S")
    )
}

/// Export every loop frame into `dir`, one CfRadial file per frame named by
/// [`export_file_name`] (duplicate names — same site + valid time — get a
/// `_2`, `_3`, … suffix). Returns how many files were written; the first
/// failure aborts the run with files 0..k already on disk.
// Reached from the native-dialog export UI; tests cover all platforms.
#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn export_volumes_cfradial(
    volumes: &[Arc<RadarVolume>],
    dir: &Path,
) -> Result<usize, String> {
    let mut used: BTreeSet<String> = BTreeSet::new();
    for volume in volumes {
        let base = export_file_name(volume);
        let stem = base.strip_suffix(".nc").unwrap_or(&base).to_owned();
        let mut name = base.clone();
        let mut suffix = 2usize;
        while used.contains(&name) {
            name = format!("{stem}_{suffix}.nc");
            suffix += 1;
        }
        used.insert(name.clone());
        export_volume_cfradial(volume, &dir.join(&name))?;
    }
    Ok(volumes.len())
}

/// Write `volume` to `path` as CfRadial-1 over classic NetCDF-3 (CDF-2).
///
/// Requirements on the volume (all satisfied by the synthetic WRF radar):
/// - at least one cut with radials, and at least 2 gates on some ray
///   (CfRadial's `range` coordinate needs two points to define spacing);
/// - ONE gate geometry: every radial and moment grid must agree on
///   `first_gate_m`/`gate_spacing_m` (gate COUNTS may differ per sweep —
///   shorter rays are padded with the fill sentinel).
// Reached from the native-dialog export UI; tests cover all platforms.
#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn export_volume_cfradial(volume: &RadarVolume, path: &Path) -> Result<(), String> {
    let sweep_indices: Vec<usize> = volume
        .cuts
        .iter()
        .enumerate()
        .filter_map(|(index, cut)| (!cut.radials.is_empty()).then_some(index))
        .collect();
    let sweeps: Vec<&ElevationCut> = volume
        .cuts
        .iter()
        .filter(|cut| !cut.radials.is_empty())
        .collect();
    if sweeps.is_empty() {
        return Err("volume has no radials to export".to_owned());
    }

    // Single gate geometry for the whole file: CfRadial 1.x carries ONE
    // range coordinate, so mixed spacings/starts cannot be represented.
    let geometry = sweeps[0].radials[0].gate_range.clone();
    let mut n_gates = 0usize;
    for cut in &sweeps {
        for radial in &cut.radials {
            check_geometry(&geometry, &radial.gate_range, "radial")?;
            n_gates = n_gates.max(radial.gate_range.gate_count);
        }
        for grid in cut.moments.values() {
            check_geometry(&geometry, &grid.gate_range, "moment grid")?;
            n_gates = n_gates.max(grid.gate_range.gate_count);
        }
    }
    if n_gates < 2 {
        return Err("CfRadial needs at least 2 gates per ray".to_owned());
    }

    let n_rays: usize = sweeps.iter().map(|cut| cut.radials.len()).sum();
    let n_sweeps = sweeps.len();

    // Per-ray and per-sweep coordinate arrays, rays concatenated in cut order.
    let mut azimuth = Vec::with_capacity(n_rays);
    let mut elevation = Vec::with_capacity(n_rays);
    let mut ray_seconds = Vec::with_capacity(n_rays);
    let mut nyquist = Vec::with_capacity(n_rays);
    let mut any_nyquist = false;
    let mut sweep_number = Vec::with_capacity(n_sweeps);
    let mut fixed_angle = Vec::with_capacity(n_sweeps);
    let mut sweep_start = Vec::with_capacity(n_sweeps);
    let mut sweep_end = Vec::with_capacity(n_sweeps);
    let mut ray_base = 0usize;
    for (sweep_index, cut) in sweeps.iter().enumerate() {
        sweep_number.push(sweep_index as f32);
        fixed_angle.push(cut.elevation_deg);
        sweep_start.push(ray_base as f32);
        sweep_end.push((ray_base + cut.radials.len() - 1) as f32);
        for radial in &cut.radials {
            azimuth.push(radial.azimuth_deg.rem_euclid(360.0));
            elevation.push(radial.elevation_deg);
            ray_seconds.push(radial.time_offset_ms as f32 / 1000.0);
            match radial.nyquist_velocity_mps {
                Some(value) if value > 0.0 => {
                    any_nyquist = true;
                    nyquist.push(value);
                }
                // The reader keeps only per-ray values > 0, so the sentinel
                // decodes back to None without any attribute masking.
                _ => nyquist.push(CFRADIAL_FILL),
            }
        }
        ray_base += cut.radials.len();
    }

    // Gate CENTERS (CfRadial spec §5.5). The reader reconstructs
    // spacing = round(range[1]-range[0]) and
    // first_gate = round(range[0] - spacing/2), so centers written as
    // first + (i+0.5)*spacing reproduce the integer GateRange exactly.
    let spacing = geometry.gate_spacing_m as f64;
    let first = geometry.first_gate_m as f64;
    let range: Vec<f32> = (0..n_gates)
        .map(|gate| (first + (gate as f64 + 0.5) * spacing) as f32)
        .collect();

    // Field matrices: (time, range) row-major, fill-padded. Union of the
    // supported moments across sweeps, in MomentType order.
    let moments: BTreeSet<MomentType> = sweeps
        .iter()
        .flat_map(|cut| cut.moments.keys().cloned())
        .filter(|moment| moment_field_spec(moment).is_some())
        .collect();
    if moments.is_empty() {
        return Err("volume has no supported moments (REF/VEL/…) to export".to_owned());
    }
    let mut field_data: Vec<(&'static str, &'static str, &'static str, Vec<f32>)> = Vec::new();
    for moment in &moments {
        let (name, units, long_name) =
            moment_field_spec(moment).expect("union filtered to canonical moments");
        let mut data = vec![CFRADIAL_FILL; n_rays * n_gates];
        let mut ray_base = 0usize;
        for cut in &sweeps {
            if let Some(grid) = cut.moments.get(moment) {
                // Moment rows are keyed to radial indices; missing rows stay
                // fill. `scaled_value` is the shared raw→physical conversion
                // (F32 grids pass through bit-identically).
                let mut row_of_radial = vec![None; cut.radials.len()];
                for (row, &radial_index) in grid.radial_indices.iter().enumerate() {
                    if radial_index < row_of_radial.len() {
                        row_of_radial[radial_index] = Some(row);
                    }
                }
                let row_gates = grid.gate_range.gate_count.min(n_gates);
                for (radial_index, row) in row_of_radial.iter().enumerate() {
                    let Some(row) = row else { continue };
                    let out_base = (ray_base + radial_index) * n_gates;
                    for gate in 0..row_gates {
                        if let Some(value) = grid.scaled_value(*row, gate)
                            && value.is_finite()
                        {
                            data[out_base + gate] = value;
                        }
                    }
                }
            }
            ray_base += cut.radials.len();
        }
        field_data.push((name, units, long_name, data));
    }

    // Global attributes. `time_coverage_start` as a global TEXT attribute is
    // one of the two spellings our reader accepts (the other is a char
    // variable, which Nc3Writer cannot emit).
    let start_iso = volume.volume_time.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let max_offset_ms = sweeps
        .iter()
        .flat_map(|cut| &cut.radials)
        .map(|radial| radial.time_offset_ms.max(0) as i64)
        .max()
        .unwrap_or(0);
    let end_iso = (volume.volume_time + chrono::Duration::milliseconds(max_offset_ms))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let mut gattrs = vec![
        Nc3Attr::text("Conventions", "CF/Radial"),
        Nc3Attr::text("version", "1.3"),
        Nc3Attr::text(
            "title",
            "BowEcho simulated radar volume (WRF forward operator)",
        ),
        Nc3Attr::text(
            "source",
            "simulated-wrf: synthetic NEXRAD-style scan forward-modelled from WRF model output",
        ),
        Nc3Attr::text(
            "history",
            "exported by BowEcho radar_export as CfRadial-1 over classic NetCDF-3 (CDF-2)",
        ),
        Nc3Attr::text(
            "comment",
            "Simulated data, not a real radar measurement. Forward-operator \
             configuration and science provenance are carried in file metadata.",
        ),
        Nc3Attr::text("instrument_name", volume.site.id.clone()),
    ];
    if let Some(vcp) = &volume.vcp {
        gattrs.push(Nc3Attr::floats("vcp_pattern", vec![vcp.pattern as f32]));
    }
    if let Some(name) = volume.site.name.as_deref().filter(|name| !name.is_empty()) {
        gattrs.push(Nc3Attr::text("site_name", name));
    }
    gattrs.push(Nc3Attr::text("time_coverage_start", start_iso.clone()));
    gattrs.push(Nc3Attr::text("time_coverage_end", end_iso));
    for (name, value) in [
        ("scan_name", volume.metadata.scan_name.as_deref()),
        ("scan_id", volume.metadata.scan_id.as_deref()),
        (
            "vcp_source_document",
            volume.metadata.vcp_source_document.as_deref(),
        ),
        (
            "vcp_source_revision",
            volume.metadata.vcp_source_revision.as_deref(),
        ),
        (
            "vcp_source_rda_build",
            volume.metadata.vcp_source_rda_build.as_deref(),
        ),
        (
            "vcp_source_figure",
            volume.metadata.vcp_source_figure.as_deref(),
        ),
        (
            "vcp_pulse_length",
            volume.metadata.vcp_pulse_length.as_deref(),
        ),
        (
            "vcp_adaptations",
            volume.metadata.vcp_adaptations.as_deref(),
        ),
        ("polarization", volume.metadata.polarization.as_deref()),
        ("calibration", volume.metadata.calibration.as_deref()),
        (
            "forward_operator",
            volume.metadata.forward_operator.as_deref(),
        ),
        (
            "forward_operator_config",
            volume.metadata.forward_operator_config.as_deref(),
        ),
        ("source_model", volume.metadata.source_model.as_deref()),
        (
            "microphysics_scheme",
            volume.metadata.microphysics_scheme.as_deref(),
        ),
        (
            "scattering_model",
            volume.metadata.scattering_model.as_deref(),
        ),
    ] {
        if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
            gattrs.push(Nc3Attr::text(name, value));
        }
    }

    // Dimensions: ids are vector positions.
    const TIME: usize = 0;
    const RANGE: usize = 1;
    const SWEEP: usize = 2;
    let mut dims = vec![
        Nc3Dim {
            name: "time".to_owned(),
            len: n_rays,
        },
        Nc3Dim {
            name: "range".to_owned(),
            len: n_gates,
        },
        Nc3Dim {
            name: "sweep".to_owned(),
            len: n_sweeps,
        },
    ];
    let frequency_dim = volume.metadata.radar_frequency_mhz.map(|_| {
        let id = dims.len();
        dims.push(Nc3Dim {
            name: "frequency".to_owned(),
            len: 1,
        });
        id
    });

    // Variable definitions and their payloads, in one shared order —
    // Nc3Writer requires write_var calls to match definition order.
    let mut vars: Vec<Nc3VarDef> = Vec::new();
    let mut payloads: Vec<Vec<f32>> = Vec::new();

    vars.push(Nc3VarDef {
        name: "time".to_owned(),
        dimids: vec![TIME],
        attrs: vec![
            Nc3Attr::text("standard_name", "time"),
            Nc3Attr::text("long_name", "time offset of ray from volume start"),
            // py-ART derives absolute ray times from this units string.
            Nc3Attr::text("units", format!("seconds since {start_iso}")),
        ],
    });
    payloads.push(ray_seconds);

    vars.push(Nc3VarDef {
        name: "range".to_owned(),
        dimids: vec![RANGE],
        attrs: vec![
            Nc3Attr::text("standard_name", "projection_range_coordinate"),
            Nc3Attr::text("long_name", "range to center of gate"),
            Nc3Attr::text("units", "meters"),
            Nc3Attr::text("spacing_is_constant", "true"),
            Nc3Attr::floats(
                "meters_to_center_of_first_gate",
                vec![(first + 0.5 * spacing) as f32],
            ),
            Nc3Attr::floats("meters_between_gates", vec![spacing as f32]),
        ],
    });
    payloads.push(range);

    if let (Some(dim), Some(frequency_mhz)) = (frequency_dim, volume.metadata.radar_frequency_mhz) {
        vars.push(Nc3VarDef {
            name: "frequency".to_owned(),
            dimids: vec![dim],
            attrs: vec![
                Nc3Attr::text("standard_name", "radiation_frequency"),
                Nc3Attr::text("long_name", "transmit frequency"),
                Nc3Attr::text("units", "Hz"),
            ],
        });
        payloads.push(vec![frequency_mhz as f32 * 1.0e6]);
    }

    vars.push(Nc3VarDef {
        name: "azimuth".to_owned(),
        dimids: vec![TIME],
        attrs: vec![
            Nc3Attr::text("standard_name", "ray_azimuth_angle"),
            Nc3Attr::text("long_name", "azimuth angle from true north"),
            Nc3Attr::text("units", "degrees"),
        ],
    });
    payloads.push(azimuth);

    vars.push(Nc3VarDef {
        name: "elevation".to_owned(),
        dimids: vec![TIME],
        attrs: vec![
            Nc3Attr::text("standard_name", "ray_elevation_angle"),
            Nc3Attr::text("long_name", "elevation angle from horizontal plane"),
            Nc3Attr::text("units", "degrees"),
        ],
    });
    payloads.push(elevation);

    if any_nyquist {
        vars.push(Nc3VarDef {
            name: "nyquist_velocity".to_owned(),
            dimids: vec![TIME],
            attrs: vec![
                Nc3Attr::text("long_name", "unambiguous doppler velocity"),
                Nc3Attr::text("units", "m/s"),
                Nc3Attr::floats("_FillValue", vec![CFRADIAL_FILL]),
            ],
        });
        payloads.push(nyquist);
    }

    for (name, long_name, units, value) in [
        (
            "pulse_width",
            "transmitted pulse width",
            "seconds",
            volume.metadata.pulse_width_us.map(|value| value * 1.0e-6),
        ),
        (
            "prt",
            "pulse repetition time",
            "seconds",
            volume.metadata.prt_s,
        ),
        (
            "unambiguous_range",
            "unambiguous range",
            "meters",
            volume
                .metadata
                .unambiguous_range_km
                .map(|value| value * 1000.0),
        ),
    ] {
        if let Some(value) = value.filter(|value| value.is_finite() && *value > 0.0) {
            vars.push(Nc3VarDef {
                name: name.to_owned(),
                dimids: vec![TIME],
                attrs: vec![
                    Nc3Attr::text("long_name", long_name),
                    Nc3Attr::text("units", units),
                ],
            });
            payloads.push(vec![value; n_rays]);
        }
    }

    vars.push(Nc3VarDef {
        name: "sweep_number".to_owned(),
        dimids: vec![SWEEP],
        attrs: vec![Nc3Attr::text("long_name", "sweep index, 0-based")],
    });
    payloads.push(sweep_number);

    vars.push(Nc3VarDef {
        name: "fixed_angle".to_owned(),
        dimids: vec![SWEEP],
        attrs: vec![
            Nc3Attr::text("long_name", "target angle for sweep"),
            Nc3Attr::text("units", "degrees"),
        ],
    });
    payloads.push(fixed_angle);

    vars.push(Nc3VarDef {
        name: "sweep_start_ray_index".to_owned(),
        dimids: vec![SWEEP],
        attrs: vec![Nc3Attr::text("long_name", "index of first ray in sweep")],
    });
    payloads.push(sweep_start);

    vars.push(Nc3VarDef {
        name: "sweep_end_ray_index".to_owned(),
        dimids: vec![SWEEP],
        attrs: vec![Nc3Attr::text("long_name", "index of last ray in sweep")],
    });
    payloads.push(sweep_end);

    // BowEcho source-qualified VCP provenance. These are custom per-sweep
    // variables because CfRadial has no standard representation for Appendix
    // C waveform/PRF-code rows. Values are codes/counts exactly as published,
    // never converted to Hz or PRT.
    let leg_fields = [
        (
            "vcp_source_row_index",
            "zero-based physical row in the qualified VCP source",
            "1",
            per_sweep_leg_values(volume, &sweep_indices, |leg| {
                leg.source_row_index.map(f32::from)
            }),
        ),
        (
            "vcp_azimuth_rate",
            "source-table antenna azimuth rate",
            "degrees/second",
            per_sweep_leg_values(volume, &sweep_indices, |leg| {
                leg.azimuth_rate_deg_per_second
            }),
        ),
        (
            "vcp_source_period",
            "source-table physical-row period",
            "seconds",
            per_sweep_leg_values(volume, &sweep_indices, |leg| leg.source_period_seconds),
        ),
        (
            "vcp_waveform_code",
            "BowEcho code for the source-table waveform abbreviation",
            "1",
            per_sweep_leg_values(volume, &sweep_indices, |leg| {
                leg.waveform.as_deref().and_then(waveform_code)
            }),
        ),
        (
            "vcp_moment_coverage_code",
            "BowEcho code for physical-row moment coverage",
            "1",
            per_sweep_leg_values(volume, &sweep_indices, |leg| {
                leg.moment_coverage
                    .as_deref()
                    .and_then(moment_coverage_code)
            }),
        ),
        (
            "vcp_surveillance_prf_code",
            "source-table surveillance PRF code (not Hz)",
            "1",
            per_sweep_leg_values(volume, &sweep_indices, |leg| {
                leg.surveillance_prf_code.map(f32::from)
            }),
        ),
        (
            "vcp_surveillance_pulse_count",
            "source-table surveillance pulse count",
            "count",
            per_sweep_leg_values(volume, &sweep_indices, |leg| {
                leg.surveillance_pulse_count.map(f32::from)
            }),
        ),
        (
            "vcp_doppler_prf_code",
            "source-table default Doppler PRF code (not Hz)",
            "1",
            per_sweep_leg_values(volume, &sweep_indices, |leg| {
                leg.doppler_prf_code.map(f32::from)
            }),
        ),
        (
            "vcp_doppler_pulse_count",
            "source-table default Doppler pulse count",
            "count",
            per_sweep_leg_values(volume, &sweep_indices, |leg| {
                leg.doppler_pulse_count.map(f32::from)
            }),
        ),
    ];
    for (name, long_name, units, values) in leg_fields {
        let Some(values) = values else { continue };
        let mut attrs = vec![
            Nc3Attr::text("long_name", long_name),
            Nc3Attr::text("units", units),
            Nc3Attr::floats("_FillValue", vec![CFRADIAL_FILL]),
        ];
        if name == "vcp_waveform_code" {
            attrs.push(Nc3Attr::floats(
                "flag_values",
                vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
            ));
            attrs.push(Nc3Attr::text("flag_meanings", "CS CD/W B CD/WO SZCS SZCD"));
        } else if name == "vcp_moment_coverage_code" {
            attrs.push(Nc3Attr::floats("flag_values", vec![1.0, 2.0, 3.0]));
            attrs.push(Nc3Attr::text("flag_meanings", "surveillance doppler all"));
        }
        vars.push(Nc3VarDef {
            name: name.to_owned(),
            dimids: vec![SWEEP],
            attrs,
        });
        payloads.push(values);
    }

    // Site scalars (0-dimensional variables), written only when known.
    let scalars = [
        ("latitude", "degrees_north", volume.site.latitude_deg),
        ("longitude", "degrees_east", volume.site.longitude_deg),
        ("altitude", "meters", volume.site.elevation_m),
    ];
    for (name, units, value) in scalars {
        if let Some(value) = value {
            vars.push(Nc3VarDef {
                name: name.to_owned(),
                dimids: Vec::new(),
                attrs: vec![Nc3Attr::text("units", units)],
            });
            payloads.push(vec![value]);
        }
    }
    for (name, long_name, value) in [
        (
            "radar_beam_width_h",
            "horizontal one-way 3 dB beam width",
            volume.metadata.beam_width_h_deg,
        ),
        (
            "radar_beam_width_v",
            "vertical one-way 3 dB beam width",
            volume.metadata.beam_width_v_deg,
        ),
    ] {
        if let Some(value) = value.filter(|value| value.is_finite() && *value > 0.0) {
            vars.push(Nc3VarDef {
                name: name.to_owned(),
                dimids: Vec::new(),
                attrs: vec![
                    Nc3Attr::text("long_name", long_name),
                    Nc3Attr::text("units", "degrees"),
                ],
            });
            payloads.push(vec![value]);
        }
    }

    for (name, units, long_name, data) in field_data {
        vars.push(Nc3VarDef {
            name: name.to_owned(),
            dimids: vec![TIME, RANGE],
            attrs: vec![
                Nc3Attr::text("long_name", long_name),
                Nc3Attr::text("units", units),
                Nc3Attr::text("coordinates", "elevation azimuth range"),
                Nc3Attr::floats("_FillValue", vec![CFRADIAL_FILL]),
            ],
        });
        payloads.push(data);
    }

    let mut writer = Nc3Writer::create(path, dims, gattrs, vars)
        .map_err(|error| format!("CfRadial export: {error}"))?;
    for payload in &payloads {
        writer
            .write_var(payload)
            .map_err(|error| format!("CfRadial export: {error}"))?;
    }
    writer
        .finish()
        .map_err(|error| format!("CfRadial export: {error}"))
}

fn check_geometry(expected: &GateRange, actual: &GateRange, what: &str) -> Result<(), String> {
    if actual.first_gate_m != expected.first_gate_m
        || actual.gate_spacing_m != expected.gate_spacing_m
    {
        return Err(format!(
            "CfRadial export needs one gate geometry, but a {what} has first_gate_m {} / \
             gate_spacing_m {} where the volume started with {} / {}",
            actual.first_gate_m,
            actual.gate_spacing_m,
            expected.first_gate_m,
            expected.gate_spacing_m
        ));
    }
    Ok(())
}

fn per_sweep_leg_values(
    volume: &RadarVolume,
    sweep_indices: &[usize],
    value: impl Fn(&ScanLegMetadata) -> Option<f32>,
) -> Option<Vec<f32>> {
    let mut any = false;
    let values = sweep_indices
        .iter()
        .map(|&index| {
            volume
                .metadata
                .scan_legs
                .get(index)
                .and_then(&value)
                .filter(|value| value.is_finite())
                .inspect(|_| any = true)
                .unwrap_or(CFRADIAL_FILL)
        })
        .collect();
    any.then_some(values)
}

fn waveform_code(waveform: &str) -> Option<f32> {
    Some(match waveform {
        "CS" => 1.0,
        "CD/W" => 2.0,
        "B" => 3.0,
        "CD/WO" => 4.0,
        "SZCS" => 5.0,
        "SZCD" => 6.0,
        _ => return None,
    })
}

fn moment_coverage_code(coverage: &str) -> Option<f32> {
    Some(match coverage {
        "surveillance" => 1.0,
        "doppler" => 2.0,
        "all" => 3.0,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use radar_core::{MomentGrid, MomentStorage, RadarSite, Radial, ScanLegMetadata, VcpInfo};

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bowecho-radar-export-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn grid(moment: MomentType, gate_range: GateRange, rows: Vec<Vec<f32>>) -> MomentGrid {
        let radial_indices = (0..rows.len()).collect();
        MomentGrid {
            moment,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices,
            storage: MomentStorage::F32(rows.into_iter().flatten().collect()),
        }
    }

    /// Hand-built 3-sweep volume with distinctive REF/VEL values, NaN gaps,
    /// a shorter last sweep (12 of 16 gates), per-ray times, and MIXED
    /// nyquist (present on sweeps 0 and 2, absent on sweep 1).
    fn sample_volume() -> RadarVolume {
        let time = DateTime::<Utc>::from_timestamp(1_779_165_000, 0).unwrap(); // 2026-05-19T04:30:00Z
        let mut site = RadarSite::new("KTST");
        site.name = Some("Test Radar".to_owned());
        site.latitude_deg = Some(39.5);
        site.longitude_deg = Some(-95.25);
        site.elevation_m = Some(350.0);
        let mut volume = RadarVolume::new(site, time);
        volume.metadata.archive_version = Some("simulated-wrf".to_owned());

        let geometry = |gates: usize| GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 250,
            gate_count: gates,
        };
        let sweeps: [(f32, usize, Option<f32>); 3] = [
            (0.5, 16, Some(27.5)),
            (1.5, 16, None),
            (3.1, 12, Some(27.5)),
        ];
        let mut global_ray = 0usize;
        for (sweep_index, (elevation, gates, nyquist)) in sweeps.into_iter().enumerate() {
            let cut = volume.push_cut(elevation, Some(sweep_index as u8 + 1));
            let mut ref_rows = Vec::new();
            let mut vel_rows = Vec::new();
            for az_index in 0..8usize {
                cut.radials.push(Radial {
                    azimuth_deg: az_index as f32 * 45.0,
                    elevation_deg: elevation,
                    time_offset_ms: (global_ray as i32) * 500,
                    gate_range: geometry(gates),
                    nyquist_velocity_mps: nyquist,
                    radial_status: None,
                });
                let mut ref_row = Vec::with_capacity(gates);
                let mut vel_row = Vec::with_capacity(gates);
                for gate in 0..gates {
                    // NaN gaps sprinkled deterministically.
                    if (az_index + gate) % 5 == 0 {
                        ref_row.push(f32::NAN);
                        vel_row.push(f32::NAN);
                    } else {
                        ref_row.push(
                            12.25
                                + sweep_index as f32 * 7.5
                                + az_index as f32 * 0.5
                                + gate as f32 * 0.125,
                        );
                        vel_row.push(
                            -24.5 + sweep_index as f32 + az_index as f32 * 1.25
                                - gate as f32 * 0.0625,
                        );
                    }
                }
                ref_rows.push(ref_row);
                vel_rows.push(vel_row);
                global_ray += 1;
            }
            cut.moments.insert(
                MomentType::Reflectivity,
                grid(MomentType::Reflectivity, geometry(gates), ref_rows),
            );
            cut.moments.insert(
                MomentType::Velocity,
                grid(MomentType::Velocity, geometry(gates), vel_rows),
            );
        }
        volume
    }

    #[test]
    fn round_trip_hand_built_volume_bit_exact() {
        let volume = sample_volume();
        let path = tmp_dir("roundtrip").join(export_file_name(&volume));
        export_volume_cfradial(&volume, &path).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            nexrad_io::sniff_supported_volume_format(&bytes),
            nexrad_io::SupportedVolumeFormat::CfRadial
        );
        let decoded = nexrad_io::decode_supported_volume_bytes(&bytes).unwrap();

        // Site + time round-trip.
        assert_eq!(decoded.site.id, "KTST");
        assert_eq!(decoded.site.name.as_deref(), Some("Test Radar"));
        assert_eq!(decoded.site.latitude_deg, Some(39.5));
        assert_eq!(decoded.site.longitude_deg, Some(-95.25));
        assert_eq!(decoded.site.elevation_m, Some(350.0));
        assert_eq!(decoded.volume_time, volume.volume_time);
        assert_eq!(
            decoded.metadata.compression.as_deref(),
            Some("cfradial1-netcdf3")
        );

        // Sweep structure: elevations, azimuths, per-ray times, nyquist.
        assert_eq!(decoded.cuts.len(), 3);
        let mut global_ray = 0usize;
        for (cut_index, cut) in decoded.cuts.iter().enumerate() {
            let expected = &volume.cuts[cut_index];
            assert_eq!(cut.elevation_deg, expected.elevation_deg);
            assert_eq!(cut.radials.len(), 8);
            for (az_index, radial) in cut.radials.iter().enumerate() {
                assert_eq!(radial.azimuth_deg, az_index as f32 * 45.0);
                assert_eq!(radial.elevation_deg, expected.elevation_deg);
                assert_eq!(radial.time_offset_ms, (global_ray as i32) * 500);
                assert_eq!(
                    radial.nyquist_velocity_mps, expected.radials[az_index].nyquist_velocity_mps,
                    "cut {cut_index} ray {az_index}"
                );
                // Gate geometry EXACT; gate_count padded to the volume max.
                assert_eq!(radial.gate_range.first_gate_m, 1000);
                assert_eq!(radial.gate_range.gate_spacing_m, 250);
                assert_eq!(radial.gate_range.gate_count, 16);
                global_ray += 1;
            }

            // Moment values: bit-identical f32 where finite, NaN where the
            // source had NaN — and NaN in the padding of the short sweep.
            for moment in [MomentType::Reflectivity, MomentType::Velocity] {
                let got = &cut.moments[&moment];
                let MomentStorage::F32(got_values) = &got.storage else {
                    panic!("decoded storage must be F32");
                };
                let source = &expected.moments[&moment];
                let MomentStorage::F32(source_values) = &source.storage else {
                    panic!("source storage must be F32");
                };
                let source_gates = source.gate_range.gate_count;
                assert_eq!(got_values.len(), 8 * 16);
                for ray in 0..8 {
                    for gate in 0..16 {
                        let got_value = got_values[ray * 16 + gate];
                        if gate >= source_gates {
                            assert!(got_value.is_nan(), "padding must decode as NaN");
                            continue;
                        }
                        let source_value = source_values[ray * source_gates + gate];
                        if source_value.is_nan() {
                            assert!(got_value.is_nan(), "NaN gap must survive round-trip");
                        } else {
                            assert_eq!(
                                got_value.to_bits(),
                                source_value.to_bits(),
                                "cut {cut_index} {moment:?} ray {ray} gate {gate}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn round_trip_instrument_metadata_and_dual_pol_attenuation_fields() {
        let mut volume = sample_volume();
        volume.metadata.radar_frequency_mhz = Some(2_800);
        volume.metadata.beam_width_h_deg = Some(0.95);
        volume.metadata.beam_width_v_deg = Some(1.05);
        volume.metadata.pulse_width_us = Some(1.57);
        volume.metadata.prt_s = Some(0.001);
        volume.metadata.unambiguous_range_km = Some(149.896);
        volume.metadata.scan_name = Some("Synthetic VCP 212".to_owned());
        volume.metadata.scan_id = Some("SIM-VCP212-001".to_owned());
        volume.metadata.polarization = Some("simultaneous H/V".to_owned());
        volume.metadata.calibration = Some("ZDR bias +0.15 dB".to_owned());
        volume.metadata.forward_operator = Some("bowecho-wrf-radar".to_owned());
        volume.metadata.forward_operator_config =
            Some(r#"{"beam_integration":"balanced"}"#.to_owned());
        volume.metadata.source_model = Some("WRF-ARW".to_owned());
        volume.metadata.microphysics_scheme = Some("Thompson aerosol-aware".to_owned());
        volume.metadata.scattering_model = Some("S-band LUT v1".to_owned());

        let exported_moments = [
            MomentType::Reflectivity,
            MomentType::Velocity,
            MomentType::SpectrumWidth,
            MomentType::DifferentialReflectivity,
            MomentType::CorrelationCoefficient,
            MomentType::DifferentialPhase,
            MomentType::SpecificDifferentialPhase,
            MomentType::Unknown("AH".to_owned()),
            MomentType::Unknown("PIA".to_owned()),
            MomentType::Unknown("REFC".to_owned()),
            MomentType::Unknown("ADP".to_owned()),
            MomentType::Unknown("PIDA".to_owned()),
            MomentType::Unknown("ZDRC".to_owned()),
        ];
        for cut in &mut volume.cuts {
            let gate_range = cut.radials[0].gate_range.clone();
            for (moment_index, moment) in exported_moments.iter().enumerate() {
                let rows = (0..cut.radials.len())
                    .map(|ray| {
                        (0..gate_range.gate_count)
                            .map(|gate| {
                                moment_index as f32 * 10.0 + ray as f32 + gate as f32 * 0.01
                            })
                            .collect()
                    })
                    .collect();
                cut.moments.insert(
                    moment.clone(),
                    grid(moment.clone(), gate_range.clone(), rows),
                );
            }
        }

        let path = tmp_dir("instrument-metadata").join("instrument.nc");
        export_volume_cfradial(&volume, &path).unwrap();
        let decoded =
            nexrad_io::decode_supported_volume_bytes(&std::fs::read(path).unwrap()).unwrap();

        assert_eq!(decoded.metadata.radar_frequency_mhz, Some(2_800));
        assert_eq!(decoded.metadata.beam_width_h_deg, Some(0.95));
        assert_eq!(decoded.metadata.beam_width_v_deg, Some(1.05));
        assert!((decoded.metadata.pulse_width_us.unwrap() - 1.57).abs() < 1.0e-4);
        assert!((decoded.metadata.prt_s.unwrap() - 0.001).abs() < 1.0e-7);
        assert!((decoded.metadata.unambiguous_range_km.unwrap() - 149.896).abs() < 1.0e-3);
        assert_eq!(
            decoded.metadata.scan_name.as_deref(),
            Some("Synthetic VCP 212")
        );
        assert_eq!(decoded.metadata.scan_id.as_deref(), Some("SIM-VCP212-001"));
        assert_eq!(
            decoded.metadata.polarization.as_deref(),
            Some("simultaneous H/V")
        );
        assert_eq!(
            decoded.metadata.calibration.as_deref(),
            Some("ZDR bias +0.15 dB")
        );
        assert_eq!(
            decoded.metadata.forward_operator.as_deref(),
            Some("bowecho-wrf-radar")
        );
        assert_eq!(
            decoded.metadata.forward_operator_config.as_deref(),
            Some(r#"{"beam_integration":"balanced"}"#)
        );
        assert_eq!(decoded.metadata.source_model.as_deref(), Some("WRF-ARW"));
        assert_eq!(
            decoded.metadata.microphysics_scheme.as_deref(),
            Some("Thompson aerosol-aware")
        );
        assert_eq!(
            decoded.metadata.scattering_model.as_deref(),
            Some("S-band LUT v1")
        );
        for cut in &decoded.cuts {
            for moment in &exported_moments {
                assert!(
                    cut.moments.contains_key(moment),
                    "missing round-tripped {moment:?}"
                );
            }
        }
    }

    #[test]
    fn round_trip_vcp_physical_rows_without_inventing_prt() {
        let mut volume = sample_volume();
        volume.vcp = Some(VcpInfo { pattern: 212 });
        volume.metadata.vcp_source_document = Some("2620002AA".to_owned());
        volume.metadata.vcp_source_revision = Some("AA".to_owned());
        volume.metadata.vcp_source_rda_build = Some("24.0".to_owned());
        volume.metadata.vcp_source_figure = Some("Figure C-4".to_owned());
        volume.metadata.vcp_pulse_length = Some("short".to_owned());
        volume.metadata.vcp_adaptations = Some(
            "Base pattern only: SAILS, MRLE, AVSET, Add-MPDA, and site-specific low-tilt adaptations are absent."
                .to_owned(),
        );
        volume.metadata.prt_s = None;
        volume.metadata.unambiguous_range_km = None;
        volume.metadata.scan_legs = vec![
            ScanLegMetadata {
                source_row_index: Some(0),
                elevation_deg: Some(0.5),
                azimuth_rate_deg_per_second: Some(21.149),
                source_period_seconds: Some(17.02),
                waveform: Some("SZCS".to_owned()),
                moment_coverage: Some("surveillance".to_owned()),
                surveillance_prf_code: Some(1),
                surveillance_pulse_count: Some(15),
                ..ScanLegMetadata::default()
            },
            ScanLegMetadata {
                source_row_index: Some(1),
                elevation_deg: Some(0.5),
                azimuth_rate_deg_per_second: Some(17.108),
                source_period_seconds: Some(21.30),
                waveform: Some("SZCD".to_owned()),
                moment_coverage: Some("doppler".to_owned()),
                doppler_prf_code: Some(6),
                doppler_pulse_count: Some(64),
                ..ScanLegMetadata::default()
            },
            ScanLegMetadata {
                source_row_index: Some(2),
                elevation_deg: Some(3.1),
                azimuth_rate_deg_per_second: Some(28.227),
                source_period_seconds: Some(12.75),
                waveform: Some("B".to_owned()),
                moment_coverage: Some("all".to_owned()),
                surveillance_prf_code: Some(5),
                surveillance_pulse_count: Some(3),
                doppler_prf_code: Some(5),
                doppler_pulse_count: Some(28),
            },
        ];

        // Two distinct physical rotations at one fixed angle. Coverage is
        // deliberately disjoint so the round trip proves duplicate order and
        // split-cut moment ownership survive the union-of-fields export.
        volume.cuts[0].elevation_deg = 0.5;
        volume.cuts[1].elevation_deg = 0.5;
        for radial in &mut volume.cuts[1].radials {
            radial.elevation_deg = 0.5;
        }
        volume.cuts[0].moments.remove(&MomentType::Velocity);
        volume.cuts[1].moments.remove(&MomentType::Reflectivity);

        let expected_legs = volume.metadata.scan_legs.clone();
        let path = tmp_dir("vcp-provenance").join("vcp212.nc");
        export_volume_cfradial(&volume, &path).unwrap();
        let decoded =
            nexrad_io::decode_supported_volume_bytes(&std::fs::read(path).unwrap()).unwrap();

        assert_eq!(decoded.vcp, Some(VcpInfo { pattern: 212 }));
        assert_eq!(
            decoded.metadata.vcp_source_document.as_deref(),
            Some("2620002AA")
        );
        assert_eq!(decoded.metadata.vcp_source_revision.as_deref(), Some("AA"));
        assert_eq!(
            decoded.metadata.vcp_source_rda_build.as_deref(),
            Some("24.0")
        );
        assert_eq!(
            decoded.metadata.vcp_source_figure.as_deref(),
            Some("Figure C-4")
        );
        assert_eq!(decoded.metadata.vcp_pulse_length.as_deref(), Some("short"));
        assert!(
            decoded
                .metadata
                .vcp_adaptations
                .as_deref()
                .unwrap()
                .contains("SAILS")
        );
        assert_eq!(decoded.metadata.prt_s, None);
        assert_eq!(decoded.metadata.unambiguous_range_km, None);
        assert_eq!(decoded.metadata.scan_legs, expected_legs);
        assert_eq!(decoded.cuts[0].elevation_deg, 0.5);
        assert_eq!(decoded.cuts[1].elevation_deg, 0.5);
        assert_eq!(
            decoded.metadata.scan_legs[0].waveform.as_deref(),
            Some("SZCS")
        );
        assert_eq!(
            decoded.metadata.scan_legs[1].waveform.as_deref(),
            Some("SZCD")
        );
        assert!(
            decoded.cuts[0]
                .moments
                .contains_key(&MomentType::Reflectivity)
        );
        assert!(!decoded.cuts[0].moments.contains_key(&MomentType::Velocity));
        assert!(
            !decoded.cuts[1]
                .moments
                .contains_key(&MomentType::Reflectivity)
        );
        assert!(decoded.cuts[1].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn arbitrary_unknown_moments_remain_outside_export_whitelist() {
        assert!(moment_field_spec(&MomentType::Unknown("unsafe/name".to_owned())).is_none());
        for id in ["AH", "PIA", "REFC", "ADP", "PIDA", "ZDRC"] {
            assert!(moment_field_spec(&MomentType::Unknown(id.to_owned())).is_some());
        }
    }

    #[test]
    fn exported_bytes_are_cdf2_and_truncated_file_errors() {
        let volume = sample_volume();
        let path = tmp_dir("magic").join("magic.nc");
        export_volume_cfradial(&volume, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"CDF\x02");
        // A truncated file must error cleanly through the router (it still
        // sniffs as CfRadial from the magic), never panic.
        let truncated = &bytes[..64];
        assert_eq!(
            nexrad_io::sniff_supported_volume_format(truncated),
            nexrad_io::SupportedVolumeFormat::CfRadial
        );
        assert!(nexrad_io::decode_supported_volume_bytes(truncated).is_err());
    }

    #[test]
    fn export_rejects_unusable_volumes() {
        let dir = tmp_dir("reject");
        // No radials at all.
        let empty = RadarVolume::default();
        let error = export_volume_cfradial(&empty, &dir.join("empty.nc")).unwrap_err();
        assert!(error.contains("no radials"), "got: {error}");

        // Single-gate rays cannot define the range coordinate spacing.
        let mut one_gate = sample_volume();
        for cut in &mut one_gate.cuts {
            for radial in &mut cut.radials {
                radial.gate_range.gate_count = 1;
            }
            for grid in cut.moments.values_mut() {
                grid.gate_range.gate_count = 1;
                if let MomentStorage::F32(values) = &mut grid.storage {
                    values.truncate(grid.radial_indices.len());
                }
            }
        }
        let error = export_volume_cfradial(&one_gate, &dir.join("one_gate.nc")).unwrap_err();
        assert!(error.contains("at least 2 gates"), "got: {error}");

        // Mixed gate spacing cannot be represented by one range coordinate.
        let mut mixed = sample_volume();
        mixed.cuts[1].radials[3].gate_range.gate_spacing_m = 500;
        let error = export_volume_cfradial(&mixed, &dir.join("mixed.nc")).unwrap_err();
        assert!(error.contains("one gate geometry"), "got: {error}");
    }

    #[test]
    fn export_file_name_is_site_and_timestamp() {
        let volume = sample_volume();
        assert_eq!(export_file_name(&volume), "KTST_20260519_043000_simwrf.nc");

        let mut odd_site = sample_volume();
        odd_site.site.id = "Sim/WRF: X".to_owned();
        assert_eq!(
            export_file_name(&odd_site),
            "SimWRFX_20260519_043000_simwrf.nc"
        );

        let mut no_site = sample_volume();
        no_site.site.id = "///".to_owned();
        assert_eq!(
            export_file_name(&no_site),
            "SIMWRF_20260519_043000_simwrf.nc"
        );
    }

    #[test]
    fn multi_frame_export_writes_one_file_per_frame() {
        let dir = tmp_dir("frames");
        let first = sample_volume();
        let mut second = sample_volume();
        second.volume_time += chrono::Duration::minutes(15);
        // Third frame REPEATS the first valid time — must get a _2 suffix,
        // not overwrite.
        let third = sample_volume();
        let frames = vec![Arc::new(first), Arc::new(second), Arc::new(third)];

        let written = export_volumes_cfradial(&frames, &dir).unwrap();
        assert_eq!(written, 3);
        for name in [
            "KTST_20260519_043000_simwrf.nc",
            "KTST_20260519_044500_simwrf.nc",
            "KTST_20260519_043000_simwrf_2.nc",
        ] {
            let path = dir.join(name);
            assert!(path.is_file(), "missing {name}");
            let decoded =
                nexrad_io::decode_supported_volume_bytes(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(decoded.site.id, "KTST");
        }
    }
}
