//! Best-effort `namelist.input` reconstruction from one raw WRF output file.
//!
//! A wrfout is not a copy of the namelist that created it. WRF persists a
//! useful subset of effective configuration as global attributes, but omits
//! many time-control, I/O, boundary, nesting, and scheme-specific settings.
//! This module therefore keeps three kinds of information visually distinct:
//! exact stored attributes become annotated assignments, derived hints stay
//! commented, and settings that cannot be recovered are called out explicitly.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use chrono::NaiveDateTime;
use wrf_core::WrfFile;

#[derive(Clone, Copy)]
enum AttributeKind {
    Integer,
    Real,
}

#[derive(Clone, Copy)]
struct FieldSpec {
    attribute: &'static str,
    namelist_key: &'static str,
    kind: AttributeKind,
    domain_scoped: bool,
}

const fn int_field(
    attribute: &'static str,
    namelist_key: &'static str,
    domain_scoped: bool,
) -> FieldSpec {
    FieldSpec {
        attribute,
        namelist_key,
        kind: AttributeKind::Integer,
        domain_scoped,
    }
}

const fn real_field(
    attribute: &'static str,
    namelist_key: &'static str,
    domain_scoped: bool,
) -> FieldSpec {
    FieldSpec {
        attribute,
        namelist_key,
        kind: AttributeKind::Real,
        domain_scoped,
    }
}

const DOMAINS_FIELDS: &[FieldSpec] = &[
    int_field("MAX_DOM", "max_dom", false),
    real_field("DT", "time_step", false),
    int_field("GRID_ID", "grid_id", true),
    int_field("PARENT_ID", "parent_id", true),
    int_field("I_PARENT_START", "i_parent_start", true),
    int_field("J_PARENT_START", "j_parent_start", true),
    int_field("PARENT_GRID_RATIO", "parent_grid_ratio", true),
    int_field("PARENT_TIME_STEP_RATIO", "parent_time_step_ratio", true),
    int_field("WEST-EAST_GRID_DIMENSION", "e_we", true),
    int_field("SOUTH-NORTH_GRID_DIMENSION", "e_sn", true),
    int_field("BOTTOM-TOP_GRID_DIMENSION", "e_vert", true),
    real_field("DX", "dx", true),
    real_field("DY", "dy", true),
    int_field("AUTO_LEVELS_OPT", "auto_levels_opt", false),
    real_field("DZBOT", "dzbot", false),
    real_field("DZSTRETCH_S", "dzstretch_s", false),
    real_field("DZSTRETCH_U", "dzstretch_u", false),
    real_field("ETAC", "etac", false),
    int_field("FEEDBACK", "feedback", false),
    int_field("SMOOTH_OPTION", "smooth_option", false),
];

