// SPDX-License-Identifier: Apache-2.0

//! The ADVANCED CONFIG SURFACE: the engine's own generated TOML as a
//! grouped, searchable, field-by-field editor.
//!
//! Mechanism (proven against the live engine before this file was
//! written): a dry-run intent plan makes the engine write its config +
//! side files (WPS namelist, Vtable) into a durable Studio workspace;
//! the EDITED copy saved beside them resolves and runs through
//! `config.path`, every relative reference intact. Every edit is
//! re-validated by a debounced engine `--resolve`; an invalid value
//! surfaces the engine's own refusal sentence — in a banner, and inline
//! at the field whose key the sentence names. Studio validates NOTHING
//! itself: even TOML syntax errors are the engine loader's to refuse.
//!
//! The grouping/hint tables below are PRESENTATION metadata only. Every
//! key found in the file is shown and editable — an unmapped key lands
//! in its table's own group, never omitted.

use std::path::PathBuf;
use std::time::Instant;

use eframe::egui;

use arwen_plan::queries::{ResolveReport, ResolvedDomain};

use crate::kit;
use crate::theme::theme;

// ---------------------------------------------------------------------------
// The line model: parse `key = value` spans out of the engine's emitted
// TOML, edit values in place, keep every other byte (comments included).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Table path: "experiment", "projection", "shared", "fetch",
    /// "case_data", or "domain[N]".
    pub table: String,
    /// Human label for the table ("d01" for domain[0]).
    pub table_label: String,
    pub key: String,
    /// Line span of `key = value` (inclusive; arrays span lines).
    pub line_start: usize,
    pub line_end: usize,
    /// Everything after `key = `, joined with newlines for spans.
    pub value: String,
}

#[derive(Clone, Debug, Default)]
pub struct ConfigModel {
    pub text: String,
    pub entries: Vec<Entry>,
}

impl ConfigModel {
    pub fn parse(text: &str) -> Self {
        let lines: Vec<&str> = text.lines().collect();
        let mut entries = Vec::new();
        let mut table = String::new();
        let mut array_counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        let mut index = 0usize;
        while index < lines.len() {
            let line = lines[index];
            let trimmed = line.trim();
            if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
                let name = trimmed.trim_matches(['[', ']']).trim().to_string();
                let count = array_counts.entry(name.clone()).or_insert(0);
                table = format!("{name}[{count}]");
                *count += 1;
                index += 1;
                continue;
            }
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                table = trimmed.trim_matches(['[', ']']).trim().to_string();
                index += 1;
                continue;
            }
            if let Some((key, first_value)) = split_key_value(line) {
                let mut end = index;
                let mut value_lines = vec![first_value.to_string()];
                // A multi-line array: opening brackets not yet closed.
                let mut depth = bracket_depth(first_value);
                while depth > 0 && end + 1 < lines.len() {
                    end += 1;
                    value_lines.push(lines[end].to_string());
                    depth += bracket_depth(lines[end]);
                }
                let table_label = table_label(&table);
                entries.push(Entry {
                    table: table.clone(),
                    table_label,
                    key: key.to_string(),
                    line_start: index,
                    line_end: end,
                    value: value_lines.join("\n"),
                });
                index = end + 1;
                continue;
            }
            index += 1;
        }
        Self {
            text: text.to_string(),
            entries,
        }
    }

    /// The `domain[N]` index of the `[[domain]]` whose `grid_id` equals
    /// `grid_id` — the bridge from a map gesture (tree grid ids) to the
    /// config's table order.
    pub fn domain_index_for_grid(&self, grid_id: u32) -> Option<usize> {
        for index in 0.. {
            let table = format!("domain[{index}]");
            let value = self
                .entries
                .iter()
                .find(|entry| entry.table == table && entry.key == "grid_id")
                .map(|entry| entry.value.trim())?;
            if value.parse::<u32>() == Ok(grid_id)
                || value.parse::<f64>().ok().map(|v| v as u32) == Some(grid_id)
            {
                return Some(index);
            }
        }
        None
    }

    /// The raw value of `key` in `[[domain]]` number `domain_index`.
    pub fn domain_value(&self, domain_index: usize, key: &str) -> Option<&str> {
        let table = format!("domain[{domain_index}]");
        self.entries
            .iter()
            .find(|entry| entry.table == table && entry.key == key)
            .map(|entry| entry.value.as_str())
    }

    /// Rewrite one nest's placement — `i_parent_start`/`j_parent_start`
    /// and, on resize, `nx`/`ny` — in `[[domain]]` number `domain_index`.
    /// Whole-cell integers exactly as the map gesture snapped them; the
    /// engine's `--resolve` remains the placement judge. Missing keys are
    /// left missing (never invented): the engine's loader owns the shape.
    pub fn with_placement(
        &self,
        domain_index: usize,
        i_parent_start: i64,
        j_parent_start: i64,
        size: Option<(u32, u32)>,
    ) -> Self {
        let mut model = self.clone();
        let mut writes: Vec<(String, String)> = vec![
            ("i_parent_start".into(), i_parent_start.to_string()),
            ("j_parent_start".into(), j_parent_start.to_string()),
        ];
        if let Some((nx, ny)) = size {
            writes.push(("nx".into(), nx.to_string()));
            writes.push(("ny".into(), ny.to_string()));
        }
        let table = format!("domain[{domain_index}]");
        for (key, value) in writes {
            let Some(index) = model
                .entries
                .iter()
                .position(|entry| entry.table == table && entry.key == key)
            else {
                continue;
            };
            model = model.with_edit(index, &value);
        }
        model
    }

    /// The `domain[N]` index of the ROOT `[[domain]]` (`parent_id = 0`).
    pub fn root_domain_index(&self) -> Option<usize> {
        for index in 0.. {
            let table = format!("domain[{index}]");
            if !self.entries.iter().any(|entry| entry.table == table) {
                return None;
            }
            let is_root = self
                .domain_value(index, "parent_id")
                .map(str::trim)
                .and_then(|value| value.parse::<f64>().ok())
                == Some(0.0);
            if is_root {
                return Some(index);
            }
        }
        None
    }

    /// Rewrite the ROOT rectangle in place: `[projection]`
    /// ref_lat/ref_lon/stand_lon follow the sketch's center and the root
    /// `[[domain]]` takes the drawn/resized nx/ny — every other byte
    /// (children, physics, comments) untouched. Missing keys are left
    /// missing (never invented); the engine's `--resolve` stays the
    /// judge of the result.
    pub fn with_root_geometry(&self, ref_lat: f64, ref_lon: f64, nx: u32, ny: u32) -> Self {
        let mut model = self.clone();
        let lon = format!("{ref_lon:.4}");
        let writes: [(&str, &str, String); 3] = [
            ("projection", "ref_lat", format!("{ref_lat:.4}")),
            ("projection", "ref_lon", lon.clone()),
            ("projection", "stand_lon", lon),
        ];
        for (table, key, value) in writes {
            let Some(index) = model
                .entries
                .iter()
                .position(|entry| entry.table == table && entry.key == key)
            else {
                continue;
            };
            model = model.with_edit(index, &value);
        }
        if let Some(root) = model.root_domain_index() {
            model = model.with_placement(root, 1, 1, Some((nx, ny)));
        }
        model
    }

    /// Rebuild the text with one entry's value replaced, byte-identical
    /// everywhere else, and re-parse (line spans shift).
    pub fn with_edit(&self, entry_index: usize, new_value: &str) -> Self {
        let Some(entry) = self.entries.get(entry_index) else {
            return self.clone();
        };
        let lines: Vec<&str> = self.text.lines().collect();
        let mut out: Vec<String> = Vec::with_capacity(lines.len());
        for (index, line) in lines.iter().enumerate() {
            if index == entry.line_start {
                out.push(format!("{} = {}", entry.key, new_value));
            } else if index > entry.line_start && index <= entry.line_end {
                // swallowed by the replacement
            } else {
                out.push((*line).to_string());
            }
        }
        let mut text = out.join("\n");
        if self.text.ends_with('\n') {
            text.push('\n');
        }
        Self::parse(&text)
    }
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let without_indent = line.trim_start();
    if without_indent.starts_with('#') {
        return None;
    }
    let equals = line.find(" = ")?;
    let key = line[..equals].trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((key, line[equals + 3..].trim()))
}

