//! Severe / thermodynamic diagnostics for POST-PROCESSED WRF, via wrf-core's
//! `met` kernels.
//!
//! Post-processed climate wrfout files (NCAR CONUS-I/II, GDEX d612005) carry
//! derived `TK` / `Z` / `P` / `QVAPOR` + staggered winds but NOT the raw
//! `T` / `PB` / `PH` / `PHB` planes `wrf_core::WrfFile` needs, so the heavy
//! getvar diagnostics can never run on them. Both import paths short-circuit
//! to [`crate::local_import::try_postprocessed_wrf`]'s shared reader, which —
//! before this module — produced only the isobaric sounding volumes and five
//! synthesized surface fields: a GDEX "future" file imported with zero severe
//! parameters. This module rebuilds the severe suite from the model-level
//! column state the reader already holds, using the same `wrf_core::met`
//! kernels the heavy path's diagnostics are built on (NOT sharprs).
//!
//! Every field is emitted under the exact store slug + units the heavy
//! `getvar` path writes (`wrf_process::wrf_product_slug`, wrf-core
//! `variables.rs`), so labels and Solar color styles light up identically.
//!
//! Documented approximations vs the heavy raw-wrfout path (all inputs the
//! post-processed files simply do not carry):
//! - Surface parcel state (`psfc`/`t2`/`q2`) is the lowest model level — the
//!   files ship no PSFC/T2/Q2. Same approximation as the synthesized 2 m/10 m
//!   surface fields in `local_import`.
//! - Height AGL uses the lowest model level's geopotential height as the
//!   terrain proxy (no HGT variable); the lowest half-level sits ~25 m above
//!   ground on CONUS-II, so layer depths are accurate to that offset.
//! - Winds are grid-relative (no earth rotation) with no 10 m prepend. SRH
//!   and bulk-shear magnitudes are rotation-invariant, so only the missing
//!   near-surface point matters, and the lowest model level stands in for it.
//! - `scp` feeds 0-3 km SRH and 0-6 km bulk shear in place of the
//!   effective-layer SRH/EBWD the heavy path computes (the ~375-line
//!   effective-layer machinery is private to wrf-core's `diag` module).
//! - `stp` is the simple fixed-layer `met::composite::compute_stp` flavor
//!   (no LCL/shear clamp thresholds of the heavy `stp_fixed`).
//! - `ehi` matches the heavy default: SBCAPE x 0-1 km SRH / 160000.
//!
//! Memory discipline (docs/wrf-import-large-grids.md): this module allocates
//! NO full-3D arrays — it borrows the reader's existing buffers (converted in
//! place by the caller) and produces only `ny*nx` 2-D planes. The kernels'
//! rayon parallelism runs in a private pool whose workers drop to
//! below-normal priority, matching the import-worker rule for the owner's
//! machine.

use rayon::prelude::*;
use wrf_core::met::composite as met;
use wrf_core::met::thermo;

/// Model-level column state for one post-processed hour. All 3-D slices are
/// `[nz * cells]` level-major (index `level * cells + cell`, level 0 nearest
/// the ground); 2-D slices are `[cells]`. Units are exactly what
/// `try_postprocessed_wrf_shared` holds after its in-place conversions.
pub(crate) struct SevereInputs<'a> {
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    /// Full pressure, Pa.
    pub pressure_pa: &'a [f64],
    /// Full pressure, hPa (the reader already keeps this for iso interp).
    pub pressure_hpa: &'a [f64],
    /// Temperature, degrees C (converted in place from TK by the caller).
    pub temperature_c: &'a [f64],
    /// Water-vapor mixing ratio, kg/kg.
    pub qvapor: &'a [f64],
    /// Height above ground, m (converted in place from MSL by the caller;
    /// terrain proxy = lowest model level).
    pub height_agl_m: &'a [f64],
    /// Destaggered mass-point winds, m/s (grid-relative).
    pub u_ms: &'a [f64],
    pub v_ms: &'a [f64],
    /// Surface parcel state synthesized from the lowest model level:
    /// pressure Pa, temperature K, mixing ratio kg/kg.
    pub psfc_pa: &'a [f64],
    pub t2_k: &'a [f64],
    pub q2_kgkg: &'a [f64],
}