const PHYSICS_FIELDS: &[FieldSpec] = &[
    int_field("MP_PHYSICS", "mp_physics", true),
    int_field("CU_PHYSICS", "cu_physics", true),
    int_field("RA_LW_PHYSICS", "ra_lw_physics", true),
    int_field("RA_SW_PHYSICS", "ra_sw_physics", true),
    real_field("RADT", "radt", true),
    int_field("BL_PBL_PHYSICS", "bl_pbl_physics", true),
    real_field("BLDT", "bldt", true),
    int_field("SF_SFCLAY_PHYSICS", "sf_sfclay_physics", true),
    int_field("SF_SURFACE_PHYSICS", "sf_surface_physics", true),
    int_field("SF_URBAN_PHYSICS", "sf_urban_physics", true),
    int_field("SF_LAKE_PHYSICS", "sf_lake_physics", true),
    int_field("SF_OCEAN_PHYSICS", "sf_ocean_physics", true),
    real_field("CUDT", "cudt", true),
    int_field("SHCU_PHYSICS", "shcu_physics", true),
    int_field("ISHALLOW", "ishallow", false),
    int_field("ISFFLX", "isfflx", false),
    int_field("ISFTCFLX", "isftcflx", false),
    int_field("ICLOUD", "icloud", false),
    int_field("ICLOUD_CU", "icloud_cu", false),
    int_field("SURFACE_INPUT_SOURCE", "surface_input_source", true),
    int_field("SST_UPDATE", "sst_update", false),
    real_field("PREC_ACC_DT", "prec_acc_dt", true),
    real_field("BUCKET_MM", "bucket_mm", false),
    real_field("BUCKET_J", "bucket_j", false),
    int_field("NUM_LAND_CAT", "num_land_cat", false),
    int_field("SF_SURFACE_MOSAIC", "sf_surface_mosaic", true),
    int_field("AER_OPT", "aer_opt", true),
    int_field("AERCU_OPT", "aercu_opt", true),
    int_field("AER_TYPE", "aer_type", true),
    int_field("AER_AOD550_OPT", "aer_aod550_opt", true),
    real_field("AER_AOD550_VAL", "aer_aod550_val", true),
    int_field("AER_ANGEXP_OPT", "aer_angexp_opt", true),
    real_field("AER_ANGEXP_VAL", "aer_angexp_val", true),
    int_field("AER_SSA_OPT", "aer_ssa_opt", true),
    real_field("AER_SSA_VAL", "aer_ssa_val", true),
    int_field("AER_ASY_OPT", "aer_asy_opt", true),
    real_field("AER_ASY_VAL", "aer_asy_val", true),
    int_field("GHG_INPUT", "ghg_input", true),
    int_field("CLDOVRLP", "cldovrlp", false),
    int_field("MFSHCONV", "mfshconv", false),
    int_field("SWINT_OPT", "swint_opt", false),
    real_field("SWRAD_SCAT", "swrad_scat", false),
    int_field("GWD_OPT", "gwd_opt", true),
    int_field("YSU_TOPDOWN_PBLMIX", "ysu_topdown_pblmix", false),
    int_field("SCALAR_PBLMIX", "scalar_pblmix", false),
    int_field("TRACER_PBLMIX", "tracer_pblmix", false),
    int_field("SLUCM_DISTRIBUTED_DRAG", "slucm_distributed_drag", false),
    int_field("DISTRIBUTED_AHE_OPT", "distributed_ahe_opt", false),
];

const DYNAMICS_FIELDS: &[FieldSpec] = &[
    int_field("HYBRID_OPT", "hybrid_opt", false),
    int_field("HYPSOMETRIC_OPT", "hypsometric_opt", false),
    int_field("USE_THETA_M", "use_theta_m", false),
    int_field("W_DAMPING", "w_damping", false),
    int_field("DIFF_OPT", "diff_opt", true),
    int_field("KM_OPT", "km_opt", true),
    int_field("DAMP_OPT", "damp_opt", true),
    real_field("DAMPCOEF", "dampcoef", true),
    real_field("KHDIF", "khdif", true),
    real_field("KVDIF", "kvdif", true),
    int_field("DIFF_6TH_OPT", "diff_6th_opt", true),
    real_field("DIFF_6TH_FACTOR", "diff_6th_factor", true),
    int_field("DIFF_6TH_SLOPEOPT", "diff_6th_slopeopt", true),
    real_field("DIFF_6TH_THRESH", "diff_6th_thresh", true),
    int_field("MOIST_ADV_OPT", "moist_adv_opt", true),
    int_field("SCALAR_ADV_OPT", "scalar_adv_opt", true),
    int_field("TKE_ADV_OPT", "tke_adv_opt", true),
    int_field("GRAV_SETTLING", "grav_settling", false),
    int_field("USE_Q_DIABATIC", "use_q_diabatic", false),
];

const FDDA_FIELDS: &[FieldSpec] = &[
    int_field("GRID_FDDA", "grid_fdda", true),
    real_field("GFDDA_INTERVAL_M", "gfdda_interval_m", true),
    real_field("GFDDA_END_H", "gfdda_end_h", true),
    int_field("GRID_SFDDA", "grid_sfdda", true),
    real_field("SGFDDA_INTERVAL_M", "sgfdda_interval_m", true),
    real_field("SGFDDA_END_H", "sgfdda_end_h", true),
    int_field("OBS_NUDGE_OPT", "obs_nudge_opt", true),
];

