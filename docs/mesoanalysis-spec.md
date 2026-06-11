# Mesoanalysis Implementation Spec (verified)
## SCHEMES
# Objective analysis for bowecho: blending surface obs with an HRRR 3-km background
## Implementation spec (Rust, ~5-minute re-analysis cadence)

All equations below were verified against the primary papers (full-text or page scans), not from memory. Local copies of every source live in `C:\Users\drew\radar-work\bowecho\.research_tmp\` (`koch1983.pdf` + page PNGs, `bratseth_p1..9.png` page scans of the Tellus paper, `lazarus2002.pdf/.txt`, `myrick2005.pdf/.txt`, `tyndall2013.pdf/.txt`, `depondeca2011.pdf/.txt`, `bouttier_courtier_clean.txt`). Equation numbers cited are the papers' own.

---

## 0. Bottom line

**Build the Bratseth (1986) successive-correction scheme operating on observation-minus-background increments**, with the ADAS concrete weight form (Lazarus et al. 2002, Eqs. 5–10), Gaussian horizontal x vertical(elevation) correlations, the Myrick–Horel–Lazarus (2005) intervening-terrain term, and per-network observation-error ratios from Tyndall & Horel (2013). It converges to the Optimal Interpolation (OI) solution without ever forming or inverting a matrix, it is embarrassingly parallel, every constant is published, isolated bad clusters are automatically de-weighted, and the analysis relaxes *exactly* to the HRRR background away from data (proof in §6). Keep a two-pass Barnes/Koch mode as the trivial fallback (it is ~60 lines and shares all infrastructure), and treat full 2DVar (RTMA) as a source of ideas to borrow (terrain anisotropy, QC, land/water handling), not something to implement.

Budget on a CONUS window: with ~3,000 obs and a 600x600-cell window, one full Bratseth analysis (10 obs-space iterations + 1–2 gridding passes) is on the order of 10^8 fused multiply-adds — tens of milliseconds multi-core. Even the full 1799x1059 grid is sub-second. Compute is not the constraint; correctness of constants is.

---

## 1. Common foundation (applies to every scheme)

### 1.1 Analyze increments, always
Define, per variable, per station `i`:

```
d_i = y_i - H(x_b)_i        // "innovation": observation minus background interpolated to the station
```

`H` = bilinear interpolation of the HRRR field to the station's projected (col,row) position. The analysis is

```
x_a(grid) = x_b(grid) + delta(grid)      // delta = analyzed increment field
```

and **every scheme below analyzes `d_i` to produce `delta`, never raw observed values**. This is universal practice: Bratseth (1986) defines his entire algebra on `f = F - F^P` (deviation from the prognosis; his p. 439 notation section); RTMA's 2DVar control vector "represents the departure of the estimate of the analysis from the specified background" (De Pondeca et al. 2011, Eq. 1 context); Tyndall & Horel (2013) Eq. (3) is `x_a = x_b + P_b H^T eta`; the BLUE itself is `x_a = x_b + K(y - H[x_b])` (Bouttier & Courtier 1999, Eq. A5). Rationale in §6.

### 1.2 Geometry
- Distances `r` in km, computed in HRRR Lambert grid space: `r = 3.0 km * hypot(dcol, drow)` (map-factor error <1% over CONUS — fine).
- Elevations: station elevation from metadata, `z_grid` from the HRRR terrain (surface HGT) field. Use the *true* station elevation in all `dz` terms — this is what naturally kills a Mount-Rainier-style ob (Tyndall & Horel 2013 explicitly do not adjust `H` for elevation mismatch; the vertical covariance term handles it).
- **Land/water trick (borrowed from RTMA and UU2DVar):** lower the effective elevation of water grid cells (and buoy/ship obs) by **500 m** in the covariance computation. This sharpens the covariance gradient at shorelines so land obs do not contaminate water and vice versa (De Pondeca et al. 2011 §2b; Tyndall & Horel 2013, p. 256: "water analysis grid points have their elevation reduced by 500 m").

### 1.3 QC chain (run before any analysis; all published)
1. Physical range checks per variable.
2. Time window: RTMA accepts obs valid within **±12 min** of analysis time (Kahler & Myrick 2008 preprint; De Pondeca et al. 2011). For a 5-min cadence: latest ob per station no older than ~20 min.
3. **Innovation gross check** (Tyndall & Horel 2013, Eq. 5, verbatim):
   `|y_o - H(x_b)| <= max[ eps_m * stddev(x_b within 40 km), t_qc ]`
   with `eps_m = 10` for all variables and floors `t_qc = 3 C` (T), `4 C` (Td), `7.5 m/s` (wind components and speed). The local-stddev term retains good obs near drylines/terrain where the background itself is highly variable; the floor prevents over-rejection where the background is flat (offshore).
   RTMA's equivalent: reject when `|innovation| / sigma_o` exceeds a per-type threshold (their Table 2 ratios: T 5.0–7.5, pseudo-RH 3–5, psfc 3–5, wind 5–7).
4. **Calm-wind rule** (Tyndall & Horel 2013): discard a wind ob when observed speed < 1 m/s but background speed > 5 m/s (NWS minimum reportable speed is 1.25 m/s; "calm" against a strong background is almost always a siting/reporting artifact).
5. **Dynamic blacklist** (RTMA practice, De Pondeca et al. 2011 §3): a station failing the gross check in the previous 3–6 cycles is excluded from the current one. RTMA flags ~10% of T/Td, ~25% of psfc, ~55% of mesonet wind this way — expect to throw away a lot of mesonet wind.
6. Per-network trust: CWOP/uncalibrated networks get inflated `sigma_o` (see §7), per Tyndall & Horel's 10 network categories.

---

## 2. Scheme 1 — Barnes two-pass with Koch–desJardins–Kocin parameter selection (baseline)

Primary sources: Barnes (1964, 1973); the operational parameterization is Koch, desJardins & Kocin (1983), *J. Climate Appl. Meteor.* **22**, 1487–1503 (all equations below read from the paper scan, pp. 1489–1493).

### 2.1 Equations (Koch et al. numbering)
Weight (Eq. 1) — note: **no** factor 4 in this formulation; kappa has units length²:

```
w_m = exp( - r_m^2 / kappa )
```

`r_m` = distance from grid point to station `m`. The influence radius (where `w = e^-1`) is `R = sqrt(kappa)`.

First pass (part of Eq. 8):

```
g0(i,j) = sum_m[ w_m * f_m ] / sum_m[ w_m ]
```

Second (correction) pass uses a *sharpened* weight (Eqs. 4, 7):

```
kappa_1 = gamma * kappa_0 ,   0.2 <= gamma <= 1.0          (Eq. 4 + p.1492 constraint)
w'_m    = exp( - r_m^2 / (gamma * kappa_0) )                (Eq. 7)
g1(i,j) = g0(i,j) + sum_m[ w'_m * (f_m - g0(x_m,y_m)) ] / sum_m[ w'_m ]   (Eq. 8)
```

`g0(x_m,y_m)` = first-pass field evaluated *at the stations* (Koch: bilinear interpolation from the four surrounding grid points, or evaluate the pass-1 sum directly at station locations — do the latter; it is exact and cheap).

Response functions (Eqs. 2, 6, 10b, 11), for wavelength `lambda`:

```
D0     = exp( - kappa_0 * (pi/lambda)^2 )        // pass 1
D1     = D0^gamma                                // response of the correction weight
D1*    = D0 * (1 + D0^(gamma-1) - D0^gamma)      // cumulative two-pass response (Eq. 11)
```

### 2.2 Parameter selection (the Koch et al. operational method)
- Compute the **average station spacing** `dn_c` = mean over stations of nearest-neighbor distance within the data area (their §3b).
- For non-uniform networks also compute the **random data spacing** (Eq. 12):
  `dn_r = sqrt(A) * (1 + sqrt(M)) / (M - 1)` (`A` = data-area, `M` = station count). For clustered data `dn_r >> dn_c`; choose the working `dn >= dn_c`, using `dn_r` as the guide (practically: `dn = max(dn_c, dn_r)`).
- **Fix kappa from dn** (Eq. 13). Choosing the pass-1 response at the `2*dn` wavelength to be `D0(2dn) = e^-5.052 = 0.0064` (selected so that with `gamma = 0.2` the *final* response at `2dn` is `D1* = 0.375`) gives:

```
kappa_0 = 5.052 * (2*dn / pi)^2
```

  Worked values: `dn = 40 km -> kappa_0 = 3,276 km² (R = 57 km)`; `dn = 60 km -> 7,371 km² (R = 86 km)`; `dn = 100 km -> 20,475 km² (R = 143 km)`.
- **gamma in [0.2, 1.0]**: `gamma = 0.2` maximum detail, `1.0` smoothest. Koch et al.: convergence is rapid, "only two passes through the data are required"; making more passes is explicitly *not* the 1973/1983 scheme. Recommend `gamma = 0.3`.
- **Cutoff radius** (p. 1493, verbatim values): `R_c = sqrt(20 * kappa_0)`, i.e. `R_c / R = 4.5`, where the weight has fallen to `2e-9`. That is generous; `sqrt(10*kappa_0)` (`w = e^-10 ~ 4.5e-5`) is a fine performance cutoff. Flag grid cells influenced by fewer than 3 stations (their warning criterion) — with increments, set the increment to 0 there instead.
- **Grid spacing constraint**: `dx/dn` in ~0.3–0.5 and `dx <= dn/2` ("Δx must be no larger than one-half of 2Δn", p. 1493). On a 3-km background grid analyzing increments you massively oversample — harmless; the constraint only says you can never claim increment detail below `2*dn` (80–200 km for METAR spacing). **This is exactly why the increment approach is mandatory with a 3-km background** (§6).

### 2.3 Assessment
Pros: trivial, deterministic, two passes, well-understood spectral response. Cons (all fixed by Bratseth): no concept of observation error (the analysis draws fully toward obs as gamma shrinks); station clusters dominate the weighted mean (no declustering); no principled background/obs blending — the only smoothing control is the response function. Keep as `AnalysisScheme::Barnes` fallback and for fast preview layers.

---

## 3. Scheme 2 — Bratseth (1986) successive corrections converging to OI (RECOMMENDED)

Primary source: Bratseth, *Tellus* **38A**, 439–447 (1986) — read from page scans. Concrete operational form: Lazarus, Ciliberti, Horel & Brewster (2002), *Wea. Forecasting* **17**, 971–1000 (ADAS), Eqs. (5)–(10); idealized validation and terrain extension: Myrick, Horel & Lazarus (2005), *Wea. Forecasting* **20**, 149–160.

### 3.1 The iteration (exact, as implemented in ADAS)
Work entirely in increment space. Let `d_i` = innovation at station `i`, `f_x(k)` = analyzed increment at grid point `x` after iteration `k`, `f_i(k)` = the *analysis estimate at station i* after iteration `k`. Initialize `f_x(0) = 0`, `f_i(0) = 0`. Each iteration (Lazarus Eqs. 5–6 = Bratseth Eq. 4):

```
f_x(k+1) = f_x(k) + sum_i  a_xi * ( d_i - f_i(k) )       // grid update
f_j(k+1) = f_j(k) + sum_i  a_ji * ( d_i - f_i(k) )       // station-estimate update
```

with weights (Lazarus Eqs. 7–9 = Bratseth Eqs. 15a,b with his M_j suggestion, Eqs. 16–18):

```
a_xi = rho_xi / m_i                          // grid-to-station weight
a_ji = ( rho_ji + eps2_i * delta_ji ) / m_i  // station-to-station weight (note the extra obs-error term on the diagonal)
m_i  = eps2_i + sum_j rho_ji                 // normalization: sum of correlations of station i with ALL stations, plus the error ratio
```

- `rho` = background-error correlation function (§3.3),
- `eps2_i = sigma_o(i)^2 / sigma_b^2` = **ratio of observation-error variance to background-error variance** (per station/network/variable, §7),
- `delta_ji` = Kronecker delta.

The asymmetry between `a_xi` and `a_ji` is the whole trick: the *station estimates* are pulled by `rho + eps2*I` (the full innovation covariance `HBH^T + R` in correlation units), while the *grid* is pulled by `rho` only (`BH^T`). Bratseth proves (his Eqs. 7–11) that the fixed point of this iteration is exactly the OI/BLUE solution: as `k -> inf`,

```
f_x(inf) = b_x^T * (Rho + eps2*I)^{-1} * d      == OI analysis increment
```

The `m_i` normalization is what makes the iteration *converge to* OI instead of merely resembling it (this is Bratseth's §4–5 "Improved successive corrections" and "A suggestion for M_j"). Because `m_i` grows with local station density, **clusters automatically de-weight themselves** — the OI behavior Barnes lacks.

### 3.2 Convergence: guaranteed, and what controls the rate
Bratseth's appendix (p. 446): write the station-space iteration matrix `A = C * D` with `C_ij = rho_ij + eps2*delta_ij` (nonnegative entries) and `D = diag(1/m_i)`. Because `m_i` is exactly the absolute row sum of `C`, Gershgorin's theorem bounds every eigenvalue `mu` of `A` in `[0, 1]`, and positive-definiteness of `C` (guaranteed by Gaussian `rho` + `eps2 > 0` on the diagonal) gives `mu > 0` for all non-redundant modes; hence the error contracts as `(1 - mu)^k` and the scheme **converges monotonically with no overshoot, for any observation distribution**. Zero eigenvalues correspond only to exactly redundant observations and are harmless (Bratseth, Eq. A6 discussion).
- Rate: slow modes are tight clusters of highly correlated stations (his two-ob example: with inter-ob correlation 0.8, ~25 iterations to converge; with 0.5, ~5).
- Operational guidance: Myrick et al. (2005, Fig. 4) — full numerical convergence took ~100 iterations, but **analysis skill saturates at ~10 iterations**, and all published operational applications use ~10 or fewer (their §3 citing Seaman 1988 etc.). ADAS uses 4 passes.
- Engineering safeguard: monitor `max_i |d_i - f_i(k)|`; stop when it falls below `0.05 * sigma_b` or at `k = 12`, whichever first. If it ever *increases* (possible only if a non-positive-definite kernel hack is introduced), halve the correction step — but with the published kernels below this cannot happen.

### 3.3 Correlation model (with terrain)
Base form — Gaussian in horizontal distance and elevation difference (Lazarus Eq. 10 = Myrick Eqs. 1–2 = Tyndall Eq. 4):

```
rho_ij = exp( - r_ij^2 / R^2 ) * exp( - dz_ij^2 / Rz^2 )
```

`r_ij` horizontal distance, `dz_ij` elevation difference (true station elevations for ob-ob pairs; station-vs-HRRR-terrain for grid-ob pairs; water cells lowered 500 m). Hard-zero the correlation beyond `r > 3.5R` (Tyndall zeroes beyond 300 km for R = 80 km) — this gives compact support and exact background relaxation (§6).

**Intervening-terrain term (ITT)** — the published, concrete "valley ob must not contaminate the ridge / next valley" fix (Myrick, Horel & Lazarus 2005, Eqs. 3–4):

```
rho'_ij = rho_ij * exp( - a_ij^2 / RB^2 )
a_ij    = max( 0,  z_t - max(z_i, z_j) )     // terrain blockage
```

where `z_t` = maximum terrain height on the straight line between the two points. **RB = 2000 m** (their value; a 1000-m ridge between two points cuts the correlation by 22%, exp(-0.25) = 0.78). The form is symmetric, so `C` stays symmetric. Implementation: max-pool the HRRR terrain to ~24 km (8x) and sample the decimated line — the ITT only cares about kilometer-scale barriers, and this makes grid-ob blockage affordable (precompute per-station radial "horizon" maps if needed; they only change when the station list changes).

**RTMA-style smooth anisotropy (optional alternative to ITT)**: see §5 — a Mahalanobis distance with the terrain-gradient aspect tensor can replace `exp(-dz^2/Rz^2)` if you want covariances that hug terrain contours rather than just decorrelate across elevation.

### 3.4 Length scales — fixed-R vs schedule
Two published configurations:
- **Fixed scale, iterate to OI** (Myrick 2005; Tyndall & Horel 2013): one `R` reflecting the *background-error* correlation scale — **R = 80 km, Rz = 200 m** estimated for RUC backgrounds by innovation statistics (Tyndall, Horel & De Pondeca 2010, via the Lönnberg & Hollingsworth 1986 method). NOTE the conceptual difference from Barnes: `R` is a property of the background error, *not* of station spacing; density is handled by `m_i`. This is the theoretically clean default. Myrick's idealized complex-terrain test used R = 75 km (also 25 km), Rz = 375 m, eps2 = 0.1.
- **Decreasing-R schedule** (ADAS, Lazarus 2002, Table 1): 4 passes, default R = 200/80/50/40 km with Rz = 500 m; Utah operational (10-km station spacing) R = 100/50/25/12 km, Rz = 1200/600/300/150 m. Sharper detail near dense data, but the fixed-point is no longer a single OI solution (Bratseth himself blesses radius reduction as the graceful way to handle mutually inconsistent obs — p. 443: it yields a smoothed version of the limiting analysis).

**Recommendation for bowecho:** primary = fixed R per variable (§7), 10 iterations. Optional "detail stage": after convergence, run a second short analysis on the residuals `d_i - f_i` with `R2 ~ 30 km, 3 iterations`, only where local station spacing < 30 km (mesonet-dense areas). This is the standard multiscale SCM compromise and mirrors what RTMA later did with multigrid filters.

### 3.5 The big implementation optimization (exact, from Bratseth Eq. 8)
The grid never needs updating inside the loop. Bratseth's general solution:

```
f_x(k+1) = b_x^T * SUM_{m=0..k} ( d - f(m) )
```

i.e. the grid increment equals the grid weights applied to the **accumulated** station-space residuals. So:

```
1. Iterate ONLY in station space (n_obs x n_local per iteration; ~3000 x 500 = 1.5M fma / iter)
   accumulating  s_i += ( d_i - f_i(k) )  each iteration.
