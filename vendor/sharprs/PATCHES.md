# BowEcho vendor lineage — sharprs

## Import record

- Source path (local `cargo` git checkout the copy was taken from):
  `C:/Users/drew/.cargo/git/checkouts/rusty-weather-d817ad55a9d6ba26/cf0ca36/vendor/sharprs`
- Upstream repository: `https://github.com/FahrenheitResearch/rusty-weather`
- Upstream revision: `cf0ca3693650153ff0f79a0e60c3ec2da7d0af01`
- Upstream path within that repository: `vendor/sharprs`
- Imported on: 2026-08-16
- Import scope: the complete crate directory, copied verbatim. Only build
  artifacts were excluded (`target/`, `.git`); neither was present in the
  source checkout, so nothing was actually dropped. The copy was verified
  byte-for-byte against the source with `diff -r`.
- Imported: 36 files, 20,448 lines of Rust across 24 `.rs` files
  (`src/` plus `tests/`), and 5 Python comparison harnesses under `tests/`.

The crate's own upstream provenance — its earlier import into `rusty-weather`
from Fahrenheit Research's internal codebase, and its SHARPpy heritage — is
recorded by upstream in [`PROVENANCE.md`](PROVENANCE.md), which is preserved
unmodified. [`BOWECHO-VENDOR.md`](BOWECHO-VENDOR.md) explains why BowEcho
vendors this crate and how the workspace `[patch]` tables wire it in.

## Local modifications

The initial import was byte-for-byte identical to
`rusty-weather@cf0ca3693650153ff0f79a0e60c3ec2da7d0af01:vendor/sharprs`
(verified with `diff -r`).

`diff -r` against `cf0ca36:vendor/sharprs` now reports exactly:

```
Only in vendor/sharprs: BOWECHO-VENDOR.md      (BowEcho-owned, added)
Only in vendor/sharprs: PATCHES.md             (BowEcho-owned, this file)
Only in vendor/sharprs: assets/                (BowEcho-owned, added — change 1)
Files ... Cargo.toml ... differ                (change 2)
Files ... src/render/canvas.rs ... differ      (change 1)
Files ... src/render/compositor.rs ... differ  (changes 3-6)
Files ... src/winds.rs ... differ              (change 4)
```

Everything else — all of `src/params/`, `src/thermo.rs`, `src/profile.rs`,
`src/interp.rs`, `src/fire.rs`, `src/watch_type.rs`, `src/utils.rs`, the rest
of `src/render/`, and all of `tests/` — remains byte-identical to upstream. No
upstream file has been deleted or reformatted.

Any future BowEcho change to the vendored source must be appended to the change
log below with the affected files, the reason, and the date, so the delta
against `cf0ca36:vendor/sharprs` stays reconstructible.

### Change log

#### 2026-08-16 — vendoring only, no behavior change

1. **`src/render/canvas.rs`** — the font `include_bytes!` path.

   Upstream reads its label font out of the enclosing rusty-weather monorepo:

   ```rust
   include_bytes!("../../../../crates/rustwx-render/assets/fonts/SourceSans3-Regular.ttf")
   ```

   That path resolves to `<rusty-weather>/crates/rustwx-render/...`, which does
   not exist once the crate stands alone; the first build after vendoring
   failed with `couldn't read ... (os error 3)`. The identical font file was
   copied into the crate at `assets/fonts/SourceSans3-Regular.ttf` (verified
   byte-identical with `cmp` against
   `cf0ca36:crates/rustwx-render/assets/fonts/SourceSans3-Regular.ttf`,
   431,196 bytes), the font's `SourceSans3-LICENSE.md` was copied alongside it,
   and the include path was changed to `../../assets/fonts/…`. The compiled-in
   bytes are unchanged, so all rendered text is pixel-identical.

2. **`Cargo.toml`** — added `[lints.rust]` / `[lints.clippy]` opt-out.

   BowEcho gates with `cargo clippy -p app_ui --all-targets -- -D warnings`,
   and cargo applies the workspace clippy driver to every workspace member.
   Upstream sharprs produces 36 lib-level clippy errors at that level
   (`too_many_arguments` ×12, `if_same_then_else` ×4, `excessive_precision` ×4,
   `manual_range_contains` ×4, `doc_lazy_continuation` ×4, `manual_clamp`,
   `unnecessary_cast`, `neg_multiply`, `collapsible_if`, `io_other_error`,
   `type_complexity`). Cleaning them up would mean rewriting vendored science
   code and destroying the auditable diff against upstream, so this one package
   allows its own lints instead. Every BowEcho-owned crate keeps `-D warnings`
   untouched. This is a build-configuration change with no effect on generated
   code.

Known cosmetic residue, deliberately NOT changed: upstream's `[profile.release]`
stanza in `Cargo.toml` is inert for a non-root workspace member, so cargo prints
`warning: profiles for the non root package will be ignored` on every command.
It was left in place to keep the manifest faithful to upstream; deleting those
three lines is a safe future cleanup.

#### 2026-08-16 — composite parity fix (behavior change, intended)

The OA mesoanalysis map paints its composite grid from
`render::compositor::compute_all_params`, while BowEcho's sounding window uses
`sharppyrs`. The two disagreed, and the map was the wrong one: SCP, STP(CIN)
and VTP came out at exactly `0.0` across the warm sector because both of the
storm-relative inputs they multiply through were computed differently here than
in every other SHARPpy implementation.