const DFI_FIELDS: &[FieldSpec] = &[int_field("DFI_OPT", "dfi_opt", false)];

const TEXT_METADATA_ATTRIBUTES: &[&str] = &[
    "TITLE",
    "START_DATE",
    "SIMULATION_START_DATE",
    "SIMULATION_INITIALIZATION_TYPE",
    "GRIDTYPE",
    "MAP_PROJ_CHAR",
    "MMINLU",
];

const INTEGER_METADATA_ATTRIBUTES: &[&str] = &[
    "MAP_PROJ",
    "JULYR",
    "JULDAY",
    "NTASKS_TOTAL",
    "NTASKS_X",
    "NTASKS_Y",
    "ISWATER",
    "ISLAKE",
    "ISICE",
    "ISURBAN",
    "ISOILWATER",
];

const REAL_METADATA_ATTRIBUTES: &[&str] = &[
    "TRUELAT1",
    "TRUELAT2",
    "STAND_LON",
    "CEN_LAT",
    "CEN_LON",
    "MOAD_CEN_LAT",
    "POLE_LAT",
    "POLE_LON",
    "GMT",
];

#[derive(Default)]
struct WrfNamelistMetadata {
    source_name: String,
    nx: usize,
    ny: usize,
    nz: usize,
    nt: usize,
    integers: BTreeMap<String, i32>,
    reals: BTreeMap<String, f64>,
    strings: BTreeMap<String, String>,
    times: Vec<String>,
}

/// Read the bounded metadata needed for a clearly-labelled reconstruction.
///
/// This intentionally uses `WrfFile`'s fast pure-Rust named-attribute surface
/// instead of opening the large NetCDF index a second time merely to enumerate
/// attributes. Missing values remain missing; `WrfFile`'s DX/DY defaults are
/// never presented as stored metadata.
pub(crate) fn reconstruct_namelist_from_wrfout(path: &Path) -> Result<String, String> {
    let file = WrfFile::open(path)
        .map_err(|error| format!("open {} as raw WRF output: {error}", path.display()))?;
    let metadata = read_metadata(&file, path);
    Ok(render_reconstructed_namelist(&metadata))
}

fn read_metadata(file: &WrfFile, path: &Path) -> WrfNamelistMetadata {
    let mut metadata = WrfNamelistMetadata {
        source_name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "selected wrfout".to_owned()),
        nx: file.nx,
        ny: file.ny,
        nz: file.nz,
        nt: file.nt,
        times: file.times().unwrap_or_default(),
        ..WrfNamelistMetadata::default()
    };

    for spec in DOMAINS_FIELDS
        .iter()
        .chain(PHYSICS_FIELDS)
        .chain(DYNAMICS_FIELDS)
        .chain(FDDA_FIELDS)
        .chain(DFI_FIELDS)
    {
        match spec.kind {
            AttributeKind::Integer => {
                if let Ok(value) = file.global_attr_i32(spec.attribute) {
                    metadata.integers.insert(spec.attribute.to_owned(), value);
                }
            }
            AttributeKind::Real => {
                if let Ok(value) = file.global_attr_f64(spec.attribute) {
                    metadata.reals.insert(spec.attribute.to_owned(), value);
                }
            }
        }
    }
    for attribute in INTEGER_METADATA_ATTRIBUTES {
        if let Ok(value) = file.global_attr_i32(attribute) {
            metadata.integers.insert((*attribute).to_owned(), value);
        }
    }
    for attribute in REAL_METADATA_ATTRIBUTES {
        if let Ok(value) = file.global_attr_f64(attribute) {
            metadata.reals.insert((*attribute).to_owned(), value);
        }
    }
    for attribute in TEXT_METADATA_ATTRIBUTES {
        if let Ok(value) = file.global_attr_str(attribute) {
            metadata.strings.insert(
                (*attribute).to_owned(),
                value.trim_end_matches('\0').to_owned(),
            );
        }
    }
    metadata
}