2. After convergence, do ONE gridding pass:  delta_x = sum_i a_xi * s_i.
```

This is exact for fixed `R` (weights constant across iterations). With an R-schedule, grid once per pass instead. Gridding is a scatter: for each station, add `a_xi * s_i` over its cutoff disk (tile the window and parallelize over tiles to avoid write contention).

### 3.6 Pseudo-Rust

```rust
struct ObsAnalysis {
    // built once per station-set / config change:
    rho_oo: SparseSym<f32>,   // rho'_ij, cutoff 3.5R, ITT applied
    m: Vec<f32>,              // eps2_i + row_sum(rho_oo)_i   (per variable class)
    grid_lists: Vec<Vec<(u32 /*cell*/, f32 /*rho'_xi*/)>>,  // per-station cutoff disk
}

fn analyze(d: &[f32], a: &ObsAnalysis, n_iter: usize, grid: &mut [f32]) {
    let n = d.len();
    let (mut f, mut s) = (vec![0f32; n], vec![0f32; n]);
    for _ in 0..n_iter {
        // residuals r_i = (d_i - f_i)/m_i, then f_j += sum_i (rho_ji + eps2 δ) r_i
        let r: Vec<f32> = (0..n).map(|i| (d[i] - f[i]) / a.m[i]).collect();
        let upd = a.rho_oo.symv_plus_diag(&r, &a.eps2);   // rayon par over rows
        for i in 0..n { f[i] += upd[i]; s[i] += d[i] - /*old*/ (f[i] - upd[i]); }
        if max_abs_residual(&d, &f) < tol { break; }
    }
    // one gridding pass (scatter, rayon over tiles)
    for (i, list) in a.grid_lists.iter().enumerate() {
        let w = s[i] / a.m[i];
        for &(cell, rho) in list { grid[cell as usize] += rho * w; }
    }
}
```

Sanity invariant (single isolated ob): converges in one iteration to `delta(at ob) = d / (1 + eps2)` — with `eps2 = 1` the analysis moves exactly halfway to the ob, matching Tyndall & Horel's statement that ratio 1.0 makes an isolated analysis "approximate roughly the average of the observation and background".

---

## 4. Scheme 3 — Optimal Interpolation, classic form (reference solver)

Primary: Daley (1991), *Atmospheric Data Analysis*, chs. 4–5; equations verified against Bouttier & Courtier (1999) ECMWF lecture notes (Eqs. A5–A8) and Tyndall & Horel (2013) Eqs. (1)–(3).

```
x_a = x_b + K ( y - H[x_b] )
K   = B H^T ( H B H^T + R )^{-1}
A   = (I - K H) B                      // analysis-error covariance (optimal K)
J(x) = (x-x_b)^T B^{-1} (x-x_b) + (y-H[x])^T R^{-1} (y-H[x])   // equivalent variational form
```

Assumptions: linear(ized) `H`, unbiased and mutually uncorrelated background/observation errors, `B`, `R` positive definite. `B` is modeled exactly as §3.3's `sigma_b^2 * rho'`; `R = diag(sigma_o_i^2)` (obs errors uncorrelated — RTMA also assumes this).