The corrected code is ported from `sharppyrs` at the *same* rusty-weather
revision (`cf0ca36:vendor/sharppyrs`) — a known-good implementation already in
this dependency graph, not a fresh derivation. Sources are cited per change.

3. **`src/render/compositor.rs`** — EBWD separated from the inflow-layer shear.

   `ComputedParams` carried a single `effective_bwd`, computed as the shear
   from the effective inflow base to the **top of the inflow layer**, and fed
   that value to `stp_cin`, `scp` and `vtp_mod` as their EBWD term. SHARPpy
   keeps two separate quantities, and only the second is EBWD
   (`sharppyrs/src/derived.rs:273-283`):

   ```rust
   d.eff_shear = shear(inner, prof.ebottom, prof.etop);            // base -> top
   let depth = (mupcl.elhght - prof.ebotm) / 2.0;                  // EBWD depth
   let elh = p_at(prof.ebotm + depth);
   d.ebwd = shear(inner, prof.ebottom, elh);                       // base -> elh
   ```

   In `src/params/composites.rs` the EBWD term is a bare multiplicative factor
   with a hard floor in all three consumers:

   | function  | line | term                        |
   |-----------|------|-----------------------------|
   | `stp_cin` | 140  | `ebwd < 12.5 m/s  => 0.0`   |
   | `scp`     | 375  | `ebwd < 10.0 m/s  => 0.0`   |
   | `vtp_mod` | 306  | `ebwd <= 20.0 m/s => 0.0`   |

   Understating EBWD therefore does not merely bias those composites — below
   the floor it zeroes them outright. On the verification fixture below the old
   value was 14.27 m/s and the corrected one is 26.56 m/s, so VTP was returning
   an exact `0.0` there while SCP and STP(CIN) were scaled down by ~46%.

   The fix adds a new `ComputedParams::eff_shear` field holding the previous
   base->top value (unchanged, still m/s) and recomputes `effective_bwd` as the
   true EBWD, mirroring the `derived.rs` block above including its guard on
   `mupcl.elhght`. The param table's "Eff Inflow" shear row now reads the new
   `eff_shear` field so the table and the struct cannot drift apart; its
   displayed number is unchanged. The repeated `0.514_444` literal became a
   named `KTS_TO_MS` constant (same value).

4. **`src/winds.rs` + `src/render/compositor.rs`** — parcel-based Bunkers
   storm motion.

   `compute_all_params` called `winds::non_parcel_bunkers_motion` on every
   profile. SHARPpy's `ConvectiveProfile` only uses that as a fall-back; when
   an effective inflow layer exists it uses the parcel-based Bunkers et al.
   (2014) motion — the 7.5 m/s deviation applied to the pressure-weighted mean
   wind from the inflow base to 65% of the MU parcel EL. That motion sets every
   storm-relative quantity, so the whole SRH/EHI/SCP/STP/VTP family inherited
   the error.

   `winds::bunkers_storm_motion` is a port of
   `sharppyrs/src/extras.rs:760-783` (itself a port of SHARPpy
   `params.bunkers_storm_motion`). Two mechanical adaptations, no numeric
   change: the QC predicate is sharprs' own `profile::is_valid` rather than
   sharppyrs' `utils::qc` (equivalent except for the unreachable
   `(-9999, -9990]` band), and the deviation magnitude uses this crate's
   `utils::ms2kts(7.5)`, which expands to the identical `7.5 * 1.94384449` the
   port hardcodes and matches the sibling `non_parcel_bunkers_motion`.

   The call site adopts the gating from `sharppyrs/src/profile.rs:197-211`:
   parcel-based when the effective inflow layer is valid, `non_parcel` when it
   is not.

5. **`src/render/compositor.rs`** — effective SRH no longer requires a
   strictly deeper-than-zero inflow layer.

   The guard was `eff_bot_h.is_finite() && eff_top_h.is_finite() && eff_top_h >
   eff_bot_h`. A one-level effective inflow layer is legitimate and
   `winds::helicity` already returns `(0, 0, 0)` for a zero-thickness layer, so
   the extra comparison only produced `None` — NaN holes on the composite grid.
   It is now a finiteness check.

6. **`src/render/compositor.rs`** — `exact: true` for all four helicity calls.

   SHARPpy's `helicity()` integrates over the sounding's own levels plus
   interpolated endpoints. All four call sites here passed `exact: false`,
   which resamples at fixed `dp`. Numerically near-inert on dense profiles, but
   flipped for parity with `sharppyrs/src/extras.rs::helicity`, which has no
   inexact mode.

**Verification.** `crates/app_ui/src/oa_derived.rs::tests::`
`compositor_matches_sharppyrs_storm_motion_and_ebwd` runs one real HRRR point
sounding (sharppyrs' own `testdata/hrrr_example.rs` golden fixture) through
both implementations and compares them. Before these changes the map disagreed
with the sounding window by 46.3% on EBWD (27.73 kt vs 51.63 kt), 48.8% on
Bunkers RM v, 14.8% on Bunkers RM u and 3.4% on effective SRH. After them every
compared quantity agrees to 0.00%.