/// One computed 2-D severe field on the full model grid. `name` is the store
/// slug the heavy getvar path uses (so it inherits the shipped label + Solar
/// style); `units` matches wrf-core `variables.rs` for that product.
pub(crate) struct SevereField {
    pub name: &'static str,
    pub units: &'static str,
    pub values: Vec<f32>,
}

impl SevereInputs<'_> {
    /// Shape check: every 3-D slice `nz*cells`, every 2-D slice `cells`.
    fn shapes_ok(&self, cells: usize) -> bool {
        let n3 = self.nz.checked_mul(cells);
        let Some(n3) = n3 else { return false };
        [
            self.pressure_pa.len(),
            self.pressure_hpa.len(),
            self.temperature_c.len(),
            self.qvapor.len(),
            self.height_agl_m.len(),
            self.u_ms.len(),
            self.v_ms.len(),
        ]
        .iter()
        .all(|len| *len == n3)
            && [self.psfc_pa.len(), self.t2_k.len(), self.q2_kgkg.len()]
                .iter()
                .all(|len| *len == cells)
    }
}

/// Compute the severe suite for one post-processed hour: SB/ML/MU CAPE + CIN,
/// LCL/LFC/EL heights, 0-1/0-3 km SRH, 0-1/0-6 km bulk shear, and the
/// STP/SCP/EHI composites. Returns one full-grid plane per slug (16 fields),
/// or empty on a degenerate grid (< 5 levels, zero cells, shape mismatch).
///
/// `progress` receives one line per kernel stage — on a CONUS-II grid each
/// parcel-lift pass is legitimately tens of seconds, and the dock shows the
/// latest line while the import runs.
pub(crate) fn compute(
    inputs: &SevereInputs<'_>,
    progress: &mut dyn FnMut(String),
) -> Vec<SevereField> {
    let (nx, ny, nz) = (inputs.nx, inputs.ny, inputs.nz);
    let Some(cells) = nx.checked_mul(ny) else {
        return Vec::new();
    };
    if cells == 0 || nz < 5 || !inputs.shapes_ok(cells) {
        return Vec::new();
    }
    let started = std::time::Instant::now();

    // Private rayon pool: the met kernels parallelize over columns with the
    // GLOBAL pool, whose workers run at normal priority. A parcel lift per
    // column over ~2M columns is exactly the all-core memory-bandwidth load
    // the import-priority rule exists for, so run every kernel inside a pool
    // whose workers drop themselves to below-normal priority first. If the
    // pool can't be built, fall back to the global pool rather than skip.
    let pool = rayon::ThreadPoolBuilder::new()
        .thread_name(|index| format!("postproc-severe-{index}"))
        .start_handler(|_| crate::wrf_process::lower_import_thread_priority())
        .build()
        .ok();

    let mut stage = |what: &str| {
        progress(format!("severe suite [{:.0?}]: {what}", started.elapsed()));
    };

    // Thermodynamics: one parcel-lift pass per parcel type. LCL/LFC come from
    // the "sb" pass, matching the heavy path's `lcl`/`lfc` default parcel.
    stage("surface-based CAPE/CIN/LCL/LFC");
    let (sbcape, sbcin, lcl, lfc) = in_pool(&pool, || cape_cin(inputs, "sb"));
    stage("mixed-layer CAPE/CIN");
    let (mlcape, mlcin, _, _) = in_pool(&pool, || cape_cin(inputs, "ml"));
    stage("most-unstable CAPE/CIN");
    let (mucape, mucin, _, _) = in_pool(&pool, || cape_cin(inputs, "mu"));

    stage("equilibrium level");
    let el = in_pool(&pool, || compute_el_grid(inputs, cells));

    // Kinematics. SRH/shear magnitudes are rotation-invariant, so the
    // grid-relative winds are equivalent to earth-rotated ones here.
    stage("storm-relative helicity 0-1 km / 0-3 km");
    let (srh1, srh3) = in_pool(&pool, || srh_pair(inputs));
    stage("bulk shear 0-1 km / 0-6 km");
    let (shear1, shear6) = in_pool(&pool, || shear_pair(inputs));

    // Composites from the 2-D ingredients (cheap, no pool needed).
    // stp: fixed-layer SBCAPE/LCL/SRH1/BWD6. scp: MUCAPE with 0-3 km SRH +
    // 0-6 km shear standing in for effective SRH/EBWD (approximation, see
    // module docs). ehi: SBCAPE x 0-1 km SRH (heavy-path default depth).
    stage("composites STP/SCP/EHI");
    let stp = met::compute_stp(&sbcape, &lcl, &srh1, &shear6);
    let scp = met::compute_scp(&mucape, &srh3, &shear6);
    let ehi = met::compute_ehi(&sbcape, &srh1);

    stage("done");
    [
        ("sbcape", "J/kg", sbcape),
        ("sbcin", "J/kg", sbcin),
        ("mlcape", "J/kg", mlcape),
        ("mlcin", "J/kg", mlcin),
        ("mucape", "J/kg", mucape),
        ("mucin", "J/kg", mucin),
        ("lcl", "m", lcl),
        ("lfc", "m", lfc),
        ("el", "m", el),
        ("srh_0_1km", "m2/s2", srh1),
        ("srh_0_3km", "m2/s2", srh3),
        ("bulk_shear_0_1km", "m/s", shear1),
        ("bulk_shear_0_6km", "m/s", shear6),
        ("stp", "dimensionless", stp),
        ("scp", "dimensionless", scp),
        ("ehi", "dimensionless", ehi),
    ]
    .into_iter()
    .map(|(name, units, values)| SevereField {
        name,
        units,
        values: clean_plane(values),
    })
    .collect()
}