fn render_reconstructed_namelist(metadata: &WrfNamelistMetadata) -> String {
    let mut output = String::new();
    let source = one_line(&metadata.source_name);
    writeln!(output, "! BOWECHO PARTIAL WRF NAMELIST RECONSTRUCTION").unwrap();
    writeln!(output, "! Source wrfout: {source}").unwrap();
    writeln!(output, "!").unwrap();
    writeln!(
        output,
        "! THIS IS NOT THE ORIGINAL namelist.input AND IS NOT A REPRODUCIBILITY RECORD."
    )
    .unwrap();
    writeln!(
        output,
        "! A wrfout stores only part of the effective run configuration. This file alone"
    )
    .unwrap();
    writeln!(
        output,
        "! CANNOT reproduce the run. Recover the original namelist, WPS inputs, boundary/initial"
    )
    .unwrap();
    writeln!(
        output,
        "! files, tables, source revision, build options, and runtime environment when possible."
    )
    .unwrap();
    writeln!(output, "!").unwrap();
    writeln!(
        output,
        "! Active assignments below are exact values stored in this one output file."
    )
    .unwrap();
    writeln!(
        output,
        "! They are indexed to this file's GRID_ID when domain-specific; other domains are unknown."
    )
    .unwrap();
    writeln!(
        output,
        "! Inferred and unavailable values remain comments and are never silently activated."
    )
    .unwrap();
    writeln!(output).unwrap();

    render_time_control(&mut output, metadata);
    render_section(
        &mut output,
        "domains",
        DOMAINS_FIELDS,
        metadata,
        &[
            "run duration/end date and adaptive-time-step controls are not recoverable",
            "values for domains not represented by this wrfout are not recoverable",
        ],
    );
    render_inferred_dimensions(&mut output, metadata);
    render_inferred_max_dom(&mut output, metadata);
    writeln!(output, "/\n").unwrap();

    render_closed_section(
        &mut output,
        "physics",
        PHYSICS_FIELDS,
        metadata,
        &[
            "scheme-specific options absent from the wrfout cannot be recovered",
            "physics tables/data files and code revision cannot be recovered",
        ],
    );
    render_closed_section(
        &mut output,
        "dynamics",
        DYNAMICS_FIELDS,
        metadata,
        &["dynamics settings not persisted as globals cannot be recovered"],
    );
    render_closed_section(
        &mut output,
        "fdda",
        FDDA_FIELDS,
        metadata,
        &["nudging input streams, coefficients, and omitted FDDA controls cannot be recovered"],
    );
    render_closed_section(
        &mut output,
        "dfi_control",
        DFI_FIELDS,
        metadata,
        &["DFI settings other than any exact stored dfi_opt value cannot be recovered"],
    );

    writeln!(output, "&bdy_control").unwrap();
    writeln!(
        output,
        "    ! unavailable from wrfout: specified/nested boundary configuration and widths"
    )
    .unwrap();
    writeln!(output, "/\n").unwrap();
    writeln!(output, "&namelist_quilt").unwrap();
    writeln!(
        output,
        "    ! unavailable from wrfout: nio_groups, nio_tasks_per_group, and I/O topology"
    )
    .unwrap();
    writeln!(output, "/\n").unwrap();

    render_metadata_appendix(&mut output, metadata);
    output
}

fn render_time_control(output: &mut String, metadata: &WrfNamelistMetadata) {
    writeln!(output, "&time_control").unwrap();
    let start = metadata
        .strings
        .get("START_DATE")
        .or_else(|| metadata.strings.get("SIMULATION_START_DATE"));
    if let Some(start) = start {
        writeln!(
            output,
            "    ! exact wrfout global start metadata: '{}'",
            fortran_string(start)
        )
        .unwrap();
        let normalized_start = start.trim_matches(char::is_whitespace);
        if let Ok(parsed) = NaiveDateTime::parse_from_str(normalized_start, "%Y-%m-%d_%H:%M:%S") {
            if let Some(domain) = domain_id(metadata) {
                writeln!(
                    output,
                    "    start_year({domain}) = {}, start_month({domain}) = {}, start_day({domain}) = {},  ! exact parse of stored start global",
                    parsed.format("%Y"),
                    parsed.format("%m"),
                    parsed.format("%d"),
                )
                .unwrap();
                writeln!(
                    output,
                    "    start_hour({domain}) = {}, start_minute({domain}) = {}, start_second({domain}) = {},  ! exact parse of stored start global",
                    parsed.format("%H"),
                    parsed.format("%M"),
                    parsed.format("%S"),
                )
                .unwrap();
            } else {
                writeln!(
                    output,
                    "    ! exact parsed start components are {}-{}-{}_{}:{}:{}, but GRID_ID is unavailable; start_* remain inactive",
                    parsed.format("%Y"),
                    parsed.format("%m"),
                    parsed.format("%d"),
                    parsed.format("%H"),
                    parsed.format("%M"),
                    parsed.format("%S"),
                )
                .unwrap();
            }
        }
    }
    writeln!(
        output,
        "    ! unavailable from wrfout: run_days/run_hours, end_*, history/restart intervals,"
    )
    .unwrap();
    writeln!(
        output,
        "    ! unavailable from wrfout: frames_per_outfile, input/output stream names, and I/O formats"
    )
    .unwrap();
    writeln!(output, "/\n").unwrap();
}