fn bracket_depth(text: &str) -> i32 {
    let mut depth = 0;
    let mut in_string = false;
    for c in text.chars() {
        match c {
            '"' => in_string = !in_string,
            '[' if !in_string => depth += 1,
            ']' if !in_string => depth -= 1,
            _ => {}
        }
    }
    depth
}

fn table_label(table: &str) -> String {
    if let Some(rest) = table.strip_prefix("domain[") {
        let index: usize = rest.trim_end_matches(']').parse().unwrap_or(0);
        format!("d{:02}", index + 1)
    } else {
        table.to_string()
    }
}

/// One nest's mechanical repair after a root-rectangle change.
#[derive(Clone, Debug, PartialEq)]
pub struct ChildRepair {
    pub grid_id: u32,
    /// The row notice, e.g. "d02 refit — no longer fit at 600×320
    /// after the root change".
    pub notice: String,
}

/// AFTER a root geometry write: carry every nest back into its
/// parent's legal envelope — whole-cell snap inside the clearance
/// band, parents processed before their children so a repaired parent
/// re-frames its own nests. Rungs, most byte-preserving first:
///   1. still fits unchanged → untouched, no notice;
///   2. anchor clamped back inside, size kept;
///   3. cannot fit at its size → reset to the emission's fitted size,
///      anchor clamped;
///   4. cannot fit even fitted → left alone for the engine's refusal
///      (the map paints it red).
/// Never leaves a config the engine will refuse when a legal repair is
/// mechanical; the engine's `--resolve` remains the judge of every
/// result.
#[cfg_attr(not(test), allow(dead_code))] // test convenience over _with_floor
pub fn carry_children(
    model: &ConfigModel,
    base: &ConfigModel,
    clearance_rows: f64,
) -> (ConfigModel, Vec<ChildRepair>) {
    carry_children_with_floor(model, base, clearance_rows, None)
}