/// Run a kernel inside the below-normal-priority pool when it exists, or on
/// the global pool as a fallback (the kernels parallelize internally either
/// way — this only chooses which workers they run on).
fn in_pool<R: Send>(pool: &Option<rayon::ThreadPool>, f: impl FnOnce() -> R + Send) -> R {
    match pool {
        Some(pool) => pool.install(f),
        None => f(),
    }
}

/// One `met::compute_cape_cin` pass over the grid for the given parcel type.
fn cape_cin(
    inputs: &SevereInputs<'_>,
    parcel_type: &str,
) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
    met::compute_cape_cin(
        inputs.pressure_pa,
        inputs.temperature_c,
        inputs.qvapor,
        inputs.height_agl_m,
        inputs.psfc_pa,
        inputs.t2_k,
        inputs.q2_kgkg,
        inputs.nx,
        inputs.ny,
        inputs.nz,
        parcel_type,
    )
}

fn srh_pair(inputs: &SevereInputs<'_>) -> (Vec<f64>, Vec<f64>) {
    let srh1 = met::compute_srh_with_pressure(
        inputs.u_ms,
        inputs.v_ms,
        inputs.height_agl_m,
        inputs.pressure_hpa,
        inputs.nx,
        inputs.ny,
        inputs.nz,
        1000.0,
    );
    let srh3 = met::compute_srh_with_pressure(
        inputs.u_ms,
        inputs.v_ms,
        inputs.height_agl_m,
        inputs.pressure_hpa,
        inputs.nx,
        inputs.ny,
        inputs.nz,
        3000.0,
    );
    (srh1, srh3)
}

fn shear_pair(inputs: &SevereInputs<'_>) -> (Vec<f64>, Vec<f64>) {
    let shear1 = met::compute_shear(
        inputs.u_ms,
        inputs.v_ms,
        inputs.height_agl_m,
        inputs.nx,
        inputs.ny,
        inputs.nz,
        0.0,
        1000.0,
    );
    let shear6 = met::compute_shear(
        inputs.u_ms,
        inputs.v_ms,
        inputs.height_agl_m,
        inputs.nx,
        inputs.ny,
        inputs.nz,
        0.0,
        6000.0,
    );
    (shear1, shear6)
}