fn render_section(
    output: &mut String,
    name: &str,
    fields: &[FieldSpec],
    metadata: &WrfNamelistMetadata,
    unavailable: &[&str],
) {
    writeln!(output, "&{name}").unwrap();
    for field in fields {
        render_exact_field(output, *field, metadata);
    }
    for message in unavailable {
        writeln!(output, "    ! unavailable from wrfout: {message}").unwrap();
    }
}

fn render_closed_section(
    output: &mut String,
    name: &str,
    fields: &[FieldSpec],
    metadata: &WrfNamelistMetadata,
    unavailable: &[&str],
) {
    render_section(output, name, fields, metadata, unavailable);
    writeln!(output, "/\n").unwrap();
}

fn render_exact_field(output: &mut String, field: FieldSpec, metadata: &WrfNamelistMetadata) {
    let value = match field.kind {
        AttributeKind::Integer => metadata
            .integers
            .get(field.attribute)
            .map(ToString::to_string),
        AttributeKind::Real => metadata
            .reals
            .get(field.attribute)
            .filter(|value| value.is_finite())
            .map(ToString::to_string),
    };
    let Some(value) = value else {
        return;
    };
    // DT is an effective real-valued runtime timestep. It does not prove the
    // original integer time_step, fractional controls, or adaptive settings,
    // even in d01, so it is context only.
    if field.attribute == "DT" {
        let domain = domain_id(metadata)
            .map(|domain| format!("d{domain:02} "))
            .unwrap_or_default();
        writeln!(
            output,
            "    ! exact effective {domain}wrfout global DT = {value}; original time_step/fraction/adaptive controls remain unavailable"
        )
        .unwrap();
        return;
    }
    // WRF writes effective per-domain DX/DY globals into each wrfout, while
    // only d01 values are safe parent-scalar reconstruction inputs. Activating
    // a nested value would silently manufacture a different grid.
    if matches!(field.attribute, "DX" | "DY") {
        match domain_id(metadata) {
            Some(1) => {
                writeln!(
                    output,
                    "    {} = {value},  ! exact d01 wrfout global {}",
                    field.namelist_key, field.attribute
                )
                .unwrap();
            }
            Some(domain) => {
                writeln!(
                    output,
                    "    ! exact effective d{domain:02} wrfout global {} = {value}; parent scalar {} remains unavailable",
                    field.attribute, field.namelist_key
                )
                .unwrap();
            }
            None => {
                writeln!(
                    output,
                    "    ! exact wrfout global {} = {value}, but GRID_ID is unavailable; parent scalar {} remains inactive",
                    field.attribute, field.namelist_key
                )
                .unwrap();
            }
        }
        return;
    }
    if field.domain_scoped {
        if let Some(domain) = domain_id(metadata) {
            writeln!(
                output,
                "    {}({domain}) = {value},  ! exact wrfout global {}",
                field.namelist_key, field.attribute
            )
            .unwrap();
        } else {
            writeln!(
                output,
                "    ! exact global {} = {value}, but GRID_ID is unavailable; {} remains inactive",
                field.attribute, field.namelist_key
            )
            .unwrap();
        }
    } else {
        writeln!(
            output,
            "    {} = {value},  ! exact wrfout global {}",
            field.namelist_key, field.attribute
        )
        .unwrap();
    }
}