Observation-space algorithm (what Tyndall & Horel's UU2DVar actually runs; ideal when n_obs << n_grid):

```
solve  ( H B H^T + R ) eta = y - H[x_b]     // n_obs x n_obs SPD system
x_a    = x_b + B H^T eta                    // one gridding pass, identical to §3.5's
```

Cost for a few thousand obs: dense Cholesky is `n^3/3` ~ 9 GFLOP at n = 3000 — well under a second with a decent kernel; with the 3.5R cutoff the matrix is sparse and conjugate gradient (each iteration = one sparse symv, the same kernel as Bratseth) converges in a few dozen iterations. Daley's classic "local volume" alternative — for each grid point/box select the ~p nearest obs and solve the p x p system — is what pre-1999 NWP did; Bouttier & Courtier note its drawback verbatim: with pointwise selection "the analysis field will generally not be continuous in space," and box selection only mitigates this. **Do not implement local-volume OI; if you want the exact OI answer, do the global observation-space solve with CG.**

Relationship to recommendation: Bratseth *is* a diagonally-scaled fixed-point solver for this same system, with guaranteed contraction and zero linear-algebra dependencies. Implement OI/CG once behind the same trait as a *verification oracle* (unit tests assert Bratseth-after-50-iterations == CG solution to tolerance; Myrick et al. 2005 demonstrated exactly this equivalence — their Bratseth, OI, and 3DVAR solutions were identical).

---

## 5. Scheme 4 — 2DVar as used by RTMA: what to borrow

Primary: De Pondeca et al. (2011), *Wea. Forecasting* **26**, 593–612.

- Cost function (their Eq. 1): `J(x_k) = 1/2 x_k^T B^{-1} x_k + 1/2 (H x_k - y_o)^T R^{-1} (H x_k - y_o)` with the control vector `x_k` = departure from background (increments again). Preconditioned conjugate gradient, **2 outer loops x 50 inner iterations**. Univariate: GSI balance constraints are switched off. Analysis variables: streamfunction, velocity potential, T, surface pressure, and pseudo-RH (= specific humidity scaled by the background's saturated value; 2-m Td derived afterward).
- `B` is synthesized by anisotropic recursive filters (Purser et al. 2003a,b) — i.e. they never store `B`; they apply it as an operator. The implied autocovariance (their Eq. A1):

```
C(dx) = sigma^2 * exp( -1/2 dx^T S^{-1} dx )
```

- **The terrain-following part — their Eq. (A2), a variant of Riishøjgaard (1998):**

```
S^{-1} = I / Lh^2  +  (grad H)(grad H)^T / Lf^2
```

  `grad H` = local terrain gradient, `Lh` = horizontal correlation scale, `Lf` = "function correlation scale" controlling anisotropy strength: small `Lf` (or steep terrain) shortens the effective correlation length *along* the terrain-gradient direction, so covariance contours hug elevation contours — that is how a valley ob is stopped from contaminating a ridge. Zero terrain gradient recovers isotropy. Worked example with their Table 1 values for temperature (`Lh = 42,834 m`, `Lf = 636 m`): on a 5% slope the effective along-gradient scale collapses from 43 km to `(1/Lh^2 + (0.05/636)^2)^{-1/2} ~ 12 km`.
- Their Table 1 (domain-averaged): T: Lh 42.8 km, Lf 636 m, sigma_b 1.7 K; pseudo-RH: 40.1 km / 636 m / 0.21; psfc: 45.0 km / 636 m / 1.9 hPa; psi/chi (wind): Lh ~64.6 km, Lf 3818 m — i.e. **wind anisotropy is deliberately very weak** ("difficult to justify for errors in the background wind, whose circulations often include flows going up and down mountain slopes").
- Their Table 2 obs errors (sigma_o, with gross-check ratio): METAR T 1.0 K (7.5); synoptic T 1.0 (5.0); **mesonet T 1.2 (7.0)**; METAR wind 1.6 m/s (7.0); **mesonet wind 4.8 m/s (5.0)** — mesonet wind is barely trusted; METAR pseudo-RH 5.9% (5.0), mesonet 7.1% (3.0); psfc 5.4 hPa (5.0) / mesonet 6.5 (3.0) — psfc sigma is representativeness-dominated (they verify psfc without elevation correction).

**Borrow without a var system:** (1) the aspect-tensor distance — usable directly inside Bratseth/OI as `rho_ij = exp(-1/2 d^T S_bar^{-1} d)` with `S_bar` averaged between the two endpoints (use instead of, or with, the `dz`/ITT terms); (2) the 500-m water offset; (3) per-type sigma_o + gross-check ratios; (4) weak-anisotropy-for-wind policy; (5) dynamic blacklists. **Do not borrow:** recursive filters, preconditioning, outer loops — they exist because RTMA must apply `B` on a 2.5–5-km national grid inside a minimizer; the kernel approach makes them unnecessary at bowecho's obs counts.

---

## 6. Why increments beat raw values, and the edge-behavior guarantee

Why increments (all three reasons are standard; first two stated in Daley 1991 §4.1, descending from Bergthorsson & Döös 1955):
1. **Data voids:** the analyzed increment is a weighted sum of innovations; where there are no innovations the sum is empty and the analysis *is* the background. Analyzing raw values instead would replace 3-km HRRR structure with a `2*dn ~ 100–200 km` low-pass interpolation of stations — catastrophic for a mesoanalysis layer.
2. **Statistical validity:** OI/Bratseth assume roughly homogeneous, isotropic, unbiased error fields. Innovations are plausibly that; raw T/Td/wind fields are not (terrain, coastlines, diurnal structure dominate).
3. **Resolution preservation:** the background carries all detail below the station Nyquist; the increments only correct the >= 2*dn-scale error component, which is the only component the network can actually see.

Edge guarantee (the math): the final grid increment is `delta_x = sum_i a_xi * s_i` with `a_xi = rho'_xi / m_i`, `m_i >= 1 + eps2 > 1`, and `|s_i|` bounded (the station iteration contracts, §3.2). Since `rho'_xi <= exp(-d_x^2 / R^2)` where `d_x` = distance to the nearest station,

```
|delta_x| <= N_loc * max_i|s_i| * exp( - d_x^2 / R^2 )  --> 0   exponentially,
```

and with the compact-support cutoff (`rho' = 0` beyond `3.5R`) the increment is **identically zero** — the analysis equals the HRRR background bit-for-bit — at any cell farther than `3.5R` from every station. Same argument covers Barnes-on-increments (weights vanish; enforce increment = 0 when fewer than 3 stations inside `R_c`). Additionally compute the **IDI (integrated data influence)** field — run the identical machinery once with all innovations set to 1 (background 0); the result in [0,1] (Uboldi et al. 2008; used by Tyndall & Horel 2013, Fig. 1) is a free "how much is this pixel obs-informed" mask: fade the obs-corrected layer toward the pure background where IDI < ~0.3, and surface it in the inspector readout.

---

## 7. Recommended configuration (per variable)

Scheme: **Bratseth, fixed R, 10 iterations (tol-early-exit), obs-space accumulation, single gridding pass; optional 3-iteration 30-km residual stage in mesonet-dense areas.** Background: latest HRRR F01 (or time-blended F00/F01) subset to the active window. All `eps2 = (sigma_o/sigma_b)^2`; published anchors cited; values marked * are derived (Td-space conversion of RTMA's pseudo-RH numbers), exposed in config.

| Variable | Background field | R horiz | Rz vert | ITT RB | sigma_b (HRRR 1-h) | sigma_o NWS/METAR | sigma_o mesonet | eps2 NWS / mesonet | Gross-check floor t_qc |
|---|---|---|---|---|---|---|---|---|---|
| T 2 m | T2m | 80 km | 200 m | 2000 m | 1.7 K (RTMA Table 1) | 1.0 K | 1.2 K (CWOP: treat as 1.5–2x) | 0.35 / 0.5 (Tyndall's conservative choice: 1.0 / 1.5) | 3 K |
| Td 2 m | Td2m (cap Td<=T after analysis) | 80 km | 200 m | 2000 m | ~2.0 K* | 1.5 K* | 2.5 K* | 0.6 / 1.5 (Tyndall: 1.0 / 1.0–2.0) | 4 K |
| u10, v10 (analyze components separately, univariate) | U10, V10 | 80 km | 1000 m or none (RTMA: keep wind anisotropy very weak) | none | ~1.8 m/s | 1.6 m/s | 4.8 m/s (RTMA) or ratio 1.5–2.0 (Tyndall; RAWS wind = 2.0) | 1.0 / 2.0 | 7.5 m/s + calm-wind rule |
| MSLP / altimeter | MSLP (MAPS reduction) | 200–300 km, single stage | none | none | ~1 hPa | altimeter ~0.7 hPa*; RTMA psfc book value 5.4 hPa (representativeness-dominated) | 6.5 hPa | 0.5 / high | 5 hPa |

Station-spacing context: METAR-only CONUS `dn ~ 60–100 km`; with mesonets, 10–40 km in populated areas. R = 80 km is the *background-error* scale (Tyndall et al. 2010, Lönnberg–Hollingsworth estimation) and should NOT be shrunk just because mesonets are dense — density is handled by `m_i`; shrink only in the optional residual stage. If running Barnes mode instead: `kappa_0 = 5.052*(2*dn/pi)^2` from the actual per-window `dn`, `gamma = 0.3`.

Caching for the 5-min cadence: station geometry, `rho_oo`, ITT blockage, `m`, and per-station grid disks change only when the station set, window, or config changes — persist them. Each cycle then costs: innovations (one bilinear read per station per variable) + QC + ~10 sparse symv + 1 scatter, per variable; re-use the same `rho` structures across T/Td/u/v (only `eps2`, hence `m`, differs — keep `row_sum(rho)` cached and form `m = eps2 + row_sum` per variable class for free). Re-grid only tiles whose increment changed beyond display epsilon to keep texture uploads small.

Validation checklist (all reproduce published results):
1. Single-ob test: increment must be a Gaussian blob of amplitude `d/(1+eps2)`, e-folding `R`, vertically clipped by `Rz`, blocked by ridges per ITT; far field exactly 0.
2. Bratseth-vs-OI: 50-iteration Bratseth == observation-space CG solve to <0.1% (Myrick et al. 2005 equivalence).
3. Barnes response: numerically measure `D1*` on synthetic sine fields against `D0*(1 + D0^(gamma-1) - D0^gamma)`; check `D1*(2dn) ~ 0.375` at gamma 0.2.
4. Leave-one-out cross-validation over a derecho case (e.g. the 2026-06-09 KEAX archive): analysis RMSE at withheld stations must beat raw HRRR (Myrick saw 14–32% over background; RTMA's tuned analysis cut RUC RMSE from 3.4 to 2.4 C in complex terrain).
5. Determinism: fixed iteration order + f32 accumulation in fixed tile order (or f64 accumulators) so re-runs are bit-identical.

---

## 8. Source-verification notes
- Koch/Barnes equations, constants (5.052, 0.0064, 0.375, gamma in [0.2,1.0], R_c = sqrt(20 kappa), dx/dn 0.3–0.5, Eq. 12) read directly from the Koch et al. (1983) paper scan, pp. 1489–1493.
- Bratseth iteration, M_j normalization, OI-limit proof and Gershgorin convergence argument read directly from the Tellus (1986) paper scan, pp. 439–446.
- ADAS concrete weights/normalization and R-schedules from Lazarus et al. (2002) full text, Eqs. (5)–(10), Tables 1–2.
- ITT form, RB = 2000 m, 75 km/375 m/eps2 = 0.1, ~10-iteration guidance from Myrick et al. (2005) full text, Eqs. (1)–(4), Fig. 4.
- RTMA cost function, aspect tensor (Eqs. 1, A1, A2), Tables 1–2, 500-m water offset, QC machinery from De Pondeca et al. (2011) full text.
- Observation-space 2DVar (Eqs. 1–3), 80 km/200 m, 300-km cutoff, network error ratios, QC Eq. (5) thresholds, calm-wind rule from Tyndall & Horel (2013) full text.
- BLUE/OI equations and the data-selection-discontinuity caveat from Bouttier & Courtier (1999) ECMWF notes (Eqs. A5–A8, §12).
- Barnes (1964, 1973) and Daley (1991) cited as the originating sources; their content was verified via the Koch and Bouttier–Courtier treatments above rather than the originals.

## SCHEME PAPERS
- Barnes, S. L., 1964: A technique for maximizing details in numerical weather map analysis. J. Appl. Meteor., 3, 396-409. DOI 10.1175/1520-0450(1964)003<0396:ATFMDI>2.0.CO;2
- Barnes, S. L., 1973: Mesoscale objective map analysis using weighted time-series observations. NOAA Tech. Memo. ERL NSSL-62, 60 pp.
- Koch, S. E., M. desJardins, and P. J. Kocin, 1983: An interactive Barnes objective map analysis scheme for use with satellite and conventional data. J. Climate Appl. Meteor., 22, 1487-1503. DOI 10.1175/1520-0450(1983)022<1487:AIBOMA>2.0.CO;2 [primary; equations read from paper scan]
- Bratseth, A. M., 1986: Statistical interpolation by means of successive corrections. Tellus, 38A, 439-447. DOI 10.1111/j.1600-0870.1986.tb00476.x (also 10.3402/tellusa.v38i5.11730) [primary; read from paper scan]
- Daley, R., 1991: Atmospheric Data Analysis. Cambridge University Press, 457 pp. [OI/B-R formalism; cross-verified via Bouttier & Courtier 1999]
- Bouttier, F., and P. Courtier, 1999: Data assimilation concepts and methods. ECMWF Meteorological Training Course Lecture Series, 59 pp. [BLUE Eqs. A5-A8; OI data-selection caveats]
- Bergthorsson, P., and B. Doos, 1955: Numerical weather map analysis. Tellus, 7, 329-340. [origin of increment-based successive correction; via Bratseth/Daley]
- Lazarus, S. M., C. M. Ciliberti, J. D. Horel, and K. A. Brewster, 2002: Near-real-time applications of a mesoscale analysis system to complex terrain. Wea. Forecasting, 17, 971-1000. DOI 10.1175/1520-0434(2002)017<0971:NRTAOA>2.0.CO;2 [ADAS Bratseth Eqs. 5-10, R/Rz schedules, error tables]
- Myrick, D. T., J. D. Horel, and S. M. Lazarus, 2005: Local adjustment of the background error correlation for surface analyses over complex terrain. Wea. Forecasting, 20, 149-160. DOI 10.1175/WAF847.1 [intervening-terrain term, RB=2000 m; Bratseth==OI==3DVAR equivalence]
- De Pondeca, M. S. F. V., et al., 2011: The Real-Time Mesoscale Analysis at NOAA's National Centers for Environmental Prediction: Current status and development. Wea. Forecasting, 26, 593-612. DOI 10.1175/WAF-D-10-05037.1 [2DVAR cost fn; terrain aspect tensor Eq. A2; Tables 1-2]
- Tyndall, D. P., and J. D. Horel, 2013: Impacts of mesonet observations on meteorological surface analyses. Wea. Forecasting, 28, 254-269. DOI 10.1175/WAF-D-12-00027.1 [obs-space 2DVar Eqs. 1-4; R=80 km, Z=200 m; network error ratios; QC Eq. 5]
- Tyndall, D. P., J. D. Horel, and M. S. F. V. De Pondeca, 2010: Sensitivity of surface air temperature analyses to background and observation errors. Wea. Forecasting, 25, 852-865. DOI 10.1175/2010WAF2222304.1 [Lonnberg-Hollingsworth estimation of length scales and error ratios]
- Riishojgaard, L. P., 1998: A direct way of specifying flow-dependent background error correlations for meteorological analysis systems. Tellus, 50A, 42-57. [basis of RTMA terrain-following covariance; via De Pondeca 2011]
- Purser, R. J., W.-S. Wu, D. F. Parrish, and N. M. Roberts, 2003: Numerical aspects of the application of recursive filters to variational statistical analysis. Parts I & II. Mon. Wea. Rev., 131, 1524-1535 and 1536-1548. [RTMA anisotropic recursive filters; via De Pondeca 2011]
- Lonnberg, P., and A. Hollingsworth, 1986: The statistical structure of short-range forecast errors as determined from radiosonde data. Part II. Tellus, 38A, 137-161. [innovation-based covariance estimation method; via Tyndall & Horel]
- Pauley, P. M., and X. Wu, 1990: The theoretical, discrete, and actual response of the Barnes objective analysis scheme for one- and two-dimensional fields. Mon. Wea. Rev., 118, 1145-1164. [discrete-response caveat for Barnes; cited for awareness]

## OPERATIONAL PRACTICE
# Operational Surface Mesoanalysis Systems — Practice Spec for the bowecho OBS ARC

Source documents downloaded and text-extracted to `C:\Users\drew\radar-work\_meso_research\` (depondeca2011.pdf, tyndall2013_v2.pdf, coniglio2012.pdf, morris2020.pdf, madaus2014.pdf, madis_sfc_qc_2005.html, coniglio2022_view.html). Local pipeline assessed: `C:\Users\drew\hrrr-mesoanalysis\mesoanalysis\obs\qc.py`, `C:\Users\drew\hrrr-mesoanalysis\mesoanalysis\analysis\barnes.py`.

---

## 1. SPC Mesoscale Analysis (SFCOA) — what it actually does

Primary sources: Bothwell, Hart & Thompson (2002, 21st Conf. Severe Local Storms, JP3.1 — full extended abstract read); SPC mesoanalysis help page; Coniglio (2012, WAF); Coniglio & Jewell (2022, MWR).

**Architecture (verified against the original paper):**
- **Scheme:** GEMPAK-based **2-pass Barnes** surface objective analysis (Barnes 1973) on a **40-km CONUS grid** (~17,000+ grid points). No published κ/γ values — SPC never documented the Barnes parameters.
- **Background:** RUC-2 (1998–2012), then **RAP 1-h forecast** surface fields (T, RH, p, u, v) as first guess. Per Bothwell 2002 the RUC fields "in addition to supplying a first guess … aid in the analysis by filling in data void areas both inland and off-shore." Per Coniglio (2012): "SFCOA simulations generated at 0, 15, and 30 min past the hour use the RUC 1-h forecast surface fields **to quality control the surface observations prior to the Barnes analysis**" and "the Barnes analysis of the surface data **does not use the RUC fields in any way during the analysis passes**." So the background's two roles are QC gate + void fill; the Barnes passes are an obs analysis (the innovation-based successive-correction variant in barnes.py is the modern equivalent and is what RTMA/UU2DVar do variationally — defensible, keep it).
- **QC:** one sentence in the literature: "Observations that depart significantly from these guess fields are excluded from the analysis" (Bothwell et al. 2002). **No threshold was ever published by SPC** — thresholds must be borrowed from RTMA/UU2DVar practice (Section 3).
- **3-D merge:** after the surface analysis, the **time-matched RAP forecast at 25-mb vertical increments** is merged above the surface → "three-dimensional pseudo-analysis."
- **Derivation ordering — VERIFIED: analyze first, then derive.** Bothwell 2002: the program "'builds' the elements of a vertical sounding (with temperature, moisture, u and v wind and vertical motion) every 25 mb in the vertical **at every grid point** … Many of the sounding analysis routines used are those from … NSHARP." SPC help page: "each gridpoint is inputed into a sounding analysis routine called 'NSHARP' to calculate about 100 new fields." CAPE/CIN/SRH/SCP/STP are computed **from the analyzed gridded soundings**, never analyzed from station-computed parameters. >225 output fields.
- **Cadence:** hourly; runs ~:05 after the hour, products posted by ~:15 ("usually updated by :15 after each hour").
- **Documented error characteristics** (Coniglio 2012, VORTEX2 soundings; Steeves et al. 2012; Coniglio & Jewell 2022, 257 field-program soundings):
  - SFCOA 2-m T rmsd ≈ 1.0 K, 2-m Td ≈ 1.5 K (vs RUC 1-h fcst 1.5 K / 2.0 K) — SFCOA's main win is **bias reduction**, more than RMSE reduction.
  - Residual biases inherited from RUC/RAP: PBL too shallow (≈ −300 m bias), lowest 1 km too cool/too moist, winds too fast in lowest 1 km, too slow 2–4 km, **MLCIN ~15 m²/s² too weak**; CIN/LFC errors remain "large relative to their potential impact on convective evolution."
  - Coniglio & Jewell 2022: SFCOA **underestimates near-ground (≤500 m) storm-relative winds and wind shear** (vertical-resolution limit), low-level CAPE too low near storms, dry bias above the PBL, misses shallow near-ground stable layers.

**Implication for bowecho:** an HRRR-background, obs-corrected surface analysis + NSHARP-style re-derivation of parcels at each grid point is exactly the SPC pattern; correcting the surface parcel from the analysis (planned item 4 in task #40) is the same lever SPC pulls — and the literature says its biggest payoff is **bias removal in thermodynamic fields** (CAPE/CIN/LCL), not winds aloft.

---

## 2. RTMA / URMA

Primary sources: De Pondeca et al. (2011, WAF 26, 593–612 — full text); Benjamin et al. (2007 — background downscaling); Morris et al. (2020, WAF 35, 977–996); Pondeca et al. (2015, WGNE Blue Book).

**System:**
- **GSI 2DVar** (univariate per analysis variable: T, pseudo-RH, ps, u/v via ψ/χ) on the 5-km NDFD CONUS grid (AWIPS 197, 1073×689 @ 5079.406 m; later 2.5 km). Incremental minimization with **two outer loops**; obs errors uncorrelated (diagonal R).
- **Background:** originally 13-km RUC 1-h forecast **downscaled** to the NDFD grid (Benjamin et al. 2007 — terrain/land-surface consistent downscaling, preserves stability/BL structure). Currently (Morris 2020): **downscaled HRRR 1-h forecast** (CONUS), with T/ps/moisture a blend of downscaled HRRR + 3-km NAM nest, RAP filling domain edges.
- **Terrain-aware covariances** (the part worth copying): anisotropic Gaussian autocovariances synthesized with recursive filters (Purser et al. 2003a,b), terrain-following per a Riishøjgaard (1998) variant. Inverse aspect tensor: **S⁻¹ = I/Lh² + (∇H)(∇H)ᵀ/Lf²** — covariance contours follow terrain contours; bigger terrain gradient ⇒ shorter effective correlation length across the gradient. Operational values (De Pondeca Table A): T Lh = 42.8 km, Lf = 636 m, σb = 1.7 K; pseudo-RH Lh = 40.1 km, Lf = 636 m, σb = 21%; ps Lh = 45.0 km, Lf = 636 m, σb = 1.9 hPa; ψ/χ Lh ≈ 64.6/64.8 km, Lf = 3818 m (wind nearly isotropic). **Land–water contrast:** effective elevation of major water bodies lowered by 500 m in the covariance construction so land obs don't bleed across shorelines.
- **Obs handling:** METAR, synoptic, mesonet (via MADIS), marine; ~14,300 T stations/analysis (2009). Obs window ±30 min, cutoff +30 min. Hourly; CONUS analysis lands ~43 min after valid time; **RTMA-RU** every 15 min (~13 min latency); **URMA re-runs at T+6 h** with late-arriving obs and is the NBM calibration/verification truth.
- **QC layers (4):** (1) honor MADIS flags; (2) **GSI gross check at the start of each outer loop**: reject when |innovation|/σo > R (table below) — the two-outer-loop design deliberately re-admits obs (cold-pool case: obs "too cold vs first guess" rejected in loop 1 passed in loop 2 after the guess moved); (3) **blacklists**: WFO-maintained static list + MADIS monthly-stats list + developer list + **dynamic list built hourly from gross-error statistics of the previous 3–6 analyses**; (4) **mesonet wind whitelist**: "trusted providers" + "trusted stations" lists inherited from RUC (Benjamin et al. 2010) — **mesonet winds are used only if whitelisted; ~60% of mesonet wind stations fail**. Flag rates: ~10% (T, moisture), ~25% (ps), **~55% (wind)**. Later versions add variational (nonlinear) QC (Purser 2011, 2018) and **relax the gross check in complex terrain** where the background can't resolve terrain-induced features (Morris 2020).
- **Known failure modes (published):**
  - **Mesonet wind slow bias** from anemometer siting drags the wind analysis low; the 31 Dec 2008 mid-Atlantic windstorm case shows MADIS-flags+gross-check alone are insufficient — most mesonet stations reported < 8 m/s against trusted METARs at 8–10 m/s; only the whitelist/blacklist stack rescued the analysis, and light-wind "bull's-eyes" persisted (De Pondeca §5b).
  - Morris 2020: assimilating mesonet obs **introduces a negative wind-speed bias** relative to Part-139 airport obs; RTMA wind is a usable METAR substitute **only ≤15 kt**; ceiling/visibility analyses are unusable for IFR decision support (non-Gaussian errors, neighbor-pair smearing); surface pressure suspect in complex terrain.
  - Univariate, static-covariance 2DVar is outperformed by flow-dependent EnKF for winds (Knopfmeier & Stensrud 2013 comparison cited by Morris); occasional **overfitting** acknowledged (De Pondeca §7); analysis-uncertainty estimate assumes zero systematic error.
- **Cross-validation results** (the verification template, §5d): five disjoint validation subsets, each ~10% of stations, built via a **Hilbert-curve space-filling partition** (Purser et al. 2009) so subsets are spatially uniform; 15 days, 3-hourly. cv-RMSE FG→ANL: **T 1.85→1.38 K (34%), q 0.68→0.47 g/kg (45%), ps 1.60→1.24 hPa (29%), wind 2.15→1.86 m/s (16%)**. Stated caveat: cv-RMSE includes obs + representativeness error ⇒ overestimates true analysis error.

---

## 3. QC for surface obs against a background — published thresholds

**MADIS (NWS TSP 88-21-R2, 1994; MADIS surface QC docs):** three levels.
- **Level 1 validity:** T −60…130 °F; Td −90…90 °F; RH 0–100%; SLP 846–1100 hPa; station p / altimeter 568–1100 hPa; wind speed 0–250 kt; gust 0–287 mph; wind dir 0–360°.
- **Level 2 temporal consistency (rate-of-change):** T ≤ 35 °F/h; Td ≤ 35 °F/h; SLP ≤ 15 hPa/h; wind speed ≤ 20 kt/h; internal consistency: Td ≤ T (both flagged on failure), SLP vs station-p, reported vs computed 3-h pressure tendency.
- **Level 3 spatial ("buddy") check:** OI analysis (Belousov et al. 1968) at each obs location using neighbors picked from **8 directional sectors** (nearest in each), with the **previous-hour RSAS analysis as background** (the OI is done on obs-minus-background residuals); if the obs–analysis difference exceeds a threshold (function of forecast + measurement + analysis error), neighbors are removed one at a time — if removing one neighbor reconciles the target, the **neighbor** is flagged suspect instead. T is converted to **potential temperature** before the check, with elevation differences entering the spatial correlation (Miller & Benjamin 1992) — this is how operational buddy checks survive complex terrain.
- **RTMA gross check:** reject if |innovation| > R·σo, re-evaluated each outer loop. Max acceptable departures from Table 2: **T: METAR 7.5 K** (1.0×7.5), synoptic/marine 5.0 K, **mesonet 8.4 K**; **wind (per component): METAR 11.2 m/s**, synoptic 8.0, **mesonet 24 m/s**, marine 13; pseudo-RH: 29.5% METAR/synoptic, 21.3% mesonet; ps: 27 hPa METAR (loose by design — blacklists do the real work for pressure).
- **UU2DVar (Tyndall & Horel 2013) — the cleanest published recipe:** reject unless
  **|y − H(xb)| ≤ max( εm · stddev(xb within 40 km), tqc )**, with **εm = 10** and floors **tqc = 3 °C (T), 4 °C (Td), 7.5 m/s (wind components & speed)**. The local-variability term keeps legitimate obs in terrain/dryline/front zones; the floor governs flat terrain. Plus: **light-wind check** (reject wind obs < 1 m/s when background > 5 m/s — kills the sheltered-anemometer failure mode at night), a manual blacklist, and **Dee (2005) adaptive bias correction** (per-hour-of-day bias grids, adaptivity γ = 0.15).
- **Madaus, Hakim & Mass (2014) — pressure-specific:** reject if model terrain anywhere in the surrounding 3×3 grid box differs from reported station elevation by **>200 m**; estimate per-station time-mean bias against a reference analysis over a quiet multi-week period and subtract it (28% of WU/CWOP altimeters had statistically significant biases); after bias correction assign **σo = 1.0 hPa to all altimeter obs regardless of network**.

**Assessment of `qc.py` (median/spread buddy + flat gross thresholds) against the above:**
- The 3-pass structure (range → background gross → buddy) is exactly canonical. Gross thresholds T 8 K / Td 10 K / wind 12 m/s / MSLP 6 hPa sit inside the RTMA METAR↔mesonet envelope — defensible, but **flat**: published practice (Tyndall) makes the threshold adaptive to local background variability — too loose in flat terrain, too tight vs HRRR in valleys. Adopt the `max(10·σ_bg,40km, floor)` form with floors 3 K / 4 K / 7.5 m/s.
- The **buddy check operates on raw T/Td over a 1.5° (~165 km) radius vs the neighbor median** — two deviations from practice: (a) operational checks buddy-check **innovations** (obs−background), or θ, never raw T — a raw-value median across 150 km straddles fronts/drylines and will false-reject exactly the stations you care about during severe weather; (b) the neighborhood is ~4× larger than the 40-km neighborhoods used by Tyndall and the nearest-in-8-sectors search of MADIS. The median (vs MADIS's OI) is actually a robustness *win* — keep the median, but feed it innovations within ≤75 km. The neighbor-exoneration step (MADIS: if removing one buddy reconciles the target, flag the buddy) is worth porting.
- The MSLP validity floor **945 hPa is too high** (hurricane cores: US landfalls < 920 hPa; MADIS uses 846–1100). Use 860–1090.
- Missing vs published practice: temporal consistency / flatline-persistence checks, station-elevation sanity (200 m), light-wind-vs-background check, per-station rolling innovation statistics → dynamic blacklist (RTMA builds its from the previous 3–6 analyses), per-station bias correction (Dee 2005), and a **mesonet-wind trust policy** (whitelist or heavy σo inflation — see §4).

**Recommended QC chain for the Rust port (ordered, with thresholds):**
1. **Metadata/elevation gate:** drop obs lacking coordinates; drop pressure obs where |station elev − model terrain (3×3 box)| > 200 m (Madaus). Maintain static + dynamic blacklist keyed on station ID.
2. **Validity (MADIS limits):** T −51…54 °C; Td −68…32 °C; Td ≤ T; RH 0–100%; SLP/altimeter 860–1090 hPa; wind speed 0–60 m/s sustained (gust ≤ 128 m/s); dir 0–360°.
3. **Temporal consistency** (when the station was seen last hour): |ΔT| ≤ 19.4 K/h, |ΔSLP| ≤ 15 hPa/h, |Δwspd| ≤ 10.3 m/s/h; flatline: reject T reporting identical value > 12 h.
4. **Background gross check (adaptive):** |y − H(xb)| ≤ max(10·stddev(xb within 40 km), floor), floors **3 K (T), 4 K (Td), 7.5 m/s (u, v, speed), 5 hPa (MSLP)**; nearest-neighbor H is fine at HRRR 3 km (current KDTree approach OK; bilinear is better near coasts).
5. **Light-wind check:** reject wind when obs < 1 m/s and background > 5 m/s (Tyndall) — single highest-value mesonet-wind filter per the RTMA case study.
6. **Buddy check on innovations:** for each obs, neighbors within **75 km** (try nearest-in-8-sectors), min 3 buddies; reject if |innov − median(buddy innovs)| > **3.5 K (T), 4.5 K (Td), 5 m/s (wind components), 3 hPa (MSLP)**; exonerate via single-neighbor removal as in MADIS. In sloped terrain compare θ-innovations.
7. **Per-station rolling bias:** accumulate innovation mean/RMS per station per hour-of-day; subtract slowly-adapting bias (Dee 2005, γ=0.15); stations whose |mean innovation| stays > 2σ of their network for N days → dynamic blacklist (this replaces RTMA's human-maintained lists).
8. **Network policy:** METAR/synoptic always eligible; mesonet/CWOP winds either whitelisted (long-term stats) or carried with 3× METAR σo; CWOP/PWS pressure only after bias correction.

---

## 4. Mesonet / CWOP quality and error weighting — the numbers

- **RTMA input error table** (De Pondeca 2011, Table 2 — σo with gross-check ratio R; "actual observation error used may be inflated … for duplicate observations or in response to the input quality control flag from MADIS"):

| Obs type | T σo (K) / R | pseudo-RH σo (%) / R | ps σo (hPa) / R | wind σo (m/s) / R |
|---|---|---|---|---|
| Land METAR | 1.0 / 7.5 | 5.9 / 5.0 | 5.4 / 5.0 | 1.6 / 7.0 |
| Land synoptic | 1.0 / 5.0 | 5.9 / 5.0 | 5.4 / 5.0 | 1.6 / 5.0 |
| **Land mesonet** | **1.2 / 7.0** | **7.1 / 3.0** | **6.5 / 3.0** | **4.8 / 5.0** |
| Marine | 1.0 / 5.0 | 5.9 / 5.0 | 5.4 / 5.0 | 2.6 / 5.0 |

  The headline number: **mesonet wind is carried at 3× the METAR error (4.8 vs 1.6 m/s) — a 9× variance deweighting — and even then only whitelisted stations are used.** (The ps σo values look inflated relative to instrument accuracy; treat them as RTMA's effective/representativeness-padded values — Madaus's 1.0 hPa post-bias-correction altimeter σo is the better number for a clean pipeline.)

- **Tyndall & Horel (2013) Table 1 — obs-to-background error variance ratios (σo²/σb²) by network category** (derived via Lönnberg & Hollingsworth 1986 from RUC innovations in Tyndall et al. 2010; ratio 1.0 ⇒ obs weighted equal to background):

| Category | Example | T | Td | Wind |
|---|---|---|---|---|
| NWS/FAA | ASOS/AWOS | 1.0 | 1.0 | 1.0 |
| FED+ | federal/state | 1.0 | 1.0 | 1.0 |
| RAWS | fire wx | 2.0 | 2.0 | 2.0 |
| **PUBLIC** | **"primarily CWOP"** (6,808 stns) | **1.5** | **1.5** | **2.0** |
| AG | agricultural (3-m towers) | 1.5 | 1.5 | 2.0 |
| AQ / EXT / LOCAL / TRANS | various | 1.5 | 1.5 | 1.5 |
| HYDRO | hydrological | 2.0 | 2.0 | 2.0 |

  Stated reasons: RAWS winds at 6 m (not 10 m) and rugged siting; "PUBLIC sensors are often mounted on or near residences with nearby obstructions commonplace." Notably their adjoint result: **impact is dominated by location/density, not network type** — "the assigned error variance ratios play a tertiary role" — and they document a CWOP station (D6557 Buxton NC) whose bias and impact matched the adjacent NWS station KHSE. So: deweight CWOP, don't discard it; in data-sparse areas it carries the analysis (their PUBLIC category showed "substantive impacts in many … locales" outside metros).
- **CWOP pressure** (Madaus et al. 2014): WU+CWOP supplied ~78% of all PNW pressure obs (METAR only 8%); after the 200-m elevation gate and per-station bias removal (28% of stations significantly biased), **all altimeters assimilated at σo = 1 hPa** with clear analysis/forecast gains. Pressure is the *best* citizen-station variable (no siting/exposure sensitivity); temperature is next (daytime radiative warm bias from cheap shields — CWOP's own guidance pushes Davis-style shields); **wind is the worst** (universal slow bias).
- Operational uptake: RUC began assimilating mesonet T/Td broadly but winds only via the trusted lists (Benjamin et al. 2010); GFS still used none of it as of 2011; MADIS QC's level-3 spatial check is what feeds the flags RTMA honors; EnKF data-denial work (Knopfmeier & Stensrud 2013) showed removing up to 75% of mesonet obs degrades analyses only nominally — density saturates in metros.

**Recommended bowecho obs-error table** (absolute, vs HRRR 1-h background with σb ≈ 1.7 K / 21% RH / 1.9 hPa / ~2 m/s, per RTMA):

| Network | T (K) | Td (K) | Wind (m/s) | Altimeter (hPa) | Notes |
|---|---|---|---|---|---|
| METAR/ASOS, synoptic | 1.0 | 1.4 | 1.6 | 1.0 | trust anchor |
| Marine/buoy (NDBC) | 1.0 | 1.4 | 2.6 | 1.0 | |
| State/federal mesonets | 1.2 | 1.7 | 2.5 | 1.5 | |
| RAWS | 1.4 | 2.0 | 3.2 (6-m mast) | 1.5 | fire-sited |
| CWOP/PWS | 1.5 | 2.0 | 4.8 (or whitelist) | 1.0 after bias-corr, else 2.0 | apply Dee bias-corr first |

---

## 5. Verification practice — the metric battery to ship

Published practice is **station-withholding cross-validation against the analysis**, never against assimilated obs:
- **Leave-one-out:** Horel & Dong (2010) — each of ~3,000 stations withheld from ~9,000 analyses (>570,000 cv experiments).
- **k-fold disjoint subsets:** De Pondeca et al. (2011) — 5 disjoint, spatially uniform ~10% subsets via Hilbert-curve partitioning; aggregate cv-RMSE over ≥2 weeks at 3-h intervals.
- **Targeted data denial:** Morris et al. (2020) — withhold specific (airport) stations, compare CONTROL/EXP/NODA (NODA = raw background = "worst case"), use **BCRMSE = √(RMSE² − bias²)** to separate systematic from random error.

**Ship this battery:**
1. Per cycle: withhold a fixed 10% station fold (Hilbert/checkerboard spatial stratification, stratified by network so METARs appear in every fold); rotate folds hourly → effectively k-fold over a day.
2. At withheld stations compute, per variable (T, Td, wind speed, altimeter): **bias (mean innov vs analysis), RMSE, BCRMSE, MAE**, and the same vs the raw HRRR background (FG) → **% improvement = 100·(FG−ANL)/ANL** (De Pondeca's IMPROV definition).
3. Stratify by: network category, hour of day, terrain class (flat / complex via local σ(elevation)), season; maintain per-station rolling innovation stats (doubles as the §3 step-7 dynamic blacklist feed).
4. **Benchmarks to beat/match:** background cv-RMSE ≈ 1.85 K (T), 2.15 m/s (wind); analysis cv-RMSE ≈ 1.38 K / 1.86 m/s; improvements ≈ 34% T, 45% q, 29% ps, 16% wind (De Pondeca Table 3). SFCOA-grade target vs independent soundings: 2-m T rmsd ≈ 1.0 K, Td ≈ 1.5 K (Coniglio 2012). Report the caveat verbatim: cv-RMSE includes the withheld obs' own measurement + representativeness error, so it overestimates true analysis error.
5. Sanity guard from the RTMA wind lesson: track analysis-minus-METAR wind bias separately — if it trends negative as CWOP wind weight rises, the mesonet slow bias is leaking in (Morris's exact failure mode).

**Barnes parameter note for `barnes.py`:** κ = 5.052e8 m² implies (Koch et al. 1983, κ = 5.052·(2Δn/π)²) an assumed mean station spacing Δn ≈ 15.7 km — appropriate only when CWOP/mesonet density is actually ingested; for a METAR-only pass (Δn ≈ 60–80 km CONUS) κ ≈ 7.4e9 m² is the design-consistent value. Compute Δn from the post-QC station set per region instead of hardcoding; γ = 0.3 with 2 passes matches Koch's recommended γ∈[0.2,0.5]. Consider a terrain-aware step toward RTMA: scale the effective Barnes radius down across strong terrain gradients (poor-man's S⁻¹ = I/Lh² + ∇H∇Hᵀ/Lf², Lf = 636 m) and apply the 500-m shoreline effective-elevation drop so lake/ocean points aren't corrected by land obs.

---

## Sources

- Bothwell, Hart & Thompson 2002 — [AMS program page](https://ams.confex.com/ams/SLS_WAF_NWP/techprogram/paper_47482.htm) / [extended abstract PDF](https://ams.confex.com/ams/pdfpapers/47482.pdf)
- [SPC Mesoscale Analysis help](https://www.spc.noaa.gov/exper/mesoanalysis/help/begin.html)
- Coniglio 2012, WAF — doi:10.1175/WAF-D-11-00096.1; Coniglio & Jewell 2022, MWR — [doi:10.1175/MWR-D-21-0222.1](https://journals.ametsoc.org/view/journals/mwre/150/3/MWR-D-21-0222.1.xml)
- De Pondeca et al. 2011, WAF — [doi:10.1175/WAF-D-10-05037.1](https://journals.ametsoc.org/view/journals/wefo/26/5/waf-d-10-05037_1.xml); [Pondeca et al. 2015 WGNE update](https://www.wcrp-climate.org/WGNE/BlueBook/2015/individual-articles/01_Pondeca_Manuel_etal_RTMA.pdf)
- Morris et al. 2020, WAF — [doi:10.1175/WAF-D-19-0201.1](https://journals.ametsoc.org/waf/article/35/3/977/345098/A-Quality-Assessment-of-the-Real-Time-Mesoscale)
- Tyndall & Horel 2013, WAF — [doi:10.1175/WAF-D-12-00027.1](https://journals.ametsoc.org/doi/abs/10.1175/WAF-D-12-00027.1); Tyndall, Horel & De Pondeca 2010, WAF — doi:10.1175/2010WAF2222357.1
- Horel & Dong 2010, JAMC — [doi:10.1175/2010JAMC2397.1](https://www.semanticscholar.org/paper/An-Evaluation-of-the-Distribution-of-Remote-Weather-Horel-Dong/bd99ce0e5ff1aaa144034ae8c65b8b54c606a35b)
- Madaus, Hakim & Mass 2014, MWR — [doi:10.1175/MWR-D-13-00269.1](https://atmos.uw.edu/~hakim/papers/madaus_hakim_mass_2013_pressure_assimilation.pdf)
- MADIS QC — [overview](https://madis.ncep.noaa.gov/madis_qc.shtml), [surface QC](https://madis.ncep.noaa.gov/madis_sfc_qc.shtml) (thresholds from archived FSL version), [CWOP in MADIS](https://madis.ncep.noaa.gov/madis_cwop.shtml)
- [Steeves, Wheatley & Coniglio REU paper](https://caps.ou.edu/reu/reu12/finalpapers/Steeves-finalpaper.pdf); [CWOP official guide](https://www.weather.gov/media/epz/mesonet/CWOP-OfficialGuide.pdf)

## OPERATIONAL PAPERS
- Bothwell, P. D., J. A. Hart, and R. L. Thompson, 2002: An integrated three-dimensional objective analysis scheme in use at the Storm Prediction Center. Preprints, 21st Conf. on Severe Local Storms, San Antonio, TX, Amer. Meteor. Soc., JP3.1.
- De Pondeca, M. S. F. V., and Coauthors, 2011: The Real-Time Mesoscale Analysis at NOAA's National Centers for Environmental Prediction: Current status and development. Wea. Forecasting, 26, 593-612, doi:10.1175/WAF-D-10-05037.1.
- Tyndall, D. P., and J. D. Horel, 2013: Impacts of mesonet observations on meteorological surface analyses. Wea. Forecasting, 28, 254-269, doi:10.1175/WAF-D-12-00027.1.
- Tyndall, D. P., J. D. Horel, and M. S. F. V. De Pondeca, 2010: Sensitivity of surface air temperature analyses to background and observation errors. Wea. Forecasting, 25, 852-865, doi:10.1175/2010WAF2222357.1.
- Coniglio, M. C., 2012: Verification of RUC 0-1-h forecasts and SPC mesoscale analyses using VORTEX2 soundings. Wea. Forecasting, 27, 667-683, doi:10.1175/WAF-D-11-00096.1.
- Coniglio, M. C., and R. E. Jewell, 2022: SPC mesoscale analysis compared to field-project soundings: Implications for supercell environment studies. Mon. Wea. Rev., 150, 567-588, doi:10.1175/MWR-D-21-0222.1.
- Morris, M. T., J. R. Carley, E. Colon, A. Gibbs, M. S. F. V. De Pondeca, and S. Levine, 2020: A quality assessment of the Real-Time Mesoscale Analysis (RTMA) for aviation. Wea. Forecasting, 35, 977-996, doi:10.1175/WAF-D-19-0201.1.
- Horel, J. D., and X. Dong, 2010: An evaluation of the distribution of Remote Automated Weather Stations (RAWS). J. Appl. Meteor. Climatol., 49, 1563-1578, doi:10.1175/2010JAMC2397.1.
- Madaus, L. E., G. J. Hakim, and C. F. Mass, 2014: Utility of dense pressure observations for improving mesoscale analyses and forecasts. Mon. Wea. Rev., 142, 2398-2413, doi:10.1175/MWR-D-13-00269.1.
- Benjamin, S. G., J. M. Brown, G. Manikin, and G. Mann, 2007: The RTMA background - hourly downscaling of RUC data to 5-km detail. Preprints, 22nd Conf. on Weather Analysis and Forecasting/18th Conf. on Numerical Weather Prediction, Park City, UT, Amer. Meteor. Soc., 4A.6.
- Benjamin, S. G., B. D. Jamison, W. R. Moninger, S. R. Sahm, B. E. Schwartz, and T. W. Schlatter, 2010: Relative short-range forecast impact from aircraft, profiler, radiosonde, VAD, GPS-PW, METAR, and mesonet observations via the RUC hourly assimilation cycle. Mon. Wea. Rev., 138, 1319-1343, doi:10.1175/2009MWR3097.1.
- Purser, R. J., W.-S. Wu, D. F. Parrish, and N. M. Roberts, 2003: Numerical aspects of the application of recursive filters to variational statistical analysis. Parts I & II. Mon. Wea. Rev., 131, 1524-1535 and 1536-1548.
- Riishojgaard, L.-P., 1998: A direct way of specifying flow-dependent error correlations for meteorological analysis systems. Tellus, 50A, 42-57.
- Koch, S. E., M. desJardins, and P. J. Kocin, 1983: An interactive Barnes objective map analysis scheme for use with satellite and conventional data. J. Climate Appl. Meteor., 22, 1487-1503.
- Barnes, S. L., 1973: Mesoscale objective map analysis using weighted time-series observations. NOAA Tech. Memo. ERL NSSL-62, 60 pp.
- Lonnberg, P., and A. Hollingsworth, 1986: The statistical structure of short-range forecast errors as determined from radiosonde data. Part II. Tellus, 38A, 137-161.
- Dee, D. P., 2005: Bias and data assimilation. Quart. J. Roy. Meteor. Soc., 131, 3323-3343.
- Fiebrich, C. A., C. R. Morgan, A. G. McCombs, P. K. Hall, and R. A. McPherson, 2010: Quality assurance procedures for mesoscale meteorological data. J. Atmos. Oceanic Technol., 27, 1565-1582.
- Knopfmeier, K. H., and D. J. Stensrud, 2013: Influence of mesonet observations on the accuracy of surface analyses generated by an ensemble Kalman filter. Wea. Forecasting, 28, 815-841.
- NWS, 1994: Techniques Specification Package (TSP) 88-21-R2 (basis of MADIS QC: validity, temporal, internal, and OI-based spatial consistency checks; Belousov et al. 1968 OI; Miller and Benjamin 1992 elevation-dependent correlations).

## VERIFICATION: SCHEMES
# Adversarial verification of the bowecho objective-analysis spec

Every formula and constant was re-checked against the primary sources in `C:\Users\drew\radar-work\bowecho\.research_tmp\` (Koch 1983 page scans `koch_p3..p7.png` = pp. 1489–1493; Bratseth 1986 scans `bratseth_p1..p9.png` = pp. 439–447; full texts of Lazarus 2002, Myrick 2005, Tyndall & Horel 2013, De Pondeca 2011, Bouttier & Courtier, and `rtma_preprint.txt`), with zoomed crops where OCR was absent, plus web cross-checks of bibliographic metadata ([AMS record for Koch et al. 1983](https://journals.ametsoc.org/view/journals/apme/22/9/1520-0450_1983_022_1487_aiboma_2_0_co_2.xml), [Wiley record for Bratseth 1986](https://onlinelibrary.wiley.com/doi/abs/10.1111/j.1600-0870.1986.tb00476.x), [Tellus archive copy](https://tellusjournal.org/articles/10.3402/tellusa.v38i5.11730)). Verdict: the spec is overwhelmingly faithful — the Bratseth weight normalization, the ADAS concrete form, the ITT, and all RTMA/Tyndall tables check out exactly — but there are **3 hard errors, 3 substantive mischaracterizations, and several unsourced constants**.

## A. WRONG — must fix

**A1. Koch's two-pass response target is e⁻¹ ≈ 0.368, not 0.375** (§2.2 and validation item 3).
Koch et al. 1983, p. 1492 (zoomed crop, verbatim): *"The value of 0.0064 for D₀(2Δn) is selected to give a second pass response of **D₁\* = e⁻¹** at the 2Δn wavelength when γ = 0.2."* The spec's own Eq. (11) confirms it numerically: D₀(1 + D₀^(γ−1) − D₀^γ) with D₀ = 0.0064, γ = 0.2 gives **0.368**, not 0.375. Fix both occurrences ("D1\* = 0.375" → "D1\* = e⁻¹ ≈ 0.368") or the validation test in checklist item 3 will fail against its own formula. Source: koch_p6.png / zoom_koch_d1.png.

**A2. Misquote of the grid-spacing constraint** (§2.2). Spec: "`dx <= dn/2` ('Δx must be no larger than one-half of **2Δn**', p. 1493)". The paper (p. 1493, zoomed) actually reads: *"Since five grid points are required to represent a wave (Peterson and Middleton, 1963)… and the minimum resolvable wave is of 2Δn scale, then **Δx must be no larger than one-half of Δn**…"* The inequality `dx ≤ dn/2` is correct; the quotation is wrong and, read literally, contradicts it (one-half of 2Δn = Δn). Delete "2" from the quote. Source: koch_p7.png / zoom_koch_dx.png.

**A3. Bratseth two-ob example: the fast case has correlation e⁻¹ ≈ 0.37, not 0.5** (§3.2). Bratseth's Eq. (20) is ρ = exp{−[(xᵢ−xⱼ)² + (yᵢ−yⱼ)²]/2} (zoom_bratseth_eq20.png). Fig. 1's separation √2 gives ρ = e⁻¹ ≈ 0.37 (≈5 iterations); Fig. 2's separation ½√2 gives ρ = 0.78, and the paper states on p. 443: *"This explains the slow convergence in Fig. 2 where ψ̄₁ψ₂ = 0.8."* So "(with inter-ob correlation 0.8, ~25 iterations…)" is right; "with 0.5, ~5" should be "with ~0.37 (e⁻¹), ~5". (Cross-check via his Eq. (24) contraction factor λ = 2ρ/(1+ρ): 0.88²⁵ ≈ 5%, 0.54⁵ ≈ 5% — consistent.)

## B. Mischaracterizations — reword

**B1. "The m_i normalization is what makes the iteration converge to OI instead of merely resembling it"** (§3.1) inverts Bratseth's own emphasis. Bratseth, p. 441, verbatim: *"It is also important to appreciate the remarkable fact that **M_j does not influence the iteration limit** (if it exists)."* What makes the limit equal OI is the **Eq. (15a)/(15b) numerators** — the ρ+ε²I obs-space covariance and the grid/obs weight asymmetry (M cancels in the limit: aₓᵀA⁻¹ = ρₓᵀC⁻¹). The row-sum M_j choice (Eqs. 16–18, = A9) is what **guarantees and paces convergence** (Gershgorin). The spec's §3.2 already says this correctly; fix the §3.1 sentence.

**B2. "with the published kernels below this cannot happen" (divergence) is not guaranteed once the ITT is applied** (§3.2). Bratseth's lower bound μ ≥ 0 (his Eq. A6) requires C to be a true (positive-semidefinite) covariance. Myrick's ITT multiplies the Gaussian elementwise by exp(−a²/RB²), which is symmetric but **not provably PSD** (it is not a Schur product with a known-PSD matrix); Myrick et al. only observe empirically that the ITT solution *converged faster* (their Fig. 4) — they prove nothing about definiteness. The 3.5R hard cutoff also formally breaks PSD (negligibly: truncated correlations ≤ e^(−12.25) ≈ 5×10⁻⁶). Keep the residual-increase safeguard active whenever ITT is enabled rather than treating it as dead code, and note that the OI/CG verification oracle (which needs SPD for Cholesky/CG) inherits the same caveat.

**B3. RTMA QC percentages misattributed** (§1.3 item 5). De Pondeca §3b, verbatim: *"The percentage of stations flagged by the various quality control mechanisms is around 10% for temperature and moisture, 25% for surface pressure, and a staggering 55% for wind."* These are totals across **all four QC layers** (MADIS flags + three static lists + the dynamic list + the Mesonet-wind trusted-provider/station lists), not the dynamic 3–6-cycle blacklist alone as "RTMA flags … this way" implies. And the 55% is **all wind stations**, not "mesonet wind"; the mesonet-specific figure is *"about 60% of the Mesonet wind stations fail the provider- and station-list criteria."* (The "previous three to six analyses" dynamic-list description itself is verbatim-correct.)

## C. Unverifiable / unsourced constants — star or cite

**C1. Wind σ_b ≈ 1.8 m s⁻¹ and MSLP σ_b ≈ 1 hPa, and "MSLP R 200–300 km, single stage"** (§7 table) have **no anchor in any cited source**. RTMA Table 1 expresses wind background error as ψ/χ variances (118 494 / 115 572 m² s⁻¹), never as a 10-m wind σ_b; RTMA's psfc σ_b is 1.9 hPa, not ~1 hPa. The §0 claim "every constant is published" overreaches for these three — star them as derived/assumed like the Td entries, or cite a source.

**C2. The "3.4 → 2.4 °C in complex terrain" validation anchor** (§7 item 4) is **not in De Pondeca et al. 2011** (their Table 3 cross-validation is 1.85 → 1.38 K domain-wide, 34%). It comes from the Kahler RTMA preprint in the research folder (`rtma_preprint.txt`, corresponding author Chad M. Kahler, NWS — i.e., the "Kahler & Myrick 2008" preprint): parallel RTMA 2.4 °C vs RUC 3.4 °C (vs operational RTMA 2.8 °C), northern-Utah point verification — that is legitimately complex terrain, and the ±12-min window also appears there. Add the explicit citation; as a conference preprint it is the weakest source in the spec.

**C3. "(and buoy/ship obs)" lowered 500 m** (§1.2): both De Pondeca §2b (*"decreases the effective elevation height on major water bodies arbitrarily by 500 m in the autocovariance construction"*) and Tyndall p. 256 (*"water analysis grid points have their elevation reduced by 500 m"*) lower **water grid cells only**; neither paper lowers observation elevations. Sensible extension, but mark it as the spec's own.

**C4. Bergthorsson & Döös 1955 / "Daley 1991 §4.1"** (§6): no Daley text exists locally; standard attribution, plausible, but unverified as cited.

## D. Nitpicks

- §3.2: the "~10 or fewer iterations (Seaman 1988…)" guidance sits in Myrick's **§2** (before their §2b), not "their §3". Citation list itself ✓ (Seaman 1988; Franke 1988; Carr et al. 1996; Lazarus et al. 2002; Xue et al. 2003).
- §8: the variational form J(x) is Bouttier & Courtier's **(A9)**, just outside the cited "A5–A8" (A5 analysis, A6 gain, A7 generic covariance, A8 optimal-K covariance).
- §3.4 "Bratseth himself blesses radius reduction as the graceful way to handle mutually inconsistent obs — p. 443: it yields a smoothed version of the limiting analysis" conflates two adjacent p. 443 passages: (i) inconsistent obs → slow convergence → a *truncated iteration* "will then be a smoothed version of the limiting analysis. For many purposes this is quite acceptable"; (ii) a separate paragraph endorses systematic radius reduction for smaller-than-average features ("Much can be said in favour of this practice even if it does not fit into the statistical interpolation framework"). Net advice survives; the attribution of the "smoothed version" clause to radius reduction does not.
- §7 "(CWOP: treat as 1.5–2x)": Tyndall's 1.5/2.0 are **variance ratios** (σ_o²/σ_b²), not σ multipliers (σ factor ≈ 1.22–1.41) — say which is meant.

## E. Verified correct (spot-checks all passed)

- **Koch**: Eq. (1) w = exp(−r²/κ) with no factor 4, κ in length², R = √κ at w = e⁻¹ (p. 1489); Eq. (2) D₀ = exp[−κ(π/λ)²]; Eq. (4) κ₁ = γκ₀; Eq. (7); Eq. (8) two-pass sum with bilinear interpolation of g₀ to stations; Eq. (6) D₁ = D₀^γ; Eqs. (10b)/(11) D₁\* = D₀(1 + D₀^(γ−1) − D₀^γ); γ ∈ [0.2, 1.0] (p. 1492); Δn_c = mean nearest-neighbor distance (§3b); Eq. (12) Δn_r = √A(1+√M)/(M−1); Eq. (13) κ₀ = 5.052(2Δn/π)² with D₀(2Δn) = 0.0064; worked κ values (3 276/7 371/20 475 km² → R = 57/86/143 km, recomputed); R_c = √(20κ₀), R_c/R = 4.5, w = 2×10⁻⁹ verbatim p. 1493; M = 3 warning; Δx/Δn ~0.3–0.5; the italicized "only two passes through the data are required" (p. 1490).
- **Bratseth**: f = F − F^P notation (p. 439); iteration Eq. (4); the §3.5 accumulated-residual optimization is exactly his Eq. (8); OI-limit machinery Eqs. (7)–(11) (limit weights p^T = aₓᵀA⁻¹ = ρₓᵀ(Ρ+ε²I)⁻¹, M-independent); asymmetric weights (15a)/(15b); M_j suggestion (16)–(18) reduces to m_i = ε² + Σⱼρᵢⱼ for uniform ε — **identical to Lazarus Eq. (9)**; appendix p. 446: A = C·D, Gershgorin named explicitly, eigenvalues in [0,1] (A6 + A8/A9, M_i = absolute row sum), zero eigenvalues ↔ redundant obs and harmless. The spec's single-ob invariant d/(1+ε²) in one iteration checks algebraically.
- **Lazarus (ADAS)**: Eqs. (5)–(10) all verbatim — a_xi = ρ_xi/m_i, a_oi = (ρ_oi + σ²δ_oi)/m_i, m_i = σ² + Σρ, Gaussian ρ (Eq. 10); Table 1 R = 200/80/50/40 km default, 100/50/25/12 km operational; Rz = 500 m default, 1200/600/300/150 m operational; 4 passes; Utah station separation "on the order of 10 km".
- **Myrick**: Eqs. (1)–(2) Gaussian; ITT Eqs. (3)–(4); a = z_t − max(z_i, z_j) applied "if higher terrain is found to block the path" (the spec's max(0,·) is a faithful formalization); RB = 2000 m; 1000-m obstacle → 22% reduction; R = 75 km (and 25 km), Rz = 375 m, ε = 0.1; ~100 iterations to full convergence, skill ≈ OI after ~10 (Fig. 4 inset); improvements 14–30–32% (Table 1: 0.87/0.71/0.69 vs 1.01); Bratseth = OI = 3DVAR identical; highest-gridpoint search along the most direct route.
- **Tyndall & Horel**: Eqs. (1)–(4) incl. obs-space solve and x_a = x_b + P_bH^Tη; R = 80 km, Z = 200 m from Tyndall, Horel & De Pondeca 2010 (WAF 25, 852–865, confirmed in reference list) via Lönnberg & Hollingsworth (1986); 300-km zeroing; 500-m water grid points (p. 256, verbatim); QC Eq. (5) with ε_m = 10, t_qc = 3 °C/4 °C/7.5 m s⁻¹ and 40-km stddev, verbatim; no forward-operator elevation adjustment (Mount Rainier example explicit); calm rule (<1 m s⁻¹ obs vs >5 m s⁻¹ background; NWS lowest reportable 1.25 m s⁻¹); 10 network categories with ratios 1.0/1.5/2.0, RAWS wind 2.0; the isolated-ob "average of the observation and background" quote; IDI built from background = 0, obs = 1, value 0.5 at an isolated ob (Uboldi et al. 2008).
- **De Pondeca (RTMA)**: Eq. (1) with control vector = "departure of the estimate of the analysis from the specified background" (verbatim); two outer loops × 50 inner; balance off → univariate; ψ, χ, T, psfc, pseudo-RH (= q scaled by background saturated value; Td derived); (A1) C = σ²exp(−½Δx^TS⁻¹Δx); (A2) S⁻¹ = I/L_h² + (∇H)(∇H)^T/L_f², "variant of Riishøjgaard (1998)"; Table 1 exact (T 42 834/636/1.7 K; pseudo-RH 40 054/636/0.21; psfc 45 003/636/1.9 hPa; ψ 64 567/3 818, χ 64 764/3 818); the weak-wind-anisotropy quote verbatim; Table 2 exact (METAR T 1.0/7.5, synoptic 1.0/5.0, mesonet T 1.2/7.0, METAR wind 1.6/7.0, mesonet wind 4.8/5.0, pseudo-RH 5.9%/5.0 & 7.1%/3.0, psfc 5.4/5.0 & 6.5/3.0, marine wind 2.6/5.0); −12/+12-min window; dynamic list from "the previous three to six analyses"; 500-m water offset in §2b verbatim; the 5%-slope worked example recomputes to 12.2 km ✓.
- **Bouttier & Courtier**: BLUE as (A5)–(A8), A = (I−KH)B for optimal K; the data-selection discontinuity quote is the Figure 9 caption in §12 (OI), with Figure 10 box-selection as the mitigation — exactly as cited.
- **Bibliography**: Koch et al., J. Climate Appl. Meteor. 22, 1487–1503 ([AMS](https://journals.ametsoc.org/view/journals/apme/22/9/1520-0450_1983_022_1487_aiboma_2_0_co_2.xml)); Bratseth, Tellus 38A(5), 439–447 ([Wiley](https://onlinelibrary.wiley.com/doi/abs/10.1111/j.1600-0870.1986.tb00476.x)); Myrick WAF 20, 149–160; Lazarus WAF 17 (Oct 2002); De Pondeca WAF 26, 593–612; Tyndall & Horel WAF 28 (Feb 2013) — all consistent with the page headers in the local scans.

## Bottom line

The architecture-level claims the task flagged for special attention all survive: Koch's κ/γ relations and response function are reproduced correctly (only the 0.375 constant is wrong — it is e⁻¹); Bratseth's iteration genuinely converges to OI with exactly the spec's weight normalization m_i = ε² + Σρ (= Lazarus Eq. 9 = Bratseth Eqs. 16–18/A9), though the spec credits the normalization for the limit when Bratseth explicitly proves the limit is M-independent; and the Myrick ITT terrain penalty is quoted correctly (form, RB = 2000 m, 22% example) but is not provably positive-definite, so the convergence "cannot happen" guarantee must be downgraded to "monitor when ITT is on." Apply fixes A1–A3 before implementation (A1 breaks a unit test as written), reword B1–B3, and star/cite the §7 constants in C1–C2.

## VERIFICATION: OPERATIONAL
# Adversarial Verification — Operational Surface Mesoanalysis Spec (bowecho OBS ARC)

Verified against the primary texts in `C:\Users\drew\radar-work\_meso_research\` (null-stripped copies `*_nn.txt` created; plus newly fetched `bothwell2002.pdf/.txt`, `steeves.txt`, `coniglio2022.txt`, `madis_sfc_qc.txt`), the live SPC help page, and the local code (`C:\Users\drew\hrrr-mesoanalysis\mesoanalysis\obs\qc.py`, `...\analysis\barnes.py`).

## Bottom line

The spec is **substantially accurate**. All three priority areas check out against primary sources: (1) SPC's analyze-first/derive-second ordering is verbatim-confirmed in Bothwell et al. (2002) and the SPC help page; (2) every quoted QC threshold (MADIS L1/L2/L3, Tyndall εm/floors, RTMA Table 2 R-ratios) matches the source documents exactly; (3) both obs-error tables (De Pondeca Table 2, Tyndall Table 1) are reproduced without error, including the mesonet-wind 4.8 vs METAR 1.6 m/s headline. I found **no wrong numbers in any threshold or error table**. There are, however, 9 items that are misattributed, misquoted, mislabeled, or unverifiable — two of which would matter if someone implements "per the cited paper."

---

## A. Errors requiring correction

**A1. RTMA obs window "±30 min" is wrong for the De Pondeca-era system it sits next to (HIGH — implementation-relevant).** De Pondeca et al. (2011, §3): "The assimilation time window is −12 to +12 min centered around the analysis time" — and the §7 planned-improvements list says the window "is to be expanded from ±12 to ±30 min" only once FGAT is added. The ±30-min window with +30-min cutoff is the **current** (v2.7-era) configuration per Morris et al. (2020, §2a): "The RTMA uses observations from 30 min prior to 30 min after the analysis time and has a data cutoff of 30 min after the analysis time." Fix: keep ±30 but cite Morris 2020, not the 2011 system description. Same fix for latency: De Pondeca 2011 says fields available "around 50 min past the hour"; the ~43-min figure is Morris 2020 (footnote: 43/38/34/33 min for CONUS/AK/HI/PR). Both numbers are right for their eras; the spec silently mixes them.

**A2. Koch γ range "[0.2,0.5]" is not the published recommendation (MEDIUM).** Koch et al. (1983, JCAM 22, 1487–1503) restrict the convergence parameter to **0.2 ≤ γ ≤ 1.0**; GEMPAK's objective-analysis documentation (Unidata GEMPAK tutorial, "Objective Analysis") states γ "ranges between 0 and 1. A value between .2 and .3 is generally assumed," with 0.3 the recommended default. So γ=0.3/2-pass is still correct practice, but cite it as "within Koch's permissible 0.2–1.0, at the GEMPAK-recommended 0.3," not "Koch's recommended [0.2,0.5]." (I could not retrieve Koch full text — AMS returns 403 — so the 0.2–1.0 range rests on the GEMPAK docs and consistent secondary citations; the [0.2,0.5] bracket appears in none of them.) The κ back-calculation checks out: κ=5.052e8 m² ⇒ Δn = (π/2)·√(κ/5.052) ≈ 15.7 km; κ≈7.4e9 corresponds to Δn = 60 km exactly (80 km would be 1.31e10 — say "κ ≈ 0.7–1.3e10" for the 60–80 km METAR range).

**A3. §5b windstorm case misquotes the METAR winds (LOW).** Spec: "most mesonet stations reported < 8 m/s against trusted METARs at 8–10 m/s." De Pondeca §5b actually says the METAR-dominated region had wind speeds **"stronger than 10 m s⁻¹"** (only a few exceptions at 5–8); it was the **first guess** that had patches of 8–10 m/s; the rescued analysis achieved "winds above 9 m s⁻¹ for most of the region of focus." The mesonet <8 m/s part is correct (Fig. 10d discussion).

**A4. Madaus "28% of WU/CWOP altimeters" — wrong referent (LOW).** Madaus et al. (2014): "28% of **the observations**" (the full altimeter set checked against RSAS, of which ~78% are WU+CWOP and 8% ASOS) had statistically significant biases. Also omitted: an additional discard rule — obs with mean difference >5 hPa or σ(difference) >2 hPa are thrown out as unrepresentative — which belongs in the recommended QC chain (step 7). Minor: Madaus's own altimeter validity range was 800–1100 hPa, and bias was estimated over **May–July 2011** (~3 months, not "multi-week"). The σo = 1 hPa ("1 hPa² error variance … regardless of their source," from a 2005 GFS/DART error table) and the 200-m 3×3-box elevation gate are verbatim-confirmed.

**A5. SFCOA cadence is incomplete (LOW, worth fixing for fidelity).** Coniglio (2012, §2): "The SFCOA is produced by SPC in real time **every 15 min**" — runs at :00/:15/:30/:45, with the :00/:15/:30 runs QC'ing against the RUC **1-h forecast** and the :45 "final" run QC'ing against the RUC **analysis** (and carrying 5–10% more obs; that final run is what's archived). The "hourly, ~:05, posted by ~:15" description matches only the public web product (SPC help page: "2-pass Barnes surface objective analysis around :05 after each hour … usually updated by :15"). If bowecho emulates SPC, note that the web-facing product = the early-run, 1-h-forecast-QC'd variant.

**A6. "Table A" → Table 1 (TRIVIAL).** The RTMA covariance parameters (T Lh=42 834 m, Lf=636 m, σb=1.7 K; pseudo-RH 40 054/636/0.21; ps 45 003/636/1.9 hPa; ψ 64 567/3818; χ 64 764/3818) are **Table 1 of the main text**, not an appendix table. Every value matches. Note the paper says these are latitude-dependent values, domain-averaged for the table.

**A7. Hilbert-curve attribution (TRIVIAL).** De Pondeca cites "De Pondeca et al. 2006; Purser et al. 2009; Tyndall et al. 2010" — the cross-validation application is **De Pondeca et al. (2006, AGU)**; Purser et al. (2009) is the NOAA Office Note on the curve construction itself. Also: subsets are constructed **separately per observation category** (T, q, wind, ps) — relevant to the verification battery design.

**A8. Recommended-chain item 2 mislabels a tightening as "MADIS limits" (TRIVIAL).** "Wind speed 0–60 m/s sustained" is the spec's own (sensible) tightening — the MADIS validity limit is 250 kt ≈ 128.6 m/s sustained. Label it as a deliberate tightening. (All actual MADIS limits quoted in §3 are exact: T −60…130 °F, Td −90…90 °F, RH 0–100%, SLP 846–1100, station-p/altimeter 568–1100, speed 0–250 kt, gust 0–287 mph, dir 0–360°; temporal: T and Td 35 °F/h, SLP 15 hPa/h, speed 20 kt/h; Td≤T flags both; SLP-vs-station-p flags both; the 3-h-tendency check flags **only the reported tendency ob**, a detail the spec elides.)

**A9. "CIN/LFC errors remain 'large relative to their potential impact on convective evolution'" is presented as a quote but is a paraphrase (TRIVIAL).** Coniglio 2012's words: "the rmsd values remain large relative to changes in variables that can have a large impact on the evolution of the convection. This is particularly true for the CIN and LFC variables." Unquote it or quote it exactly.

## B. Unverifiable / inference items (keep, but mark as such)

- **Wind σb "~2 m/s per RTMA":** Table 1 gives σb for ψ/χ in m² s⁻¹ (118 494 / 115 572), not wind in m/s. The ~2 m/s is an inference (consistent with FG cv-RMSE 2.15 m/s) — mark as inferred, not published.
- **Horel & Dong "~3,000 stations":** abstract confirms **8,925 control analyses** and **>570,000 cross-validation experiments** (RAWS + NWS stations in ~4°×4° domains, summer 2008); the distinct-station count is not in the abstract and I could not verify ~3,000 (RAWS ~2,200 + NWS CONUS makes it plausible). Soften to "every RAWS and NWS station in each domain."
- **"ps σo loose by design — blacklists do the real work for pressure":** plausible inference; not stated in De Pondeca.
- **"CWOP's own guidance pushes Davis-style shields":** not verified against the CWOP guide PDF; low stakes.
- **"SPC never documented the Barnes parameters":** consistent with everything checked (no κ/γ in Bothwell 2002, the help page, or Coniglio 2012), but it's a negative claim — phrase as "no published values found in any primary source."
- **"neighbor-pair smearing" (Morris ceiling/vis):** Morris's actual mechanism is analysis increments of discrete, non-Gaussian variables being spread to nearby grid points by the climatological decorrelation length; rephrase.

## C. Verified verbatim (the load-bearing claims)

- **Analyze-then-derive (the #1 question): CONFIRMED.** Bothwell et al. 2002: the program "'builds' the elements of a vertical sounding (with temperature, moisture, u and v wind and vertical motion) every 25 mb in the vertical at every grid point (currently over 17000 grid points at 40 km resolution)"; "Many of the sounding analysis routines used are those from … NSHARP"; "over 225 combined surface and upper-air fields"; "time matched forecast RUC 2 data at 25 mb vertical increments is combined with the surface objective analysis." SPC help page: "Each gridpoint is inputed into a sounding analysis rountine called 'NSHARP' to calculate about 100 new fields" [sic]. Parameters are computed from analyzed gridded soundings, never analyzed from station-computed parameters.
- **SFCOA background roles: CONFIRMED verbatim.** Bothwell: "The RUC 2 forecast fields, in addition to supplying a first quess [sic] … aid in the analysis by filling in data void areas both inland and off-shore. The RUC 2 data are also used as a quality control measure. Observations that depart significantly from these guess fields are excluded from the analysis." Coniglio 2012: both spec quotes ("…use the RUC 1-h forecast surface fields to quality control the surface observations prior to the Barnes analysis"; "the Barnes analysis of the surface data does not use the RUC fields in any way during the analysis passes") are exact. Worth adding: Coniglio notes SFCOA "primarily uses METAR and marine observations," mesonets "on rare occasions" — bowecho's CWOP/mesonet ingestion goes **beyond** SPC, closer to RTMA/UU2DVar practice.
- **RTMA Table 2 (obs errors + R): all 16 cells CONFIRMED** (METAR 1.0/7.5, 5.9/5.0, 5.4/5.0, 1.6/7.0; synoptic 1.0/5.0…1.6/5.0; mesonet 1.2/7.0, 7.1/3.0, 6.5/3.0, **4.8/5.0**; marine 1.0/5.0, 5.9/5.0, 5.4/5.0, 2.6/5.0), incl. the duplicate/MADIS-flag inflation caveat and the pseudo-RH gross-check note. All max-departure arithmetic (7.5/8.4 K; 11.2/8.0/24/13 m/s; 29.5/21.3%; 27 hPa) checks out.
- **RTMA QC stack: CONFIRMED** — 4 layers as described; gross check re-evaluated at the start of each of **two outer loops** (50 inner each); 13 Jun 2007 D.C. cold-pool loop-1-reject/loop-2-readmit case; 4-part blacklist incl. dynamic list "constructed using the gross-error statistics from the previous three to six analyses"; trusted-provider/trusted-station lists from RUC (Benjamin et al. 2010); "Mesonet winds are only used in the assimilation if they belong to at least one of these lists"; flag rates 10/25/55% with "about 60% of the Mesonet wind stations fail"; Morris adds variational QC (Purser 2011, 2018) and complex-terrain gross-check relaxation, and notes mesonet winds are now "rejected by default" on CONUS unless tested-in.
- **Covariances: CONFIRMED** — S⁻¹ = I/Lh² + (∇H)(∇H)ᵀ/Lf² (Eq. A2), Riishøjgaard (1998) variant, Purser et al. (2003a,b) recursive filters, deliberately weak wind anisotropy, and the 500-m effective-elevation drop on "major water bodies."
- **Cross-validation: CONFIRMED** — 5 disjoint ~10% subsets, Hilbert curve, 15–29 Nov 2009 at 3-h intervals; Table 3 exactly: T 1.85→1.38 K (34%), SH 0.68→0.47 g/kg (45%), ps 1.60→1.24 hPa (29%), WSP 2.15→1.86 m/s (16%); IMPROV = 100(FG−ANL)/ANL; the overestimation caveat verbatim. Station count 14,299 (84.5% mesonet) ✓.
- **Morris 2020: CONFIRMED** — HRRR background with T/ps/moisture = HRRR+3-km-NAM-nest blend, RAP edge fill; ≤15 kt wind-substitute finding; ceiling/visibility unusable for IFR; pressure suspect in complex terrain; "the assimilation of mesonet observations introduces a negative wind speed bias relative to Part 139 observations" (verbatim); BCRMSE = √(RMSE²−bias²) (Eq. 3); CONTROL/EXP/NODA design; RTMA-RU 15-min/~13-min latency; URMA T+6h, NBM calibration/validation; Knopfmeier & Stensrud (2013) EnKF-wind superiority and 75% mesonet-denial results.
- **Tyndall & Horel 2013: CONFIRMED** — innovation check |y−H(xb)| ≤ max[εm·stddev(xb, r≤40 km), tqc] with εm=10 and tqc = 3/4 °C, 7.5 m/s (verbatim, Eq. 5); light-wind check (<1 m/s obs vs >5 m/s background, noting NWS minimum report 1.25 m/s); manual blacklist; Dee (2005) bias grids per hour-of-day, γ=0.15; Table 1 ratios all match (NWS/FED+ 1.0; RAWS 2.0; PUBLIC 6,808 stns 1.5/1.5/2.0; AG 1.5/1.5/2.0; AQ/EXT/LOCAL/TRANS 1.5; HYDRO 2.0); RAWS 6-m masts; PUBLIC residence-siting quote verbatim; "the assigned error variance ratios play a tertiary role"; Buxton NC **D6557** vs KHSE (the paper itself typos "D6657" once); ratios estimated in Tyndall et al. (2010) via Lönnberg & Hollingsworth (1986) on RUC innovations; background = 13→5-km downscaled RUC.
- **MADIS: CONFIRMED in full** from the archived FSL page, incl. TSP 88-21-R2 (1994) provenance, all validity/temporal limits, OI buddy check (Belousov et al. 1968) on obs-minus-background residuals with previous-hour **RSAS** background, nearest-in-8-directional-sectors neighbor search, single-neighbor-removal exoneration (target "good," neighbor "suspect," suspects excluded from subsequent OI), θ conversion with Miller & Benjamin (1992) elevation-dependent correlations, threshold = f(forecast, measurement, analysis error), and reject/accept list subjective override.
- **Coniglio 2012 / Steeves 2012 / Coniglio & Jewell 2022: CONFIRMED** — wind too fast <1 km AGL, too slow 2–4 km; PBL too shallow (the **−300 m** number and the T/Td rmsd quartet **1.0/1.5 vs 1.5/2.0 K are from Steeves et al. 2012** — "biases are low on the order of −300 m AGL"; "SFCOA has rmsd value of 1.5 K for dew point temperature, 1 K for temperature … RUC01 corresponding values of 2 K and 1.5 K" — attribute them there; Coniglio 2012's own text says T mean errors "mostly <1 K," moisture "mostly <2 K," PBL shallow in 26/40 soundings); SBCIN/MLCIN residual high bias ~15 m² s⁻²; CIN rmsd 60–90 m² s⁻²; "largest positive impact … is on reducing the bias in most of the thermodynamic fields" (the spec's bias-reduction thesis, verbatim support); C&J22: 257 soundings (143 NONTOR + 114 TOR), near-ground (≤500 m) SR-wind/shear underestimate from vertical-resolution limits, low-level CAPE too low near storms, dry bias above PBL, missing shallow near-ground stable layers.
- **Local-code assessment: CONFIRMED against source** — `qc.py`: 3-pass chain; gross T 8 / Td 10 / wind 12 (per component) / MSLP 6; buddy = raw T/Td vs neighbor median, 1.5° radius, min 3 buddies (spreads 6 °C T / 8 °C Td); MSLP validity 945–1055; KDTree nearest-neighbor H; no temporal/elevation/light-wind/blacklist/bias checks. `barnes.py`: κ=5.052e8, γ=0.3, 2 passes, innovation-based successive correction on a projected grid, cutoff 4√κ. All of the spec's critiques of this code are factually grounded.

## D. Suggested additions surfaced during verification

1. Madaus's outlier discard (mean diff >5 hPa or σ>2 hPa vs reference) → add to QC chain step 7.
2. The :45 SFCOA "final" run QCs against the RUC **analysis** rather than the 1-h forecast — a free accuracy upgrade bowecho can copy when HRRR analyses are available.
3. De Pondeca: actual obs error is **inflated for duplicates and in response to MADIS flags** — duplicate handling matters once CWOP+NWS+mesonet feeds overlap (same station via multiple aggregators).
4. Tyndall's note that Mount-Rainier-type sensor-vs-grid elevation mismatch is exactly what the variability term protects — supports the spec's bilinear-near-coasts caveat and suggests also skipping the gross check where |station − grid elevation| is extreme for T (not just pressure).