/// Equilibrium-level height (m AGL) for every column: the parallel port of
/// wrf-core `diag::cape::compute_el` (surface-based parcel, 0.0 where the
/// column has no EL — the heavy path's convention), built on the public
/// `met::thermo::el` + `get_height_at_pres` kernels.
fn compute_el_grid(inputs: &SevereInputs<'_>, cells: usize) -> Vec<f64> {
    let nz = inputs.nz;
    (0..cells)
        .into_par_iter()
        .map_init(
            || {
                (
                    Vec::with_capacity(nz),
                    Vec::with_capacity(nz),
                    Vec::with_capacity(nz),
                    Vec::with_capacity(nz),
                )
            },
            |(p_prof, t_prof, td_prof, h_prof), ij| {
                p_prof.clear();
                t_prof.clear();
                td_prof.clear();
                h_prof.clear();
                for k in 0..nz {
                    let idx = k * cells + ij;
                    let p = inputs.pressure_hpa[idx];
                    p_prof.push(p);
                    t_prof.push(inputs.temperature_c[idx]);
                    td_prof.push(met::dewpoint_from_q(inputs.qvapor[idx], p));
                    h_prof.push(inputs.height_agl_m[idx]);
                }

                // Surface-based parcel from the synthesized 2 m state, same
                // normalization as diag::cape::surface_parcel_from_2m.
                let psfc_hpa = inputs.psfc_pa[ij] / 100.0;
                let t2_c = inputs.t2_k[ij] - 273.15;
                let td2_c = met::dewpoint_from_q(inputs.q2_kgkg[ij], psfc_hpa).min(t2_c);

                // Profile starting at the parcel: parcel point + every model
                // level above it (p < parcel pressure).
                let mut mod_p = vec![psfc_hpa];
                let mut mod_t = vec![t2_c];
                let mut mod_td = vec![td2_c];
                for k in 0..nz {
                    if p_prof[k] < psfc_hpa {
                        mod_p.push(p_prof[k]);
                        mod_t.push(t_prof[k]);
                        mod_td.push(td_prof[k]);
                    }
                }

                if mod_p.len() < 2 {
                    return 0.0;
                }
                match thermo::el(&mod_p, &mod_t, &mod_td) {
                    Some((el_pres, _)) if el_pres > 0.0 => {
                        thermo::get_height_at_pres(el_pres, p_prof, h_prof)
                    }
                    _ => 0.0,
                }
            },
        )
        .collect()
}