fn render_inferred_dimensions(output: &mut String, metadata: &WrfNamelistMetadata) {
    let suffix = domain_suffix(metadata);
    for (attribute, key, value) in [
        (
            "WEST-EAST_GRID_DIMENSION",
            "e_we",
            metadata.nx.checked_add(1),
        ),
        (
            "SOUTH-NORTH_GRID_DIMENSION",
            "e_sn",
            metadata.ny.checked_add(1),
        ),
        (
            "BOTTOM-TOP_GRID_DIMENSION",
            "e_vert",
            metadata.nz.checked_add(1),
        ),
    ] {
        if !metadata.integers.contains_key(attribute)
            && let Some(value) = value
        {
            writeln!(
                output,
                "    ! [exact:file dimension] mapped to WRF staggered extent: {key}{suffix} = {value},"
            )
            .unwrap();
        }
    }
}

fn render_inferred_max_dom(output: &mut String, metadata: &WrfNamelistMetadata) {
    if !metadata.integers.contains_key("MAX_DOM")
        && let Some(domain) = domain_id(metadata)
    {
        writeln!(
            output,
            "    ! inferred lower bound only from GRID_ID: max_dom >= {domain}; exact max_dom is unavailable"
        )
        .unwrap();
    }
}

fn render_metadata_appendix(output: &mut String, metadata: &WrfNamelistMetadata) {
    writeln!(
        output,
        "! Exact output metadata below is provenance/context, not active namelist.input syntax."
    )
    .unwrap();
    writeln!(
        output,
        "! Exact WRF dataset shape: Time={} bottom_top={} south_north={} west_east={}",
        metadata.nt, metadata.nz, metadata.ny, metadata.nx
    )
    .unwrap();
    for attribute in TEXT_METADATA_ATTRIBUTES {
        if let Some(value) = metadata.strings.get(*attribute) {
            writeln!(
                output,
                "! exact wrfout global {attribute} = '{}'",
                fortran_string(value)
            )
            .unwrap();
        }
    }
    for attribute in INTEGER_METADATA_ATTRIBUTES {
        if let Some(value) = metadata.integers.get(*attribute) {
            writeln!(output, "! exact wrfout global {attribute} = {value}").unwrap();
        }
    }
    for attribute in REAL_METADATA_ATTRIBUTES {
        if let Some(value) = metadata.reals.get(*attribute)
            && value.is_finite()
        {
            writeln!(output, "! exact wrfout global {attribute} = {value}").unwrap();
        }
    }
    for (index, time) in metadata.times.iter().enumerate() {
        writeln!(
            output,
            "! exact WRF Times[{index}] = '{}' (an output time, not proof of configured start/end)",
            fortran_string(time)
        )
        .unwrap();
    }
    writeln!(
        output,
        "! Projection globals are WPS/geogrid context; a namelist.wps cannot be recovered here."
    )
    .unwrap();
}

fn domain_id(metadata: &WrfNamelistMetadata) -> Option<i32> {
    metadata
        .integers
        .get("GRID_ID")
        .copied()
        .filter(|domain| *domain > 0)
}

fn domain_suffix(metadata: &WrfNamelistMetadata) -> String {
    domain_id(metadata)
        .map(|domain| format!("({domain})"))
        .unwrap_or_default()
}

fn fortran_string(value: &str) -> String {
    one_line(value).replace('\'', "''")
}