/// [`carry_children`] with the engine's reported nest-span floor, which
/// arms the SHRINK-TO-FIT rung (the regression's -74: a footprint-true 52×31
/// twelve-km root can never host the emission's 408×320 child at any
/// anchor, but a 124-wide child is perfectly legal there).
pub fn carry_children_with_floor(
    model: &ConfigModel,
    base: &ConfigModel,
    clearance_rows: f64,
    nest_span_floor: Option<u32>,
) -> (ConfigModel, Vec<ChildRepair>) {
    let clearance = clearance_rows.max(0.0);
    let mut model = model.clone();
    let mut repairs = Vec::new();
    for index in 0.. {
        let table = format!("domain[{index}]");
        if !model.entries.iter().any(|entry| entry.table == table) {
            break;
        }
        let value = |m: &ConfigModel, idx: usize, key: &str| -> Option<f64> {
            m.domain_value(idx, key)?.trim().parse::<f64>().ok()
        };
        let Some(parent_grid) = value(&model, index, "parent_id") else {
            continue;
        };
        if parent_grid == 0.0 {
            continue;
        }
        let Some(parent_index) = model.domain_index_for_grid(parent_grid as u32) else {
            continue;
        };
        let (
            Some(grid_id),
            Some(i),
            Some(j),
            Some(nx),
            Some(ny),
            Some(ratio),
            Some(parent_nx),
            Some(parent_ny),
        ) = (
            value(&model, index, "grid_id"),
            value(&model, index, "i_parent_start"),
            value(&model, index, "j_parent_start"),
            value(&model, index, "nx"),
            value(&model, index, "ny"),
            value(&model, index, "parent_grid_ratio"),
            value(&model, parent_index, "nx"),
            value(&model, parent_index, "ny"),
        )
        else {
            continue;
        };
        let ratio = ratio.max(1.0);
        let lo = (1.0 + clearance).ceil() as i64;
        let hi = |span: f64, parent_n: f64| -> i64 {
            (parent_n - clearance - (span - 1.0) / ratio).floor() as i64
        };
        let fits = |nx: f64, ny: f64| -> Option<(i64, i64)> {
            let hi_i = hi(nx, parent_nx);
            let hi_j = hi(ny, parent_ny);
            (hi_i >= lo && hi_j >= lo).then_some((hi_i, hi_j))
        };
        let grid = grid_id as u32;
        if let Some((hi_i, hi_j)) = fits(nx, ny) {
            let ci = (i.round() as i64).clamp(lo, hi_i);
            let cj = (j.round() as i64).clamp(lo, hi_j);
            if ci != i.round() as i64 || cj != j.round() as i64 {
                model = model.with_placement(index, ci, cj, None);
                repairs.push(ChildRepair {
                    grid_id: grid,
                    notice: format!(
                        "d{grid:02} carried along — anchor clamped to ({ci}, {cj}) \
                         inside the resized parent"
                    ),
                });
            }
            continue;
        }
        // Cannot fit at its current size: fall back to the emission's
        // own fitted placement, clamped into the new envelope.
        if let Some((bi, bj, bnx, bny)) = base_placement(base, index)
            && let Some((hi_i, hi_j)) = fits(bnx as f64, bny as f64)
        {
            let ci = bi.clamp(lo, hi_i);
            let cj = bj.clamp(lo, hi_j);
            model = model.with_placement(index, ci, cj, Some((bnx, bny)));
            repairs.push(ChildRepair {
                grid_id: grid,
                notice: format!(
                    "d{grid:02} refit — no longer fit at {}×{} after the root change",
                    nx as u32, ny as u32,
                ),
            });
            continue;
        }
        // SHRINK TO FIT: the largest ratio-multiple spans this parent
        // can legally host (whole cells inside the clearance band),
        // capped at the current size, floored at the engine's reported
        // nest-span minimum — the regression's -74 rung. Anchor: the old anchor
        // clamped into the shrunken envelope.
        let floor_points = nest_span_floor.unwrap_or(1).max(1) as f64;
        let lo_f = lo as f64;
        let max_span = |parent_n: f64| -> i64 {
            let span = ratio * (parent_n - clearance - lo_f) + 1.0;
            let multiple = (span / ratio).floor() * ratio;
            multiple as i64
        };
        let new_nx = max_span(parent_nx).min(nx as i64);
        let new_ny = max_span(parent_ny).min(ny as i64);
        if new_nx as f64 >= floor_points
            && new_ny as f64 >= floor_points
            && let Some((hi_i, hi_j)) = fits(new_nx as f64, new_ny as f64)
        {
            let ci = (i.round() as i64).clamp(lo, hi_i);
            let cj = (j.round() as i64).clamp(lo, hi_j);
            model = model.with_placement(index, ci, cj, Some((new_nx as u32, new_ny as u32)));
            repairs.push(ChildRepair {
                grid_id: grid,
                notice: format!(
                    "d{grid:02} shrunk to fit — {new_nx}×{new_ny} inside the \
                     changed parent (was {}×{})",
                    nx as u32, ny as u32,
                ),
            });
            continue;
        }
        // No mechanical repair exists (the parent cannot host even the
        // engine's minimum nest): the engine's refusal (painted on the
        // map) is the honest state.
        repairs.push(ChildRepair {
            grid_id: grid,
            notice: format!(
                "d{grid:02} cannot fit inside its parent at any anchor — \
                 the engine's refusal shows on the map"
            ),
        });
    }
    (model, repairs)
}

/// The clearance the mechanical carry clamps against when no resolve
/// has reported the floor yet: the config's OWN `spec_bdy_width +
/// blend_width` — the exact sum the engine's refusal sentence names.
/// Engine numbers read from the engine's file; nothing invented.
pub fn model_clearance_rows(model: &ConfigModel) -> Option<f64> {
    let read = |key: &str| {
        model
            .entries
            .iter()
            .find(|entry| entry.table == "experiment" && entry.key == key)
            .and_then(|entry| entry.value.trim().parse::<f64>().ok())
    };
    Some(read("spec_bdy_width")? + read("blend_width")?)
}

/// Every domain a refusal names, with the naming line: the engine's
/// placement sentences spell the offender as `grid_id = N`. Feeds the
/// map's red outlines, the inspector row flags, and the estimate
/// strip's "see map" headline.
pub fn refusal_named_domains(error: &str) -> Vec<(u32, String)> {
    let mut named: Vec<(u32, String)> = Vec::new();
    for line in error.lines() {
        let mut search = line;
        while let Some(position) = search.find("grid_id") {
            let tail = search[position + "grid_id".len()..].trim_start();
            if let Some(tail) = tail.strip_prefix('=') {
                let tail = tail.trim_start();
                let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(grid) = digits.parse::<u32>()
                    && !named.iter().any(|(existing, _)| *existing == grid)
                {
                    named.push((grid, line.trim().to_string()));
                }
            }
            search = &search[position + "grid_id".len()..];
        }
    }
    named
}

/// Do the working config's placement keys for `[[domain]]` number
/// `domain_index` differ from the engine's own emission? Drives the
/// "auto (fitted)" ↔ "manual" chip; reset-to-fitted restores the base
/// values through [`ConfigModel::with_placement`].
pub fn placement_is_manual(model: &ConfigModel, base: &ConfigModel, domain_index: usize) -> bool {
    ["i_parent_start", "j_parent_start", "nx", "ny"]
        .iter()
        .any(|key| {
            let current = model.domain_value(domain_index, key).map(str::trim);
            let fitted = base.domain_value(domain_index, key).map(str::trim);
            match (current, fitted) {
                (Some(current), Some(fitted)) => {
                    // Numeric comparison so "52" == "52.0" never reads
                    // as a manual move.
                    match (current.parse::<f64>(), fitted.parse::<f64>()) {
                        (Ok(a), Ok(b)) => a != b,
                        _ => current != fitted,
                    }
                }
                (a, b) => a != b,
            }
        })
}

/// The engine's size floors from the last good resolve, for the min
/// hints on the size fields. Engine numbers only — `None` = unreported.
#[derive(Clone, Copy, Debug, Default)]
pub struct FloorHints {
    /// `domain_size_floor.nest_span_mass_points` (per-axis nest minimum).
    pub nest_span: Option<u32>,
    /// `domain_size_floor.root_mass_points` (nx, ny).
    pub root: Option<(u32, u32)>,
}

pub fn floor_hints(state: &AdvancedState) -> FloorHints {
    let Some(floor) = state
        .resolve
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .and_then(|report| report.domain_size_floor.as_ref())
    else {
        return FloorHints::default();
    };
    FloorHints {
        nest_span: floor
            .get("nest_span_mass_points")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        root: floor.get("root_mass_points").and_then(|value| {
            Some((
                value.get("nx")?.as_u64()? as u32,
                value.get("ny")?.as_u64()? as u32,
            ))
        }),
    }
}

