# Dealiasing fold-branch failure — root-cause analysis (2026-06-10)

## Field report
KMBX, tornado-warned QLCS, ~23:47–23:54 UTC June 9: the dealiased velocity
display showed a large outbound (red) blob covering much of the warning
polygon where RadarScope (dealiased) showed uniform inbound flow. Transient —
gone the next volume.

## Reproduction
`KMBX20260609_235423_V06` (kept at `radar-work/data/`). The
`dealias_blob_probe` example finds the failure:

```
region 600:  size 826  mean −23.4  fold +1 → unfolded +29.1   (WRONG)
region 2660: size 1652 mean +22.5  fold −1 → unfolded −30.0   (correct)
```

The neighbouring volume `KMBX20260609_234055_V06` contains a verified-correct
unfold (az 202°, 57.9 km: raw +23.5 → −33.2, matching RadarScope's −68 mph) —
it is the positive regression case. `velocity_point_probe` samples both.

## Root cause chain (each hypothesis tested on the real volume)
1. **Not** a raw-display issue (that was the earlier, separate report — fixed
   with the RAW VEL chip + Nyquist warning).
2. **Not** the vote math or the offset union-find (algebra verified; the
   relative folds inside the failing subgraph are self-consistent).
3. **Mega-region chaining**: gradual noise chains a single 166k-gate region
   across the whole ±Nyquist span; truly-aliased +22 patches join it and
   anchor at fold 0, forcing genuinely-inbound −23 neighbours to fold UP
   (+29 outbound). Py-ART-style velocity-interval splitting
   (Helmus & Collis 2016) splits the mega-region but the bad branch persists
   through the vote graph.
4. **Vote-graph subgraph misbranch**: one spurious edge misbranches a whole
   internally-consistent subgraph — invisible to boundary evidence and to
   per-region/sub-cluster mismatch checks (tested: both pass on the wrong
   solution).
5. **Single-sweep VAD reference is circular here**: a Browning & Wexler
   zeroth-harmonic fit per range band (the UNRAVEL-style external reference)
   was implemented and gated on fit quality. Diagnostics showed healthy fits
   (68/75 bands) that **endorse the wrong folds** — because the sweep is so
   widely aliased that wrapped gates (true −35 → raw +15) pass the
   |v| ≤ 0.7·Nyq reliability filter and poison the fit. In the regime where a
   reference is needed most, a same-sweep reference is contaminated.

All five attempts were reverted; the shipped algorithm remains the proven
baseline (derecho: 21,458 → 196 fold boundaries; all unit tests; the 23:40
positive case).

## Tilt-cascade engine (SHIPPED as the second, opt-in engine)
Higher tilts often carry higher Nyquist velocities and less aliasing, so the
cascade dealiases top-down, branch-checking each tilt against the
Browning & Wexler 1968 zeroth-harmonic fit from the tilt above (`cascade.rs`;
selectable as "Cascade (beta)" next to Unfold VEL; region stays the default).
Unit-tested: recovers a 35 m/s uniform wind under Nyquist 20 from a clean
Nyquist-40 tilt above (the designed regime), and falls back to the plain
region engine on single-tilt volumes.

### Validation results on the captured set (2026-06-10)
- `KEAX...055143` (derecho): unchanged, 196 residual boundaries. ✓
- `KMBX...234055` az 202/57.9 km: raw +23.5 → −33.2 (correct) on BOTH
  engines. ✓
- `KMBX...235423` blob regions: **cascade does NOT fix this event** — per
  `cut_probe`, this VCP runs Nyquist 26.2 m/s on EVERY tilt to 6.6°
  (near-Nyquist fractions 10–20% all the way up); the only clean tilts
  (8–20°, Nyq 28–33) sample completely different altitudes and fail the
  coverage gate. A TEMPORAL reference (fit from the previous volume's
  dealiased field — `dealias_temporal_probe`) also fails: the previous
  volume (23:40) contains the SAME misbranch (az 316/21 km,
  raw −17.5 → +39.2, 16k gates — it is persistent across volumes, not
  transient), so the reference inherits the error (GIGO).

### Remaining truth (the third engine, future)
For events where deep strong flow aliases every usable tilt, no self-derived
reference (same sweep, upper tilt, previous volume) can disambiguate the
branch. The discriminator must be EXTERNAL: model winds (RAP/HRRR surface or
925 mb at the radar site — a tiny fetch) seeding the range-band reference,
after which the existing branch + per-region machinery applies unchanged.
That is the designed third engine; the `RangeBandReference` seam in
`dealias_velocity_grid_with_reference` is exactly where it plugs in.

Validation set for any future engine work (all must pass):
- `KMBX...235423`: blob regions (az 339/20°N sector) must end inbound,
  never +1-folded outbound.
- `KMBX...234055` az 202/57.9 km: raw +23.5 → ≈ −33.
- `KEAX...055143` (derecho): ≤ ~250 residual fold boundaries.
- Unit suite incl. `region_dealias_recovers_smooth_folded_ramp`.

## Operational note (current behavior)
The failure class is transient and now characterized. When a velocity blob
looks suspicious: toggle **Unfold VEL** off — the raw field plus the
"RAW VEL — folds possible" chip and the near-Nyquist inspector warning make
the underlying truth visible immediately.