/// Narrow a kernel plane to the store's f32 with the same sentinel handling
/// as the heavy path's `wrf_process::clean_values` (non-finite, |v| >= 1e30,
/// and <= -9998 fill values all become NaN).
fn clean_plane(values: Vec<f64>) -> Vec<f32> {
    values
        .into_iter()
        .map(|value| {
            if !value.is_finite() || value.abs() >= 1.0e30 || value <= -9998.0 {
                f32::NAN
            } else {
                value as f32
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replicate one hand-built sounding across a small grid, level-major,
    /// returning every slice `compute` needs. Column entries are
    /// (p_hpa, h_agl_m, t_c, td_c, u_ms, v_ms), surface first.
    struct ColumnGrid {
        nx: usize,
        ny: usize,
        nz: usize,
        p_pa: Vec<f64>,
        p_hpa: Vec<f64>,
        t_c: Vec<f64>,
        qv: Vec<f64>,
        h_agl: Vec<f64>,
        u: Vec<f64>,
        v: Vec<f64>,
        psfc: Vec<f64>,
        t2: Vec<f64>,
        q2: Vec<f64>,
    }

    /// Mixing ratio (kg/kg) from dewpoint (C) and pressure (hPa) — inverse of
    /// `met::dewpoint_from_q`, so the kernels see the intended dewpoints.
    fn q_from_dewpoint(td_c: f64, p_hpa: f64) -> f64 {
        let e = 6.112 * ((17.67 * td_c) / (243.5 + td_c)).exp();
        0.622 * e / (p_hpa - e)
    }

    fn grid_from_column(
        nx: usize,
        ny: usize,
        column: &[(f64, f64, f64, f64, f64, f64)],
    ) -> ColumnGrid {
        let cells = nx * ny;
        let nz = column.len();
        let mut grid = ColumnGrid {
            nx,
            ny,
            nz,
            p_pa: vec![0.0; nz * cells],
            p_hpa: vec![0.0; nz * cells],
            t_c: vec![0.0; nz * cells],
            qv: vec![0.0; nz * cells],
            h_agl: vec![0.0; nz * cells],
            u: vec![0.0; nz * cells],
            v: vec![0.0; nz * cells],
            psfc: vec![0.0; cells],
            t2: vec![0.0; cells],
            q2: vec![0.0; cells],
        };
        for (k, &(p_hpa, h, t_c, td_c, u, v)) in column.iter().enumerate() {
            let q = q_from_dewpoint(td_c, p_hpa);
            for cell in 0..cells {
                let idx = k * cells + cell;
                grid.p_pa[idx] = p_hpa * 100.0;
                grid.p_hpa[idx] = p_hpa;
                grid.t_c[idx] = t_c;
                grid.qv[idx] = q;
                grid.h_agl[idx] = h;
                grid.u[idx] = u;
                grid.v[idx] = v;
            }
        }
        let (p0, _, t0, td0, _, _) = column[0];
        for cell in 0..cells {
            grid.psfc[cell] = p0 * 100.0;
            grid.t2[cell] = t0 + 273.15;
            grid.q2[cell] = q_from_dewpoint(td0, p0);
        }
        grid
    }

    fn inputs(grid: &ColumnGrid) -> SevereInputs<'_> {
        SevereInputs {
            nx: grid.nx,
            ny: grid.ny,
            nz: grid.nz,
            pressure_pa: &grid.p_pa,
            pressure_hpa: &grid.p_hpa,
            temperature_c: &grid.t_c,
            qvapor: &grid.qv,
            height_agl_m: &grid.h_agl,
            u_ms: &grid.u,
            v_ms: &grid.v,
            psfc_pa: &grid.psfc,
            t2_k: &grid.t2,
            q2_kgkg: &grid.q2,
        }
    }

    fn field<'a>(fields: &'a [SevereField], slug: &str) -> &'a SevereField {
        fields
            .iter()
            .find(|field| field.name == slug)
            .unwrap_or_else(|| panic!("missing field {slug}"))
    }

    fn first_finite(fields: &[SevereField], slug: &str) -> f32 {
        field(fields, slug)
            .values
            .iter()
            .copied()
            .find(|value| value.is_finite())
            .unwrap_or_else(|| panic!("field {slug} entirely NaN"))
    }

    /// A classic unstable, veering, strongly-sheared supercell sounding must
    /// yield positive CAPE (SB and MU), negative-or-zero CIN, positive
    /// right-mover SRH, deep-layer shear, an EL above the LCL, and finite
    /// positive composites — proving the full column -> wrf-core met kernel
    /// -> slug chain end to end.
    #[test]
    fn supercell_column_produces_positive_severe_suite() {
        // (p hPa, h m AGL, T C, Td C, u m/s, v m/s), surface first. Winds
        // veer surface -> 6 km with ~35 m/s of deep shear.
        let column = [
            (1000.0, 0.0, 30.0, 23.0, -2.0, 2.0),
            (925.0, 700.0, 24.0, 20.0, 0.0, 8.0),
            (850.0, 1400.0, 20.0, 17.0, 4.0, 14.0),
            (700.0, 3100.0, 9.0, 4.0, 12.0, 18.0),
            (500.0, 5800.0, -8.0, -18.0, 24.0, 14.0),
            (400.0, 7500.0, -20.0, -32.0, 30.0, 8.0),
            (300.0, 9600.0, -38.0, -50.0, 34.0, 2.0),
            (250.0, 10800.0, -48.0, -62.0, 36.0, 0.0),
            (200.0, 12300.0, -55.0, -70.0, 34.0, -4.0),
        ];
        let grid = grid_from_column(3, 3, &column);
        let fields = compute(&inputs(&grid), &mut |_| {});
        assert_eq!(fields.len(), 16, "one plane per emitted slug");
        for field in &fields {
            assert_eq!(
                field.values.len(),
                9,
                "{}: plane must cover the full grid",
                field.name
            );
        }

        let sbcape = first_finite(&fields, "sbcape");
        assert!(sbcape > 500.0, "unstable column: SBCAPE {sbcape}");
        let mucape = first_finite(&fields, "mucape");
        assert!(mucape > 500.0, "unstable column: MUCAPE {mucape}");
        let sbcin = first_finite(&fields, "sbcin");
        assert!(sbcin <= 0.0, "CIN must be <= 0, got {sbcin}");
        let mlcin = first_finite(&fields, "mlcin");
        assert!(mlcin <= 0.0, "ML CIN must be <= 0, got {mlcin}");

        let lcl = first_finite(&fields, "lcl");
        assert!(
            (100.0..4000.0).contains(&lcl),
            "warm moist surface: LCL {lcl} m"
        );
        let el = first_finite(&fields, "el");
        assert!(el > lcl, "EL {el} m must sit above the LCL {lcl} m");

        let srh1 = first_finite(&fields, "srh_0_1km");
        assert!(srh1 > 0.0, "veering low-level winds: 0-1 km SRH {srh1}");
        let srh3 = first_finite(&fields, "srh_0_3km");
        assert!(srh3 > srh1, "0-3 km SRH {srh3} should exceed 0-1 km {srh1}");
        let shear6 = first_finite(&fields, "bulk_shear_0_6km");
        assert!(shear6 > 15.0, "deep shear {shear6} m/s");
        let shear1 = first_finite(&fields, "bulk_shear_0_1km");
        assert!(
            shear1 > 0.0 && shear1 < shear6,
            "0-1 km shear {shear1} vs 0-6 km {shear6}"
        );

        for slug in ["stp", "scp", "ehi"] {
            let value = first_finite(&fields, slug);
            assert!(
                value > 0.0,
                "significant-severe environment: {slug} {value}"
            );
        }
    }

    /// A bone-dry stable column must produce zero CAPE/CIN (the kernel's
    /// no-instability convention) and zero composites — no spurious severe
    /// signal over stable air.
    #[test]
    fn stable_column_produces_zero_cape_and_composites() {
        // Isothermal-ish, very dry, calm: nothing to lift.
        let column = [
            (1000.0, 0.0, 5.0, -25.0, 1.0, 1.0),
            (925.0, 700.0, 6.0, -26.0, 1.0, 1.0),
            (850.0, 1400.0, 7.0, -28.0, 2.0, 1.0),
            (700.0, 3100.0, 4.0, -32.0, 2.0, 2.0),
            (500.0, 5800.0, -10.0, -40.0, 3.0, 2.0),
            (300.0, 9600.0, -40.0, -60.0, 4.0, 2.0),
        ];
        let grid = grid_from_column(2, 2, &column);
        let fields = compute(&inputs(&grid), &mut |_| {});
        assert_eq!(fields.len(), 16);
        for slug in ["sbcape", "mlcape", "mucape", "stp", "scp", "ehi"] {
            let value = first_finite(&fields, slug);
            assert_eq!(value, 0.0, "{slug} must be zero for a stable column");
        }
    }

    /// Degenerate inputs (too few levels) yield no fields rather than
    /// panicking or emitting garbage planes.
    #[test]
    fn thin_grid_yields_no_fields() {
        let column = [
            (1000.0, 0.0, 20.0, 15.0, 0.0, 0.0),
            (850.0, 1400.0, 12.0, 8.0, 0.0, 0.0),
        ];
        let grid = grid_from_column(2, 2, &column);
        assert!(compute(&inputs(&grid), &mut |_| {}).is_empty());
    }

    /// Every emitted slug must be one the heavy getvar path already writes
    /// (`wrf_process::wrf_product_slug` maps it to itself), so the shipped
    /// labels and Solar styles apply to the post-processed fields with no new
    /// label/style code.
    #[test]
    fn emitted_slugs_match_heavy_path_slugs() {
        let column = [
            (1000.0, 0.0, 30.0, 23.0, -2.0, 2.0),
            (925.0, 700.0, 24.0, 20.0, 0.0, 8.0),
            (850.0, 1400.0, 20.0, 17.0, 4.0, 14.0),
            (700.0, 3100.0, 9.0, 4.0, 12.0, 18.0),
            (500.0, 5800.0, -8.0, -18.0, 24.0, 14.0),
            (300.0, 9600.0, -38.0, -50.0, 34.0, 2.0),
        ];
        let grid = grid_from_column(2, 2, &column);
        let fields = compute(&inputs(&grid), &mut |_| {});
        assert_eq!(fields.len(), 16);
        for field in &fields {
            assert_eq!(
                crate::wrf_process::wrf_product_slug(field.name),
                Some(field.name),
                "{}: not a heavy-path store slug",
                field.name
            );
        }
    }
}