/// The engine's fitted placement for `[[domain]]` number `domain_index`,
/// read from the base emission: (i_parent_start, j_parent_start, nx, ny).
pub fn base_placement(base: &ConfigModel, domain_index: usize) -> Option<(i64, i64, u32, u32)> {
    let int = |key: &str| -> Option<i64> {
        let value = base.domain_value(domain_index, key)?.trim();
        value
            .parse::<i64>()
            .ok()
            .or_else(|| value.parse::<f64>().ok().map(|v| v as i64))
    };
    Some((
        int("i_parent_start")?,
        int("j_parent_start")?,
        int("nx")? as u32,
        int("ny")? as u32,
    ))
}

// ---------------------------------------------------------------------------
// Presentation metadata: group + hint per key. NEVER filters — an
// unmapped key shows under its table's group.
// ---------------------------------------------------------------------------

pub const GROUP_ORDER: &[&str] = &[
    "Grid & placement",
    "Projection",
    "Vertical coordinate",
    "Time step & acoustics",
    "Microphysics",
    "PBL & surface layer",
    "Land surface",
    "Radiation (profile-governed)",
    "Cumulus",
    "Damping & diffusion",
    "Advection & numerics",
    "Boundaries & nesting",
    "Output & restart",
    "Storm following & spawn (1.8)",
    "Experiment",
    "Fetch",
    "Case data",
    "Other",
];

/// (group, hint) for a key. Hints are one-line orientation, not science.
fn group_for(table: &str, key: &str) -> (&'static str, &'static str) {
    let by_key: Option<(&'static str, &'static str)> = match key {
        "nx" | "ny" => Some((
            "Grid & placement",
            "mass-grid points (engine-fitted; edits are yours to own)",
        )),
        "dx" | "dy" => Some(("Grid & placement", "grid spacing, meters")),
        "i_parent_start" | "j_parent_start" => {
            Some(("Grid & placement", "nest anchor in PARENT grid points"))
        }
        "parent_grid_ratio" => Some(("Grid & placement", "dx refinement vs parent")),
        "parent_time_step_ratio" => Some(("Grid & placement", "dt refinement vs parent")),
        "specified" | "nested" => Some(("Grid & placement", "boundary role flags")),
        "grid_id" | "parent_id" => Some(("Grid & placement", "tree identity")),
        "map_proj" | "ref_lat" | "ref_lon" | "truelat1" | "truelat2" | "stand_lon" => {
            Some(("Projection", "WPS &geogrid projection parameter"))
        }
        "nz" => Some(("Vertical coordinate", "mass levels")),
        "ztop" => Some(("Vertical coordinate", "model top height, m")),
        "p_top" => Some(("Vertical coordinate", "model top pressure, Pa")),
        "eta_levels" => Some(("Vertical coordinate", "full eta interface set, 1.0 → 0.0")),
        "hybrid_opt" => Some(("Vertical coordinate", "hybrid vertical coordinate switch")),
        "etac" => Some(("Vertical coordinate", "hybrid transition eta")),
        "time_step" => Some(("Time step & acoustics", "root dt, integer seconds")),
        "time_step_fract_num" | "time_step_fract_den" => {
            Some(("Time step & acoustics", "rational dt correction"))
        }
        "time_step_sound" => Some(("Time step & acoustics", "acoustic substeps per dt")),
        "epssm" => Some(("Time step & acoustics", "acoustic time off-centering")),
        "mp_physics" => Some(("Microphysics", "scheme selector")),
        "morr_rimed_ice" => Some(("Microphysics", "Morrison rimed-ice identity")),
        "wsm6_hail_opt" => Some(("Microphysics", "WSM6 hail switch")),
        "moist" | "moist_cq" => Some(("Microphysics", "moisture coupling flags")),
        "bl_pbl_physics" => Some(("PBL & surface layer", "PBL scheme")),
        "sf_sfclay_physics" => Some((
            "PBL & surface layer",
            "surface-layer scheme (paired with PBL)",
        )),
        "bldt" => Some(("PBL & surface layer", "PBL call interval, min")),
        "sf_surface_physics" => Some(("Land surface", "LSM selector")),
        "num_soil_layers" => Some(("Land surface", "soil column depth")),
        "terrain_opt" => Some(("Land surface", "terrain switch")),
        "ra_physics" | "ra_lw_physics" | "ra_sw_physics" => Some((
            "Radiation (profile-governed)",
            "radiation stream selector — coherent sets come from the physics profile",
        )),
        "radt" => Some((
            "Radiation (profile-governed)",
            "radiation call interval, min",
        )),
        "wrf_rrtmg_compatibility" | "ra_rrtmg_variant" => Some((
            "Radiation (profile-governed)",
            "RRTMG lineage switch — profile-governed",
        )),
        "cu_physics" => Some(("Cumulus", "cumulus scheme (0 = explicit only)")),
        "cudt_minutes" => Some(("Cumulus", "cumulus call interval, min")),
        "w_damping" => Some(("Damping & diffusion", "vertical-velocity damping switch")),
        "damp_opt" => Some(("Damping & diffusion", "upper damping option")),
        "zdamp" => Some(("Damping & diffusion", "damping layer depth, m")),
        "dampcoef" => Some(("Damping & diffusion", "damping coefficient")),
        "diff_6th_opt" | "diff_6th_factor" | "diff_6th_slopeopt" => {
            Some(("Damping & diffusion", "6th-order horizontal filter"))
        }
        "khdif" | "kvdif" => Some(("Damping & diffusion", "constant diffusivity, m²/s")),
        "smdiv" => Some(("Damping & diffusion", "divergence damping")),
        "emdiv" => Some(("Damping & diffusion", "external-mode damping")),
        "km_opt" => Some(("Damping & diffusion", "eddy coefficient option")),
        "moist_adv_opt" => Some(("Advection & numerics", "moisture advection option")),
        "h_sca_adv_order" => Some(("Advection & numerics", "horizontal scalar advection order")),
        "hypsometric_opt" => Some(("Advection & numerics", "hypsometric option")),
        "top_lid" => Some(("Advection & numerics", "rigid lid switch")),
        "base_temp" => Some(("Advection & numerics", "base-state temperature, K")),
        "nwp_diagnostics" => Some(("Output & restart", "NWP diagnostics switch")),
        "spec_bdy_width" | "spec_zone" | "relax_zone" | "blend_width" => {
            Some(("Boundaries & nesting", "Davies boundary / blend widths"))
        }
        "feedback" | "smooth_option" => Some(("Boundaries & nesting", "child→parent feedback")),
        "history_interval_s" => Some((
            "Output & restart",
            "wrfout cadence, s (whole steps — engine-checked)",
        )),
        "restart_interval_s" => Some(("Output & restart", "checkpoint cadence, s")),
        "name" | "start_time" | "run_seconds" => Some(("Experiment", "run identity / window")),
        _ => None,
    };
    if key == "spawn" {
        return (
            "Storm following & spawn (1.8)",
            "dormant-nest declaration (inline table; validated by the engine)",
        );
    }
    if let Some(found) = by_key {
        return found;
    }
    if table == "relocation" || table.starts_with("relocation.") {
        return (
            "Storm following & spawn (1.8)",
            "discrete-relocation surface (validated by the engine)",
        );
    }
    match table {
        "fetch" => ("Fetch", "the wizard's fetch hints"),
        "case_data" => (
            "Case data",
            "declared inputs (paths relative to this config)",
        ),
        "experiment" => ("Experiment", ""),
        "projection" => ("Projection", ""),
        _ => ("Other", ""),
    }
}