fn one_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn representative_metadata() -> WrfNamelistMetadata {
        let mut metadata = WrfNamelistMetadata {
            source_name: "wrfout_d02_1974-04-03_23_00_00".to_owned(),
            nx: 100,
            ny: 80,
            nz: 49,
            nt: 1,
            times: vec!["1974-04-03_23:00:00".to_owned()],
            ..WrfNamelistMetadata::default()
        };
        metadata.integers.extend([
            ("GRID_ID".to_owned(), 2),
            ("PARENT_ID".to_owned(), 1),
            ("WEST-EAST_GRID_DIMENSION".to_owned(), 101),
            ("MP_PHYSICS".to_owned(), 8),
            ("MAP_PROJ".to_owned(), 1),
        ]);
        metadata.reals.extend([
            ("DT".to_owned(), 3.0),
            ("DX".to_owned(), 1000.0),
            ("TRUELAT1".to_owned(), 25.66670036315918),
        ]);
        metadata.strings.extend([
            ("START_DATE".to_owned(), "1974-04-03_18:00:00".to_owned()),
            (
                "TITLE".to_owned(),
                " OUTPUT FROM WRF V4.7.1 MODEL".to_owned(),
            ),
        ]);
        metadata
    }

    #[test]
    fn reconstruction_separates_exact_inferred_and_unrecoverable_values() {
        let rendered = render_reconstructed_namelist(&representative_metadata());
        assert!(rendered.contains("THIS IS NOT THE ORIGINAL namelist.input"));
        assert!(rendered.contains("CANNOT reproduce the run"));
        assert!(rendered.contains("mp_physics(2) = 8,  ! exact wrfout global MP_PHYSICS"));
        assert!(rendered.contains(
            "exact effective d02 wrfout global DT = 3; original time_step/fraction/adaptive controls remain unavailable"
        ));
        assert!(!rendered.contains("\n    time_step = 3,"));
        assert!(rendered.contains(
            "exact effective d02 wrfout global DX = 1000; parent scalar dx remains unavailable"
        ));
        assert!(!rendered.contains("dx(2)"));
        assert!(rendered.contains("start_year(2) = 1974"));
        assert!(rendered.contains("exact parse of stored start global"));
        assert!(rendered.contains("! inferred lower bound only from GRID_ID: max_dom >= 2"));
        assert!(rendered.contains("! unavailable from wrfout: run_days/run_hours"));
        assert!(rendered.contains("an output time, not proof of configured start/end"));
    }

    #[test]
    fn missing_dimension_globals_keep_exact_file_shape_mapping_commented() {
        let mut metadata = representative_metadata();
        metadata.integers.remove("WEST-EAST_GRID_DIMENSION");
        let rendered = render_reconstructed_namelist(&metadata);
        assert!(
            rendered.contains(
                "! [exact:file dimension] mapped to WRF staggered extent: e_we(2) = 101,"
            )
        );
        assert!(!rendered.contains("\n    e_we(2) = 101,"));
    }

    #[test]
    fn unknown_domain_never_activates_domain_scoped_values() {
        let mut metadata = representative_metadata();
        metadata.integers.remove("GRID_ID");
        let rendered = render_reconstructed_namelist(&metadata);
        assert!(rendered.contains(
            "! exact global MP_PHYSICS = 8, but GRID_ID is unavailable; mp_physics remains inactive"
        ));
        assert!(!rendered.contains("\n    mp_physics = 8,"));
    }

    #[test]
    fn root_domain_dx_dy_activate_but_effective_dt_stays_context_only() {
        let mut metadata = representative_metadata();
        metadata.integers.insert("GRID_ID".to_owned(), 1);
        metadata.reals.insert("DY".to_owned(), 1000.0);
        let rendered = render_reconstructed_namelist(&metadata);
        assert!(rendered.contains(
            "exact effective d01 wrfout global DT = 3; original time_step/fraction/adaptive controls remain unavailable"
        ));
        assert!(!rendered.contains("\n    time_step = 3,"));
        assert!(rendered.contains("dx = 1000,  ! exact d01 wrfout global DX"));
        assert!(rendered.contains("dy = 1000,  ! exact d01 wrfout global DY"));
        assert!(!rendered.contains("dx(1)"));
        assert!(!rendered.contains("dy(1)"));
    }

    #[test]
    fn metadata_strings_are_single_line_and_fortran_escaped() {
        let mut metadata = representative_metadata();
        metadata
            .strings
            .insert("TITLE".to_owned(), "owner's\nWRF run".to_owned());
        let rendered = render_reconstructed_namelist(&metadata);
        assert!(rendered.contains("owner''s WRF run"));
        assert!(!rendered.contains("owner's\n"));
    }
}