// ---------------------------------------------------------------------------
// State + window UI
// ---------------------------------------------------------------------------

pub struct AdvancedState {
    pub open: bool,
    pub search: String,
    pub model: ConfigModel,
    /// The drafts workspace holding the emission + side files (kept for
    /// cleanup offers and the hover provenance line).
    #[allow(dead_code)]
    pub workspace: PathBuf,
    /// The EDITED file (written beside the engine's own emission).
    pub config_path: PathBuf,
    /// Route fixed at generation time (mirrored into `Draft::custom`).
    #[allow(dead_code)]
    pub route: String,
    /// The engine's emission, for the "modified" marker.
    pub base_text: String,
    /// The emission parsed once — the "fitted" side of the placement
    /// chips and reset-to-fitted.
    pub base_model: ConfigModel,
    /// Debounce anchor: set on every edit, consumed by the app's poll.
    pub dirty_at: Option<Instant>,
    /// Bumped every accepted edit; feeds the draft fingerprint.
    pub rev: u64,
    pub resolve: Option<Result<Box<ResolveReport>, String>>,
    pub resolving: bool,
    /// Cached from the last good resolve, for the map.
    pub fitted: Option<arwen_map::LambertDomain>,
    pub tree: Vec<ResolvedDomain>,
    /// The last root change's mechanical child repairs (carry/refit
    /// notices, shown on the inspector rows). A direct gesture on a
    /// nest clears its own entry.
    pub repairs: Vec<ChildRepair>,
}

impl AdvancedState {
    pub fn new(workspace: PathBuf, config_path: PathBuf, route: String, text: String) -> Self {
        Self {
            open: true,
            search: String::new(),
            model: ConfigModel::parse(&text),
            workspace,
            config_path,
            route,
            base_model: ConfigModel::parse(&text),
            base_text: text,
            // Validate immediately: the engine's verdict (and the fitted
            // tree the map paints) must not wait for a first edit.
            dirty_at: Some(Instant::now()),
            rev: 0,
            resolve: None,
            resolving: false,
            fitted: None,
            tree: Vec::new(),
            repairs: Vec::new(),
        }
    }

    pub fn is_modified(&self) -> bool {
        self.model.text != self.base_text
    }
}

#[derive(Debug, Default)]
pub struct AdvancedActions {
    pub regenerate: bool,
    pub discard: bool,
}

/// Does this refusal line name this key as a word?
fn line_mentions_key(line: &str, key: &str) -> bool {
    line.match_indices(key).any(|(start, _)| {
        let bytes = line.as_bytes();
        let before_ok =
            start == 0 || !(bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        let end = start + key.len();
        let after_ok =
            end >= bytes.len() || !(bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_');
        before_ok && after_ok
    })
}

/// The refusal line for a specific entry, if the engine's sentence
/// names its key (placement only — the full text stays in the banner).
fn refusal_line_for_entry<'e>(state: &'e AdvancedState, entry: &Entry) -> Option<&'e str> {
    let Some(Err(error)) = &state.resolve else {
        return None;
    };
    error
        .lines()
        .find(|line| line_mentions_key(line, &entry.key))
}

pub fn window_ui(ctx: &egui::Context, state: &mut AdvancedState, actions: &mut AdvancedActions) {
    let mut open = state.open;
    egui::Window::new("Advanced config — every engine knob")
        .open(&mut open)
        .resizable(true)
        .default_width(600.0)
        .default_height(640.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.search)
                        .hint_text("Search knobs (key, group, table)…")
                        .desired_width(220.0),
                );
                if ui
                    .button("Regenerate from intent")
                    .on_hover_text(
                        "Discard these edits and have the engine re-emit the \
                         config from the intent cards",
                    )
                    .clicked()
                {
                    actions.regenerate = true;
                }
                if ui
                    .button("Discard custom config")
                    .on_hover_text("Back to the pure intent plan (the wizard writes the config)")
                    .clicked()
                {
                    actions.discard = true;
                }
            });

            // Engine verdict banner — the authority, verbatim.
            match (&state.resolve, state.resolving, state.dirty_at.is_some()) {
                (_, _, true) | (_, true, _) => {
                    ui.colored_label(theme().text_weak, "validating with the engine…");
                }
                (Some(Ok(report)), _, _) => {
                    ui.colored_label(
                        theme().live,
                        format!(
                            "engine accepted — {} automatic resolutions, {} warnings",
                            report.automatic_resolutions.len(),
                            report.warnings.len()
                        ),
                    );
                }
                (Some(Err(error)), _, _) => {
                    egui::ScrollArea::vertical()
                        .id_salt("refusal")
                        .max_height(84.0)
                        .show(ui, |ui| {
                            ui.colored_label(theme().alert, error);
                        });
                }
                (None, _, _) => {
                    ui.colored_label(theme().text_weak, "engine verdict pending");
                }
            }
            if state.is_modified() {
                ui.colored_label(
                    theme().warn,
                    "custom (experimental) — edits diverge from the profile's \
                     coherent set; the engine remains the validity authority",
                );
            }
            ui.separator();

            let search = state.search.to_lowercase();
            // Group → entry indices, preserving GROUP_ORDER.
            let mut grouped: Vec<(&'static str, Vec<usize>)> = GROUP_ORDER
                .iter()
                .map(|group| (*group, Vec::new()))
                .collect();
            for (index, entry) in state.model.entries.iter().enumerate() {
                let (group, hint) = group_for(&entry.table, &entry.key);
                if !search.is_empty() {
                    let haystack =
                        format!("{} {} {} {}", entry.key, entry.table_label, group, hint)
                            .to_lowercase();
                    if !haystack.contains(&search) {
                        continue;
                    }
                }
                if let Some(slot) = grouped.iter_mut().find(|(name, _)| *name == group) {
                    slot.1.push(index);
                }
            }

            egui::ScrollArea::vertical()
                .id_salt("knobs")
                .show(ui, |ui| {
                    let mut pending_edit: Option<(usize, String)> = None;
                    for (group, indices) in &grouped {
                        if indices.is_empty() {
                            continue;
                        }
                        let has_error = indices.iter().any(|index| {
                            refusal_line_for_entry(state, &state.model.entries[*index]).is_some()
                        });
                        egui::CollapsingHeader::new(*group)
                            .default_open(!search.is_empty() || has_error)
                            .show(ui, |ui| {
                                if *group == "Radiation (profile-governed)" {
                                    ui.label(
                                        egui::RichText::new(
                                            "Coherent radiation sets come from the physics \
                                             profile picker (Physics card). These exact \
                                             values are editable; edits make the run \
                                             custom (experimental).",
                                        )
                                        .color(theme().text_weak)
                                        .size(10.0),
                                    );
                                }
                                for index in indices {
                                    let entry = state.model.entries[*index].clone();
                                    let (_, hint) = group_for(&entry.table, &entry.key);
                                    let label = if entry.table.starts_with("domain[") {
                                        format!("{} · {}", entry.table_label, entry.key)
                                    } else {
                                        entry.key.clone()
                                    };
                                    let multiline =
                                        entry.value.contains('\n') || entry.value.len() > 48;
                                    let mut value = entry.value.clone();
                                    let changed = kit::row(ui, &label, |ui| {
                                        let edit = if multiline {
                                            egui::TextEdit::multiline(&mut value)
                                                .desired_rows(2)
                                                .desired_width(ui.available_width())
                                                .font(egui::TextStyle::Monospace)
                                        } else {
                                            egui::TextEdit::singleline(&mut value)
                                                .desired_width(ui.available_width())
                                                .font(egui::TextStyle::Monospace)
                                        };
                                        let response = ui.add(edit).on_hover_text(format!(
                                            "[{}] {}{}{}",
                                            entry.table,
                                            entry.key,
                                            if hint.is_empty() { "" } else { " — " },
                                            hint
                                        ));
                                        response.changed()
                                    });
                                    if changed {
                                        pending_edit = Some((*index, value));
                                    }
                                    if let Some(line) = refusal_line_for_entry(state, &entry) {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(line)
                                                    .color(theme().alert)
                                                    .size(10.0),
                                            )
                                            .wrap(),
                                        );
                                    }
                                }
                            });
                    }
                    if let Some((index, value)) = pending_edit {
                        state.model = state.model.with_edit(index, &value);
                        state.dirty_at = Some(Instant::now());
                        state.rev += 1;
                    }
                });
        });
    state.open = open;
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# comment stays\n[experiment]\nname = \"n\"\nrun_seconds = 21600.0\n\n[shared]\nnz = 49\neta_levels = [\n    1.0, 0.5,\n    0.0,\n]\nsmdiv = 0.1\n\n[[domain]]\ngrid_id = 1\ndx = 12000.0\n\n[[domain]]\ngrid_id = 2\ni_parent_start = 52\n";

    #[test]
    fn parse_finds_every_key_with_table_paths_and_spans() {
        let model = ConfigModel::parse(SAMPLE);
        let keys: Vec<(&str, &str)> = model
            .entries
            .iter()
            .map(|entry| (entry.table.as_str(), entry.key.as_str()))
            .collect();
        assert!(keys.contains(&("experiment", "name")));
        assert!(keys.contains(&("shared", "eta_levels")));
        assert!(keys.contains(&("domain[0]", "dx")));
        assert!(keys.contains(&("domain[1]", "i_parent_start")));
        let eta = model
            .entries
            .iter()
            .find(|entry| entry.key == "eta_levels")
            .unwrap();
        assert!(eta.line_end > eta.line_start, "array span detected");
        assert!(eta.value.contains("1.0, 0.5"));
        let nest = model
            .entries
            .iter()
            .find(|entry| entry.key == "i_parent_start")
            .unwrap();
        assert_eq!(nest.table_label, "d02");
    }

    #[test]
    fn edit_replaces_only_the_value_and_keeps_comments() {
        let model = ConfigModel::parse(SAMPLE);
        let index = model
            .entries
            .iter()
            .position(|entry| entry.key == "smdiv")
            .unwrap();
        let edited = model.with_edit(index, "0.12");
        assert!(edited.text.contains("smdiv = 0.12"));
        assert!(edited.text.contains("# comment stays"));
        assert!(edited.text.contains("grid_id = 2"));
        // The multi-line array collapses into one line when edited.
        let eta_index = edited
            .entries
            .iter()
            .position(|entry| entry.key == "eta_levels")
            .unwrap();
        let collapsed = edited.with_edit(eta_index, "[1.0, 0.4, 0.0]");
        assert!(collapsed.text.contains("eta_levels = [1.0, 0.4, 0.0]"));
        assert!(!collapsed.text.contains("1.0, 0.5"));
        // Every other entry survives the round trips.
        assert_eq!(collapsed.entries.len(), model.entries.len());
    }

    #[test]
    fn real_engine_emission_parses_with_domain_groups() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/generated-config.toml");
        let text = std::fs::read_to_string(fixture).unwrap();
        let model = ConfigModel::parse(&text);
        // The engine's own emission: both domains, the vertical set, the
        // damping block all become editable entries.
        assert!(
            model
                .entries
                .iter()
                .any(|e| e.table == "domain[1]" && e.key == "i_parent_start")
        );
        assert!(model.entries.iter().any(|e| e.key == "eta_levels"));
        assert!(model.entries.iter().any(|e| e.key == "damp_opt"));
        assert!(model.entries.iter().any(|e| e.key == "epssm"));
        assert!(model.entries.iter().any(|e| e.table == "case_data"));
        // Nothing is omitted: every non-comment `key = value` line is an
        // entry (spot check by count floor).
        assert!(model.entries.len() > 60, "{}", model.entries.len());
    }

    #[test]
    fn placement_write_targets_the_right_domain_and_flags_manual() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/generated-config.toml");
        let base = ConfigModel::parse(&std::fs::read_to_string(fixture).unwrap());
        // grid_id → table order.
        assert_eq!(base.domain_index_for_grid(1), Some(0));
        assert_eq!(base.domain_index_for_grid(2), Some(1));
        assert_eq!(base.domain_index_for_grid(7), None);
        assert_eq!(
            base.domain_value(1, "i_parent_start").map(str::trim),
            Some("52")
        );
        assert_eq!(base_placement(&base, 1), Some((52, 42, 408, 320)));
        assert!(!placement_is_manual(&base, &base, 1));

        // A move edit: only that domain's anchor changes; every other
        // byte (comments, the root domain, physics) survives.
        let moved = base.with_placement(1, 20, 95, None);
        assert_eq!(
            moved.domain_value(1, "i_parent_start").map(str::trim),
            Some("20")
        );
        assert_eq!(
            moved.domain_value(1, "j_parent_start").map(str::trim),
            Some("95")
        );
        assert_eq!(moved.domain_value(1, "nx").map(str::trim), Some("408"));
        assert_eq!(
            moved.domain_value(0, "i_parent_start").map(str::trim),
            Some("1")
        );
        assert!(moved.text.contains("# Emitted by `gpuwm domain`"));
        assert!(placement_is_manual(&moved, &base, 1));
        assert!(!placement_is_manual(&moved, &base, 0));

        // A resize edit carries nx/ny too; reset-to-fitted restores the
        // base placement bit-for-bit at the placement keys.
        let resized = moved.with_placement(1, 20, 95, Some((360, 280)));
        assert_eq!(resized.domain_value(1, "nx").map(str::trim), Some("360"));
        assert_eq!(resized.domain_value(1, "ny").map(str::trim), Some("280"));
        let (i, j, nx, ny) = base_placement(&base, 1).unwrap();
        let reset = resized.with_placement(1, i, j, Some((nx, ny)));
        assert!(!placement_is_manual(&reset, &base, 1));
        assert_eq!(
            reset.domain_value(1, "i_parent_start").map(str::trim),
            Some("52")
        );
    }

    /// Representative engine clearance refusal used by the repair ladder tests.
    const CLEARANCE_REFUSAL: &str = "child domain grid_id = 2 (d02) violates the \
        parent-row clearance rule: west-east high clearance is -33 parent rows \
        but spec_bdy_width + blend_width = 5 + 5 = 10 rows are required.";

    #[test]
    fn carry_children_clamps_refits_or_leaves_for_the_engine() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/generated-config.toml");
        let base = ConfigModel::parse(&std::fs::read_to_string(fixture).unwrap());
        // Clearance from the config's own keys = spec_bdy 5 + blend 5.
        assert_eq!(model_clearance_rows(&base), Some(10.0));

        // Rung 1 — still fits: shrinking 204 → 190 leaves d02 (52..154
        // of 190 − 10) legal; every byte survives, no notices.
        let shrunk = base.with_root_geometry(35.2, -97.4, 190, 162);
        let (kept, repairs) = carry_children(&shrunk, &base, 10.0);
        assert!(repairs.is_empty(), "{repairs:?}");
        assert_eq!(kept.text, shrunk.text, "byte-preserving when children fit");

        // Rung 2 — anchor clamp: root 150 → d02's east limit is
        // floor(150 − 10 − 407/4) = 38; the anchor comes along, size kept.
        let shrunk = base.with_root_geometry(35.2, -97.4, 150, 162);
        let (carried, repairs) = carry_children(&shrunk, &base, 10.0);
        assert_eq!(
            carried.domain_value(1, "i_parent_start").map(str::trim),
            Some("38")
        );
        assert_eq!(carried.domain_value(1, "nx").map(str::trim), Some("408"));
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].grid_id, 2);
        assert!(
            repairs[0].notice.contains("carried along"),
            "{}",
            repairs[0].notice
        );

        // Rung 3 — refit: d02 manually blown up to 600 wide cannot fit
        // a 150-wide root at any anchor, but the emission's 408 can —
        // it refits with the coordinator's notice.
        let oversized = base.with_placement(1, 52, 42, Some((600, 320)));
        let shrunk = oversized.with_root_geometry(35.2, -97.4, 150, 162);
        let (refit, repairs) = carry_children(&shrunk, &base, 10.0);
        assert_eq!(refit.domain_value(1, "nx").map(str::trim), Some("408"));
        assert_eq!(
            refit.domain_value(1, "i_parent_start").map(str::trim),
            Some("38")
        );
        assert_eq!(repairs.len(), 1);
        assert!(
            repairs[0]
                .notice
                .contains("refit — no longer fit at 600×320"),
            "{}",
            repairs[0].notice
        );

        // Rung 4 — SHRINK TO FIT: a 110-wide root cannot host even the
        // fitted 408, so the child resizes to the largest legal
        // ratio-multiple span (356 = 4·89) with the anchor clamped in.
        let shrunk = base.with_root_geometry(35.2, -97.4, 110, 162);
        let (resized, repairs) = carry_children(&shrunk, &base, 10.0);
        assert_eq!(resized.domain_value(1, "nx").map(str::trim), Some("356"));
        assert_eq!(resized.domain_value(1, "ny").map(str::trim), Some("320"));
        assert_eq!(
            resized.domain_value(1, "i_parent_start").map(str::trim),
            Some("11")
        );
        assert_eq!(
            resized.domain_value(1, "j_parent_start").map(str::trim),
            Some("42")
        );
        assert_eq!(repairs.len(), 1);
        assert!(
            repairs[0].notice.contains("shrunk to fit"),
            "{}",
            repairs[0].notice
        );
    }

    /// The -74 clearance regression: a footprint-true 52×31 twelve-km root
    /// around the emission's (52, 42) 408×320 child. No
    /// clamp and no base-refit can host a 408-wide child in a 52-wide
    /// parent; the SHRINK rung must produce the legal 124×40 at
    /// (11, 11) instead of leaving an engine-refused config.
    #[test]
    fn clearance_minus_74_shape_shrinks_the_child_to_fit() {
        let base_text = "[[domain]]\ngrid_id = 1\nparent_id = 0\n\
i_parent_start = 1\nj_parent_start = 1\nparent_grid_ratio = 1\n\
nx = 204\nny = 162\ndx = 12000.0\n\n\
[[domain]]\ngrid_id = 2\nparent_id = 1\ni_parent_start = 52\n\
j_parent_start = 42\nparent_grid_ratio = 4\nnx = 408\nny = 320\n";
        let base = ConfigModel::parse(base_text);
        // The footprint-true manual root exactly as his artifact holds it.
        let stranded = base.with_placement(0, 1, 1, Some((52, 31)));
        let (repaired, repairs) = carry_children_with_floor(&stranded, &base, 10.0, Some(12));
        assert_eq!(repaired.domain_value(1, "nx").map(str::trim), Some("124"));
        assert_eq!(repaired.domain_value(1, "ny").map(str::trim), Some("40"));
        assert_eq!(
            repaired.domain_value(1, "i_parent_start").map(str::trim),
            Some("11")
        );
        assert_eq!(
            repaired.domain_value(1, "j_parent_start").map(str::trim),
            Some("11")
        );
        assert_eq!(repairs.len(), 1);
        assert!(
            repairs[0].notice.contains("shrunk to fit — 124×40"),
            "{}",
            repairs[0].notice
        );
        // The repaired placement satisfies the clearance rule the
        // engine's -74 sentence named: hi clearance ≥ 0 both axes.
        // (52 − 11 − 123/4) − 10 = 0.25 ≥ 0.
        // Truly-impossible parents (below the nest-span floor) are
        // still left for the engine, said out loud.
        let hopeless = base.with_placement(0, 1, 1, Some((22, 22)));
        let (_, repairs) = carry_children_with_floor(&hopeless, &base, 10.0, Some(12));
        assert!(
            repairs[0].notice.contains("engine's refusal"),
            "{}",
            repairs[0].notice
        );
    }

    #[test]
    fn refusal_parser_names_the_offending_domains() {
        assert_eq!(
            refusal_named_domains(CLEARANCE_REFUSAL),
            vec![(2, CLEARANCE_REFUSAL.to_string())]
        );
        let two = format!("{CLEARANCE_REFUSAL}\nchild domain grid_id = 3 too small");
        let named = refusal_named_domains(&two);
        assert_eq!(named.len(), 2);
        assert_eq!(named[1].0, 3);
        assert!(refusal_named_domains("history_interval_s must divide dt").is_empty());
        // The same grid named twice collapses to one flag.
        let twice = format!("{CLEARANCE_REFUSAL}\ngrid_id = 2 again");
        assert_eq!(refusal_named_domains(&twice).len(), 1);
    }

    #[test]
    fn root_geometry_write_moves_projection_and_size_only() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/generated-config.toml");
        let base = ConfigModel::parse(&std::fs::read_to_string(fixture).unwrap());
        assert_eq!(base.root_domain_index(), Some(0));

        // THE DRAWN RECTANGLE IS THE DOMAIN: a 2:1 root written through
        // the root-geometry writer lands center + size, nothing else.
        let drawn = base.with_root_geometry(36.75, -99.125, 340, 170);
        assert!(drawn.text.contains("ref_lat = 36.7500"), "{}", drawn.text);
        assert!(drawn.text.contains("ref_lon = -99.1250"));
        assert!(drawn.text.contains("stand_lon = -99.1250"));
        assert_eq!(drawn.domain_value(0, "nx").map(str::trim), Some("340"));
        assert_eq!(drawn.domain_value(0, "ny").map(str::trim), Some("170"));
        // Truelats, children, comments: byte-untouched.
        assert!(drawn.text.contains("truelat1 = 25.2"));
        assert_eq!(drawn.domain_value(1, "nx").map(str::trim), Some("408"));
        assert_eq!(
            drawn.domain_value(1, "i_parent_start").map(str::trim),
            Some("52")
        );
        assert!(drawn.text.contains("# Emitted by `gpuwm domain`"));
        // The root reads MANUAL; reset-to-fitted is the road back to the
        // engine's own VRAM fit.
        assert!(placement_is_manual(&drawn, &base, 0));
        let (i, j, nx, ny) = base_placement(&base, 0).unwrap();
        let reset = drawn.with_placement(0, i, j, Some((nx, ny)));
        assert!(!placement_is_manual(&reset, &base, 0));
        assert_eq!(reset.domain_value(0, "nx").map(str::trim), Some("204"));
    }

    /// Live-proof harness (ignored; run by hand): apply the REAL
    /// placement writer to an ENGINE-EMITTED config and save the result
    /// for external `--resolve` validation — the running engine is the
    /// judge of every anchor the map can produce.
    ///
    /// env: ARWEN_OFFCENTRE_SOURCE (engine-emitted TOML),
    ///      ARWEN_OFFCENTRE_DEST (output path),
    ///      ARWEN_OFFCENTRE_MOVES ("grid:i:j[;grid:i:j...]").
    #[test]
    #[ignore]
    fn emit_offcentre_placements_for_external_engine_validation() {
        // Hand harness: self-skips (with a reason) so the one-command
        // matrix (--include-ignored) stays a truthful green without the
        // env contract set.
        let (Ok(source), Ok(dest), Ok(moves)) = (
            std::env::var("ARWEN_OFFCENTRE_SOURCE"),
            std::env::var("ARWEN_OFFCENTRE_DEST"),
            std::env::var("ARWEN_OFFCENTRE_MOVES"),
        ) else {
            eprintln!("skipped: set ARWEN_OFFCENTRE_SOURCE/DEST/MOVES to run this hand harness");
            return;
        };
        let mut model = ConfigModel::parse(&std::fs::read_to_string(&source).unwrap());
        for spec in moves.split(';') {
            let parts: Vec<i64> = spec
                .split(':')
                .map(|part| part.trim().parse().unwrap())
                .collect();
            let [grid_id, i, j] = parts[..] else {
                panic!("move spec must be grid:i:j, got {spec}");
            };
            let index = model
                .domain_index_for_grid(grid_id as u32)
                .unwrap_or_else(|| panic!("no [[domain]] with grid_id {grid_id}"));
            model = model.with_placement(index, i, j, None);
        }
        std::fs::write(&dest, &model.text).unwrap();
        eprintln!("wrote {dest}");
    }

    #[test]
    fn key_mention_matching_is_word_bounded() {
        assert!(line_mentions_key(
            "history_interval_s = 47 s on domain",
            "history_interval_s"
        ));
        assert!(!line_mentions_key(
            "nest_history_interval_s = 47",
            "history_interval_s"
        ));
        assert!(line_mentions_key("got damp_opt 7", "damp_opt"));
        assert!(!line_mentions_key("dxdy mismatch", "dx"));
    }
}
