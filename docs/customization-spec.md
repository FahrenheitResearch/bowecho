# BowEcho Customization System — Implementation Spec

**Status:** ready for implementation, designed to land alongside the UI overhaul (`docs/ui-refresh-proposal.md`, direction A "everything is a layer" + layer rail).
**Audited:** `crates/app_ui/src/main.rs` (22,698 lines), `crates/color_tables/src/lib.rs` (2,794 lines), `crates/settings/src/lib.rs` at workspace v0.14.1, eframe 0.34.3. Line numbers below are anchors at this audit and will drift; function names are the durable references.
**Owner directive:** customizable polygon colors/line thickness, color table overhaul with better defaults plus in-app editing and saving, the green-to-red radar-age ring as a customization, and generally "freedom to customize anything."

## 0. Findings from the audit that shape this design

These are the facts the spec is built on; the implementing agent should verify each before starting:

1. **`palette_by_family` is dead state.** `AppSettings.palette_by_family` (crates/settings/src/lib.rs:26) is serialized, tested (lib.rs:287), and *never read or written by app_ui* — color table choices do not survive a restart today. User-loaded `.pal` files are read from an arbitrary path (`load_color_table_path`, main.rs:8403) and never copied anywhere. **The single highest-value fix in this whole spec is making palette choice persist.**
2. **Every style the owner mentioned is a hard-coded constant.** `hazard_color` (main.rs:18894) — match on family + damage threat, including TOR EMERGENCY purple `(150,50,250)`, PDS magenta `(255,64,175)`, SVR DESTRUCTIVE `(252,122,28)`. Stroke widths `2.4`/`1.5` and stroke alphas `245`/`205` inline in `build_hazard_overlay_shapes` (main.rs:14300–14308). `DEFAULT_HAZARD_FILL_ALPHA: u8 = 24` (main.rs:133). SPC outlook fill alpha `36` / stroke alpha `230` (spc_layers.rs:92–93), outlook stroke width `2.0` (main.rs:12862). Storm report colors in `ReportKind::color` (spc_layers.rs:58). GLM ramp in `glm_layer::flash_color` (glm_layer.rs:128). Obs plot colors (main.rs:13012–13056: METAR dot `(210,214,220)`, mesonet `(214,176,96)`, T red `(255,120,110)`, Td green `(120,235,130)`, gust amber `(255,196,110)`), declutter cell `88.0`. Site markers (main.rs:14928). Halo text (`draw_halo_text`, main.rs:16760).
3. **The radar-age ring already exists.** `freshness_ring_color` (main.rs:15692) colors the data-range ring green→yellow→red→dark-red using `FRESH_RING_{GREEN,YELLOW,RED}_SECONDS = 360/600/900` (main.rs:76–78), with tests at main.rs:21018. The LIVE/STALE chip (`draw_mode_chip`, main.rs:13380) uses a *separate, inconsistent* hard-coded 8-minute threshold. The owner's ask is therefore: surface it, customize it, and make it visible at storm zoom (today the only age ring is at full data range, ~230 km — off-screen exactly when you're zoomed into a hook echo).
4. **The hazard shape cache must learn about styles.** `build_hazard_overlay_shapes` results are cached keyed on a hash that includes `hazard_fill_alpha` (main.rs:14242) but obviously not future style state. Any style registry must contribute a generation/signature to that key or edits won't repaint.
5. **The `.pal` dialect is fully understood and GR2A-faithful.** Parser handles `Product:`, `Units:` (kt/mph→m/s scaling), `Scale:`, `Step:` (legend ticks only in GrPal mode), `RF:`, `Color:`/`Color4:`/`SolidColor:`/`SolidColor4:`, two-color gradient rows, `;`/`#`/`$$` comments (color_tables/src/lib.rs:137–229, 1376–1461). There is no writer. `parse_gr_pal` is what user files get (main.rs:15935). Export is therefore a small, well-defined addition.
6. **`color_tables` has zero dependencies** (its Cargo.toml has an empty `[dependencies]`). Keep it that way — no serde in that crate; the style registry lives elsewhere.
7. **The render pipeline already supports per-product table swaps.** `render_color_tables_for_product` (main.rs:4264) clones the `ColorTableSet` per render request and mutates family slots (velocity flip, display thresholds); `color_table_signature` flows into render keys. Per-product palette overrides are a three-line injection at this exact point.
8. **The UI overhaul gives us the surfaces.** `layer_row()` (main.rs:16140) is the unified row grammar; the proposal's end state has a LAYERS tab with per-row `[⚙]` gears and a ⚙ Settings tab. The Guide window (guide.rs) has a section nav ready for a "Customize" section. The proposal also bans silent keybind changes and the owner dislikes tours — discoverability is hover text + Guide.

---

## 1. Style registry architecture

### 1.1 Decision: a new `crates/styles` crate, persisted to a separate `styles.json`

**Not folded into `AppSettings`/config.json.** Justification:

- **A profile is a file.** Presets/profiles (§4) and community sharing demand "export my look as one JSON." `config.json` mixes machine-local and semi-private state (favorites, startup site, placefile URLs — SpotterNetwork feeds carry personal position data, `poll_url`). Folding styles in means every export needs a field-by-field filter that must be maintained forever. A separate document makes export = serialize, import = deserialize.
- **Different write cadence and blast radius.** Style edits happen mid-drag (color pickers, width sliders). `config.json` is saved from ~12 call sites already. Keeping the documents separate means a styles-save bug can't corrupt site/placefile state and vice versa.
- **Schema versioning without entangling AppSettings.** `AppSettings` deliberately has "unknown fields fall back to default" semantics with no version field. Styles need real migrations (slots will be added every release).
- **Crate placement:** new crate `crates/styles`, depending on `serde`, `serde_json`, and `settings` (for the config-dir path helper — add `pub fn bowecho_config_dir() -> Option<PathBuf>` to settings, refactoring the existing private `config_dir()`/`bowecho_dir()`). `app_ui` depends on `styles`. `color_tables` stays dependency-free.

What *does* go in `AppSettings` (config.json): `style_profile: String` (active profile name, default `"BowEcho default"`), and the now-actually-wired `palette_by_family` plus new `palette_by_product` (§2.4) — palette *bindings* are session state; palette *contents* are files.

File layout under the config dir:

```
%APPDATA%\bowecho\
  config.json            (AppSettings, unchanged location)
  styles.json            (StyleSettings — the live working style document)
  styles.json.bak        (written once before any schema migration rewrites)
  color_tables\*.pal     (user tables, "My tables")
  profiles\*.json        (saved/imported profiles)
```

### 1.2 Sparse overrides, not full documents

Every slot resolves as **built-in default ← user override**. Only overrides serialize (`Option<T>` + `skip_serializing_if`). This is non-negotiable for one reason: the owner will keep improving defaults (he just did, with the damage-threat escalation colors). If we serialize full resolved values, every user who ever opened the appearance panel is frozen on the defaults of the version they first ran. With sparse overrides, default improvements flow to everyone who hasn't touched that specific slot, and "Reset to default" = `None` (delete the key), not "copy a constant back."

### 1.3 The schema (concrete)

```rust
// crates/styles/src/lib.rs

pub const STYLES_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleSettings {
    pub schema: u32,                                   // 0 in old files => treat as 1
    /// Hazard polygon overrides. Keys: family ids ("tornado", "severe-thunderstorm",
    /// "flash-flood", "flood", "special-marine", "snow-squall", "watch",
    /// "mesoscale-discussion", "local-storm-report", "special-weather",
    /// "text-polygon", "other") AND escalation subkeys
    /// ("tornado/catastrophic", "tornado/considerable",
    ///  "severe-thunderstorm/destructive", "flash-flood/catastrophic").
    pub hazards: BTreeMap<String, PolygonStyleOverride>,
    pub hazard_global: HazardGlobalOverride,           // stroke-width multiplier, default fill alpha, label size
    pub spc: SpcStyleOverride,
    pub reports: BTreeMap<String, MarkerStyleOverride>, // "tornado" | "wind" | "hail"
    pub placefiles: PlacefileStyleOverride,
    pub obs: ObsStyleOverride,
    pub range_rings: RangeRingStyleOverride,
    pub labels: LabelStyleOverride,
    pub radar_age: RadarAgeStyleOverride,
    pub glm: GlmStyleOverride,
    pub drapes: DrapeStyleOverride,
}

/// Colors are [r,g,b,a] u8 — NOT egui types; styles stays UI-toolkit-agnostic
/// and app_ui converts (Color32::from_rgba_unmultiplied).
pub type Rgba = [u8; 4];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolygonStyleOverride {
    #[serde(skip_serializing_if = "Option::is_none")] pub stroke_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")] pub stroke_width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] pub fill_color:  Option<Rgba>, // None = stroke color
    #[serde(skip_serializing_if = "Option::is_none")] pub fill_alpha:  Option<u8>,   // None = global hazard alpha
    #[serde(skip_serializing_if = "Option::is_none")] pub dash:        Option<DashPattern>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DashPattern {
    Solid,
    Dashed { dash: f32, gap: f32 },
    Dotted,
}
```

Property-level `Option`s matter: "keep the default TOR red but make all warning outlines 3 px" is one `hazard_global.stroke_width_scale` override; "make PDS TOR scream" touches only `hazards["tornado/considerable"].stroke_color`.

**Resolution order for hazards:** `hazards["family/escalation"]` → `hazards["family"]` → built-in default for (family, escalation). Each *property* resolves independently through that chain (an escalation override of stroke_color still inherits the family's fill_alpha override).

The full slot inventory v1 (all defaults are the current hard-coded values, verified in §0 — landing the registry is pixel-identical):

| Group | Slots (defaults) |
|---|---|
| `hazards` | per family + escalation: stroke color (the `hazard_color` table), stroke width (1.5; selected = width + 0.9 computed, see §1.4), fill color (= stroke), fill alpha (global 24), dash (Solid; **default Dashed{6,4} for `watch` and `mesoscale-discussion`** — the one deliberate default change, it matches operational displays and visually separates non-warning polygons; ships in PR 4, not PR 1) |
| `hazard_global` | `fill_alpha` (24, the existing slider's default), `stroke_width_scale` (1.0), `selected_boost` (width +0.9, alpha 245 vs 205), `label_font_px` (11.0, selected 12.0) |
| `spc` | `outlook_fill_alpha` (36), `outlook_stroke_alpha` (230), `outlook_stroke_width` (2.0), `use_spc_published_colors` (true; when false, a `BTreeMap<String, Rgba>` keyed by LABEL — TSTM/MRGL/SLGT/ENH/MDT/HIGH and prob labels — overrides fills) |
| `reports` | per kind: color (tornado `(235,60,60,255)`, wind `(90,140,245,255)`, hail `(80,200,100,255)`), size_px (5.0/3.5/3.5 effective today), outline color/width (black 1.0, tornado only today — normalize: all three get outlines) |
| `placefiles` | `line_width_scale` (1.0), `icon_scale` (1.0), `text_size_scale` (1.0), `default_show_text` (true) — multipliers, because placefiles carry their own colors/sizes and overriding them outright breaks the format's intent |
| `obs` | METAR dot, mesonet dot, T color, Td color, gust color, station-id color, value_font_px (11), small_font_px (9), barb stroke, declutter_cell_px (88) |
| `range_rings` | data-edge ring width (1.8 primary / 1.5 overlay), `color_mode`: `Age` \| `Fixed(Rgba)` (default Age — this is the existing freshness ring), site marker colors (selected `(88,210,245)`, idle `(106,132,154)`) |
| `labels` | town label font px, halo strength (maps to existing `bold_labels`; keep the bool in AppSettings for back-compat, registry reads it as the default), warning/marker halo color `(0,0,0,210)` |
| `radar_age` | §3: thresholds `[360, 600, 900]` s, colors green `(65,238,104)` / yellow `(238,218,62)` / red `(246,76,48)` / expired `(205,34,48)`, `ring_enabled` (true), `glyph_arc_enabled` (true, new), `glyph_arc_radius_px` (9), `stale_chip_seconds` (480 — today's 8 min) |
| `glm` | fresh color `(255,235,120,235)`, aged color `(255,75,10,85)` (the ramp endpoints of `flash_color`), `size_scale` (1.0), `window_minutes` (10) |
| `drapes` | initial radar opacity (1.0), initial GOES opacity, initial model opacity, min overlay alpha (`MIN_RADAR_OVERLAY_ALPHA`) — these are *defaults for new layers*; live per-layer opacity stays on the layer rows |

### 1.4 Resolved registry — hot-path discipline

`StyleSettings` is the document; the app holds a **`StyleRegistry`**: a fully-resolved, flat struct rebuilt on every edit (cheap — hundreds of fields), stored in `ViewerApp`, with:

```rust
pub struct StyleRegistry {
    resolved: ResolvedStyles,   // flat structs, plain field reads in draw code
    signature: u64,             // FNV/DefaultHasher over the override document
}
impl StyleRegistry {
    pub fn hazard_polygon(&self, family: &str, damage_threat: Option<&str>) -> &PolygonStyle;
    pub fn signature(&self) -> u64;
    // ... one accessor per group
}
```

- Draw code never does string map lookups per polygon per frame for fixed groups (obs, glm, rings) — those are direct field reads. Hazards do one map lookup per *record* per shape-cache rebuild, which is already amortized by the cache.
- `signature()` is hashed into `build_hazard_overlay_shapes`'s cache key (alongside the existing `hazard_fill_alpha` hash, which it subsumes) and into any other view-keyed shape caches that consume styles (`view_shape_key` callers that draw styled content: SPC, obs are immediate-mode so they're fine).
- `hazard_color(record)` (main.rs:18894) becomes `style_registry.hazard_polygon(&record.event_family, record.damage_threat.as_deref()).stroke_color` — keep a free-function wrapper taking `&StyleRegistry` so the existing tests (main.rs:21394+) migrate mechanically. The inline `2.4/1.5` widths and `245/205` alphas in `build_hazard_overlay_shapes` come from the same resolved style plus `selected_boost`.
- The existing `hazard_fill_alpha` slider in the Warnings panel stays — it now reads/writes `styles.hazard_global.fill_alpha` (override) instead of an app field, so it persists for free (today it resets every launch).

### 1.5 Versioning and forward compatibility

- `schema: u32` with `#[serde(default)]` (0 ⇒ 1). On load: if `schema > STYLES_SCHEMA`, **do not rewrite the file** — load what parses (serde tolerates unknown fields), set a session flag "styles file from a newer BowEcho; saving disabled to protect it" surfaced in the Appearance panel. If `schema < STYLES_SCHEMA`: copy to `styles.json.bak`, run stepwise migrations (`fn migrate_1_to_2(&mut Value)` style, operating on `serde_json::Value` so renames are explicit), then save.
- New slots in future versions are new `Option` fields/keys — old files simply resolve them to defaults; no migration needed. Migrations are only for renames/semantic changes.
- Load is best-effort like `AppSettings::load` (missing/corrupt file ⇒ all defaults, app always starts).

**Tests (styles crate):** round-trip with only-overrides serialization (assert a default document serializes to `{"schema":1}` modulo empty maps); property-level inheritance through the escalation→family→default chain; unknown-field tolerance; newer-schema load-without-save; migration backup creation.

---

## 2. Color table overhaul

### 2.1 Better defaults

Grounding: Kovesi, *Good Colour Maps: How to Design Them* (arXiv:1509.03700, 2015) — perceptual uniformity means equal data steps produce equal perceived color steps, and lightness should carry the ordering; Borland & Taylor, *Rainbow Color Map (Still) Considered Harmful* (IEEE CG&A 27(2), 2007) — uncontrolled rainbow hue cycles create false boundaries and hide real ones; the viridis lineage (van der Walt & Smith, SciPy 2015) and turbo (Mikhailov, Google AI Blog 2019) as the modern fixed versions; cmocean `balance` (Thyng et al., *True Colors of Oceanography*, Oceanography 29(3), 2016) for CVD-safe diverging.

**The opinionated position:** radar color tables are *not* a generic scivis problem. The NWS hue ladder (blue→green→yellow→orange→red→magenta) is a shared operational language — a spotter glancing at a stranger's screen must read 60 dBZ instantly. So: **defaults keep operational convention; perceptual-uniformity principles are applied *within* that constraint (controlled lightness at category boundaries, no accidental hue cycling, transparency below the noise floor); and a properly perceptual/CVD-safe table ships as a promoted, badged alternative for every family rather than as the default.** The current "Analyst * HD" set already embodies most of this (the velocity table's doc comment at color_tables/src/lib.rs:842–855 shows the reasoning was done); the overhaul is curation, gaps, metadata, and persistence — not a rip-and-replace.

Concrete default set per family (changes marked **Δ**):

| Family | Default table | Reasoning / changes |
|---|---|---|
| Reflectivity | `Analyst Reflectivity HD` (keep) | NWS hue order, transparent < 10 dBZ, magenta reserved ≥ 65, white ≥ 80 — already the overhauled table. **Δ add** `Turbo REF (smooth)`: a turbo-derived interpolated table for fine QLCS/velocity-couple structure work (turbo has near-uniform perceptual derivative vs jet's banding — Mikhailov 2019), badged "smooth". |
| Velocity / SRV | `Analyst Velocity HD` (keep as default) | Diverging around zero per operational convention (green inbound / red outbound), neutral-dark near zero so couplets read by chroma jump, near-white reserved for extremes. Not lightness-monotonic and that's a documented, deliberate trade (lib.rs:846–851). **Δ promote** `Balance VEL (CVD-safe)` (already implemented, modeled on cmocean balance / Kovesi CET-D: blue↔red axis robust to deuteranopia, lightness monotonic per arm) to second position with a "CVD-safe" badge and a picker description. |
| CC | **Δ `Analyst CC v2`** | The one default that genuinely earns a rework. Operational action is 0.80–1.05 (WDTD dual-pol training; debris signatures live at ρhv ≈ 0.5–0.8 co-located with rotation, melting layer at 0.90–0.97, uniform precip ≥ 0.97). v2 reallocates: collapse 0.20–0.60 (junk: birds/chaff/clutter) to two dark cool steps; hard hue break at 0.80 (the TDS boundary becomes a visible edge, per Kovesi's "feature boundaries deserve discontinuities, gradients don't"); finer warm gradations 0.95–1.00 so the melting-layer sag is readable. Stops: `(0.20: 38,38,46) (0.60: 70,58,140) (0.80: 0,150,190) — break — (0.82: 60,190,100) (0.90: 200,212,52) (0.95: 245,158,32) (0.97: 226,46,40) (0.99: 160,24,32) (1.00: 236,236,244) (1.05: 255,255,255)`. Keep current `Analyst CC` and `Analyst CC Debris` as alternatives. |
| ZDR | `Analyst ZDR` (keep) | Diverging about 0 dB with gray sphere-point, warm positives weighted to 0–4 dB — matches convention. **Δ tighten** the neutral band to ±0.25 dB (currently the gray point only sits at exactly 0.0 with ramps either side; add stops at −0.25/+0.25 so "near-spherical" reads as one flat gray band instead of a gradient — dry hail ID). |
| SW | `Analyst Spectrum Width` (keep) | Dark below ~4 m/s where most of the field lives; warm break into the 8+ turbulence/rotation range. Fine as-is. |
| Echo Tops / VIL / VILD / MEHS | keep | Threshold-anchored designs already cite Amburn & Wolf 1997 (VILD 3.5 g/m³ hail break) and report thresholds (MEHS breaks at 19/25/44/50 mm, per the severe criteria; cf. Witt et al. 1998 for MEHS itself). These are *categorically stepped on physical thresholds*, which is the right design — leave them. |
| KDP / PHI / AzShear / Generic | keep | Diverging-about-zero (KDP, AzShear) and monotonic (PHI) designs are sound. |

**Δ metadata for the picker.** Add to `color_tables` a catalog API (no new deps):

```rust
pub struct CatalogEntry {
    pub table: ColorTable,
    pub description: &'static str,   // one line: "NWS-convention stepped dBZ; default"
    pub badges: &'static [Badge],    // Default | CvdSafe | Classic | Smooth | HighContrast
}
pub fn builtin_catalog_for_family(family: ColorTableFamily) -> Vec<CatalogEntry>;
```

`builtin_tables_for_family` stays (back-compat); the picker switches to the catalog and renders each entry as: swatch strip (sample the table at N points into a small `Mesh` — cheap, immediate), name, badges, hover = description + summary. This is most of the perceived "overhaul": users can finally *see* tables before applying.

### 2.2 In-app editor

A dedicated `egui::Window` "Color table editor" (window = deep-config surface, consistent with the UI proposal's window rule), opened from the picker via "New table…" / "Edit a copy…" (built-ins are immutable; editing one forks "My <name>"). New module `crates/app_ui/src/table_editor.rs` (main.rs is 22.7k lines; new UI goes in modules).

State: `TableDraft { name: String, family: ColorTableFamily, units: String, rf: Rgba8, mode: DraftMode (Smooth | Stepped), stops: Vec<DraftStop { value: f32, color: [u8;4], end_color: Option<[u8;4]>, solid: bool }>, dirty: bool, live_preview: bool }`.

UI, top to bottom:
1. Name + family + units row (units combo from the family: dBZ, kt, m/s, dB, °/km, kft…; the editor edits in *declared* units and writes the `Units:` header — the existing loader's kt→m/s scaling then applies, so a velocity table authored in kt behaves exactly like a community .pal).
2. **Gradient preview bar**: full-width horizontal strip sampling a `ColorTable` built from the draft (rebuild on change only, cached by a draft hash; `ColorTable::from_parts` sorts/dedups — surface its `NotEnoughStops`/validation errors inline). Checkerboard backing so alpha reads. Value axis ticks. Clicking the bar inserts a stop at that value with the sampled color.
3. **Stop list**: one row per stop — `DragValue` (value, units suffix, drag speed scaled to family range), `color_edit_button_srgba` (Alpha::OnlyBlend), optional end-color (adds the GR two-color gradient row), "solid" checkbox (SolidColor semantics: hard band), delete. `[+ Add stop]` appends midway between the last two.
4. **Live preview** toggle (default on): on each draft change, set the working table into `self.color_tables.set_family(...)` + `clear_texture()` — identical to the existing palette-switch path, so the viewport, LUT path, and colorbar all follow via `color_table_signature`. On editor cancel, restore the previous table (keep a snapshot).
5. Buttons: **Save to My tables** (writes `.pal`, §2.3), **Export…** (rfd save dialog, same bytes), Revert, Close.

### 2.3 Save/load — user tables as GR2A-compatible `.pal`

**Writer** lives in `color_tables` (pure string formatting, zero deps):

```rust
pub fn to_gr_pal(table: &ColorTable) -> String
```

Mapping (dialect verified against the parser at lib.rs:137–229):
- Header: `Product:` (family→GR product code: Reflectivity→`BR`, Velocity→`BV`, SW→`SW`, CC→`CC`, ZDR→`ZDR`, PHI→`PHI`, KDP→`KDP`, ET→`ET`, VIL→`VIL`…), `Units:`, `Step:` (legend tick hint — pick a round step from the value range), `RF: r g b a`.
- Stops: alpha==255 and no end_color and not solid → `Color: v r g b`; alpha≠255 → `Color4: v r g b a`; end_color → 6/8-component `Color:`/`Color4:` row; solid → `SolidColor:`/`SolidColor4:`.
- Sample-mode fidelity: Stepped drafts export as `SolidColor:` rows (GR renders hard bands — identical); Smooth drafts export as plain `Color:` rows (GR ramps between rows — identical). **This means exported files are bit-faithful in GR2A with no nonstandard headers.** Internally, files re-load through `parse_gr_pal` (the existing user path, main.rs:15944), so BowEcho and GR2A see the same thing. No `Mode:` line in exports.
- First line comment: `; exported by BowEcho <version>` (parser skips `;`).

**Round-trip test (color_tables):** for each built-in and a synthetic alpha/gradient/solid table: `parse_gr_pal(to_gr_pal(t))` then assert `sample(v)` equality across a dense value sweep (not struct equality — stepped→solid conversion legitimately changes representation, sampling must not change).

**"My tables":** on startup and after any save/import, scan `%APPDATA%\bowecho\color_tables\*.pal` (`settings::bowecho_config_dir()` + new `color_tables_dir()` helper), parse with `parse_gr_pal`, map `Product:` code → family (new `pub fn family_for_product_code(&str) -> ColorTableFamily` in color_tables; unknown → Generic, and the picker additionally offers user tables under "all families" since community files often omit/mislabel Product). Picker structure per family: **Built-in** (catalog entries) / **My tables** (with per-table "Edit", "Delete", "Show in folder") / **Import…** (rfd; *copies* the file into the dir — fixes the current load-by-path-and-lose-it behavior).

### 2.4 Persisted bindings

- **Wire `palette_by_family` for real.** On any table apply (picker, editor live-apply excluded, editor save+apply included): `app_settings.palette_by_family.insert(family.label().into(), table_name)` + save. On startup, after the user-table scan: for each family, resolve the saved name against built-ins ∪ user tables; on miss (deleted file), fall back to default and log to `color_table_status`. Test: binding restore with a missing table name falls back cleanly.
- **Per-product overrides** (the owner's "per-product default binding"): `palette_by_product: BTreeMap<String, String>` (product label → table name) in AppSettings. Injection point: `render_color_tables_for_product` (main.rs:4264) — after the clone, if an override exists for `product.label()`, `set_family(product.color_family(), resolved_table)`. The colorbar uses the same resolved set, so it follows automatically. UI: the existing quick picker (`active_product_color_picker`, main.rs:8154) gains a small "bind to: ◉ family ○ this product" toggle — family stays the default mental model (REF/CREF share), per-product is for e.g. giving SRV a different table than VEL (both Velocity family today, a real community request pattern).

---

## 3. The radar-age ring

Keep the existing data-edge freshness ring; add the missing piece (age at storm zoom); unify the chip; make all of it one style group.

**Surfaces:**
1. **Data-edge ring** (exists): `draw_radar_layer`/`draw_radar_overlay_layers` keep calling `freshness_ring_color`, now parameterized: `freshness_ring_color(volume_time, now, alpha, &style.radar_age)`. Width from `range_rings`/`radar_age` style (1.8/1.5 defaults). `color_mode: Fixed` turns off age-coloring for users who want a plain ring.
2. **Site-glyph age arc** (new): a fixed-screen-radius arc (default 9 px, style `glyph_arc_radius_px`) centered on the radar site position of every *loaded* radar (primary + overlay layers), drawn in `draw_radar_layer`/`draw_radar_overlay_layers` next to the ring call. Geometry: arc starts at 12 o'clock, sweeps clockwise by `age / red_threshold` (clamped to full circle), stroke 2.5 px, color from the same `freshness_ring_color` gradient; once age > red threshold: static dark-red full circle, **no pulsing** (animation in the data area is noise during ops). Implementation: `egui::Shape::line` over ~48 arc points. Hover (small `interact` rect over the glyph): "KTLX scan age 4m 12s". This is the owner's "green to red ring for radar age" made useful at hook-echo zoom where the 230 km ring is off-screen. Toggle: `glyph_arc_enabled` (default **on**).
3. **LIVE/STALE chip** (`draw_mode_chip`, main.rs:13380): replace the hard-coded `age_min <= 8` with `style.radar_age.stale_chip_seconds` (default 480 — behavior-preserving). The chip's red "STALE" background color comes from the same style group's red. Chip thresholds and ring thresholds stay *separate fields* in the same group (they answer different questions: "is the feed refreshing" vs "how old is this scan") but live side-by-side in the UI so users see them together.

**Customization exposed (Appearance ▸ Radar age):** three threshold DragValues (minutes, validated ascending), three color buttons + expired color, ring on/off + width, glyph arc on/off + radius, chip stale threshold. Hover text on the group: "Colors the range ring and site arc by scan age: green = fresh, red = stale."

**Tests:** extend the existing `freshness_ring_color_tracks_scan_age` (main.rs:21018) to take a custom `RadarAgeStyle` (custom thresholds shift the boundaries; custom colors come through); arc sweep fraction math (age 0 → 0, age = red threshold → full circle, clamp beyond).

---

## 4. Presets / profiles

**Document:**

```jsonc
// profiles/chase-dark.json
{
  "format": "bowecho-style-profile",
  "schema": 1,
  "name": "Chase dark",
  "styles": { /* StyleSettings — sparse overrides */ },
  "palette_by_family": { "Reflectivity": "Dark Scope REF", "Velocity / SRV": "Balance VEL (CVD-safe)" },
  "palette_by_product": { },
  "embedded_tables": [ { "name": "My Custom REF", "pal": "Product: BR\n..." } ]
}
```

`embedded_tables` carries the `.pal` text of any *user* table referenced by the bindings, so a shared profile is self-contained — the single-file community-sharing requirement. On import: write embedded tables into `color_tables\` (name clash → prompt: replace / keep both with " (2)" suffix), then apply.

**Built-in profiles** (code-defined, not files; listed first in the switcher):
- **BowEcho default** — empty overrides, default bindings. (Switching to it = reset everything; this replaces a scary global "reset all" button with a natural action.)
- **GR2-classic** — bindings: `GR2Analyst Classic REF`, `GR2Analyst Classic VEL`; hazards: outline-only (global fill_alpha 0), stroke width 2.0, TOR `(255,0,0)`, SVR `(255,255,0)`, FFW `(0,255,0)` — the GR2A polygon language the community's eyes are calibrated to.
- **Chase dark** — `Dark Scope REF` + `Balance VEL`; thicker strokes (scale 1.4), label font +1, heavier halos, brighter obs values — tuned for a dim truck cab and sunlight glare.
- **Accessibility (CVD-safe)** — `Balance VEL`, `Turbo REF (smooth)`, report markers re-hued to a CVD-safe triad (blue/orange/teal), TOR/SVR distinction carried by width+dash not just hue (TOR solid 3 px, SVR dashed 2 px) — grounded in Kovesi/Thyng CVD guidance.

**Semantics:** switching a profile **replaces** `styles.json` content and the two binding maps, bumps the registry generation, clears textures (palette change) and shape caches. Subsequent edits modify the live document; the Appearance header shows `Profile: Chase dark (modified)` with **Save** (overwrite the named profile file; disabled for built-ins), **Save as…**, **Export…**, **Import…**. Active profile name persists in `AppSettings.style_profile`. No auto-sync back to profile files — a profile is a snapshot, the live document is the truth.

---

## 5. UI surfaces

Rule (one sentence, to prevent re-crowding): **every style slot has exactly one canonical home — the Appearance section of the ⚙ Settings tab — and layer-rail gear popovers are small filtered views onto the same slots.**

1. **⚙ Settings ▸ Appearance** (new collapsible section in `settings_panel`, main.rs:6550, beside Display/Color tables/Hotkeys/Performance):
   - Profile row: combo (built-ins + `profiles\*.json`) · Save · Save as… · Export… · Import….
   - Collapsible sub-groups mirroring the style schema: Warnings & polygons (a compact grid: one row per family — color button, width drag, dash combo; escalation rows indented under TOR/SVR/FFW), SPC & reports, Obs plot, Placefiles, Range rings & site markers, Radar age, Lightning, Labels & halos, Layer opacities.
   - Every overridden slot shows a small `↺` reset button (visible only when overridden — the affordance doubles as the "you changed this" indicator). Group header gets "Reset group".
   - Color widget: `color_edit_button_srgba`; widths/sizes: `DragValue` with clamped ranges.
2. **Color tables** stay their own Settings section (existing fold, main.rs:8325) upgraded per §2.3: catalog entries with swatch strips and badges, My tables, Import…, New table… / Edit a copy… → editor window. The quick picker in PRODUCTS (`active_product_color_picker`) gets the same swatch-strip rendering and the per-product binding toggle; its "Edit…" jump (already wired via `open_color_tables_request`) is unchanged.
3. **Layer rail gears** (after UI-overhaul steps 3/6 land): `layer_row` rows gain the proposal's `[⚙]` slot opening an anchored popover with that layer's 4–8 most-used slots and an "All appearance settings…" link that jumps to ⚙ ▸ Appearance with the matching group expanded (reuse the `open_color_tables_request` one-shot pattern — `open_appearance_group: Option<StyleGroup>`). Mapping: Warnings row → family colors mini-grid + fill alpha + width; obs row → dot/T/Td colors + font; GLM row → fresh/aged colors + size; placefile rows → the three multipliers; radar rows → ring + age arc toggles.
4. **Discoverability** (owner: no tours): hover text on every control, written as "what it changes + where it draws" ("Outline width for tornado warning polygons on the map"); a new **Guide ▸ Customize** section (guide.rs — add `GuideSection::Customize` to the enum/ALL/label/match, between Tools and Shortcuts) documenting: Appearance panel, profiles + the JSON sharing format, the color table editor, `.pal` import/export and GR2A compatibility, the radar-age ring meaning and thresholds, and the on-disk paths. The Sources/credits section gains the colormap literature credits (Kovesi, cmocean, turbo).
5. **Status-line honesty** (house style): applying a profile or table writes one-line confirmations into the existing `color_table_status` / `self.status` channels.

---

## 6. Migration plan — PR-sized, each shippable, ordered for the UI-overhaul interleave

Coordination: PRs 1–3 and 5 touch no sidebar layout and can run in parallel with UI-overhaul steps 1–5. PR 4 lands in whatever Settings surface exists at the time (the section is self-contained either way). PR 7 depends on the overhaul's `layer_row` keystone having landed.

| PR | Contents | Refactors | Tests |
|---|---|---|---|
| **1. styles crate + hazard wiring** | New `crates/styles` (StyleSettings, StyleRegistry, load/save/migration scaffold, `settings::bowecho_config_dir()` exposure). Wire hazards only: `hazard_color` → registry wrapper; stroke widths/alphas in `build_hazard_overlay_shapes` from `PolygonStyle` + `selected_boost`; registry `signature()` into the hazard shape-cache key; `hazard_fill_alpha` slider backed by the registry. **Zero visual change** (defaults = current constants). | `hazard_color(record)` → `hazard_color(registry, record)`; `DEFAULT_HAZARD_FILL_ALPHA` moves to styles defaults | styles unit tests (§1.5); **golden parity test in app_ui**: empty StyleSettings ⇒ resolved TOR EMERGENCY == `(150,50,250)`, PDS == `(255,64,175)`, SVR DESTRUCTIVE == `(252,122,28)`, widths 1.5/2.4, alphas 205/245 — pin every legacy constant |
| **2. remaining consumers** | SPC outlooks/reports (`spc_layers::ReportKind::color` → registry; fill/stroke alphas in `parse_outlook` move to draw time so style edits don't require refetch — pass alphas into `draw_spc_outlooks`), GLM (`flash_color(age, &GlmStyle)`), obs constants, range rings + site markers, halo/label styles, placefile multipliers, drape initial opacities, dash rendering for polygons (`Shape::dashed_line` over closed point lists). | `flash_color` signature; `OutlookFeature.fill/stroke` become base colors (alpha applied at draw) | golden parity per group; dashed-polygon shape-count smoke test |
| **3. palette persistence** | `color_tables_dir()` + scan; wire `palette_by_family` (write on apply, resolve on boot); `palette_by_product` map + injection in `render_color_tables_for_product`; Import… copies into the dir; picker "My tables" listing. Fixes the dead-state bug outright. | `load_color_table_path` gains a "copy to My tables" path; startup sequence in `ViewerApp::new` | binding restore incl. missing-table fallback; product-code→family mapping; per-product override reaches `color_table_signature` |
| **4. Appearance UI v1** | ⚙ ▸ Appearance section: profile row (built-ins only at this point), hazard grid, all PR-1/2 slots, `↺` reset affordances, hover text. The watch/MD dashed default ships here (release note it). | `settings_panel` gains the section; `open_appearance_group` one-shot | UI smoke; reset-clears-override |
| **5. table editor + .pal export + catalog picker** | `color_tables::to_gr_pal` + `family_for_product_code` + `builtin_catalog_for_family`; `app_ui/src/table_editor.rs` window; picker upgrade (swatch strips, badges, My tables edit/delete, New/Edit-copy); editor live preview through the existing palette-switch path. | picker rewrite in `color_table_panel` + `active_product_color_picker` | **export→`parse_gr_pal` sampling round-trip across all built-ins**; units (kt) round-trip; editor draft validation |
| **6. profiles full** | Profile files, Save/Save-as/Export/Import with `embedded_tables`, clash prompts, `style_profile` in AppSettings, "(modified)" indicator. | — | import-with-embedded-tables; clash suffixing; switch resets generation + clears caches |
| **7. radar-age completion + rail gears + Guide** | Site-glyph age arc + hover; chip threshold unification (`stale_chip_seconds`); Appearance ▸ Radar age group; layer-rail `[⚙]` popovers (requires overhaul `layer_row`); `GuideSection::Customize`. | `draw_mode_chip`, `freshness_ring_color` param; `layer_row` trailing slot | extend `freshness_ring_color_tracks_scan_age` for custom styles; arc sweep math |

Each PR compiles, ships, and improves the app alone; if the arc stops after PR 3 the owner has persistence + better polygon internals, after PR 4 he has the headline customization, after PR 5 the community has shareable tables.

**Test strategy summary:** (a) styles crate: pure serde/resolution/migration unit tests; (b) app_ui: golden parity tests pinning every legacy constant against an empty style document (the regression net for the whole refactor); (c) color_tables: writer round-trip via *sampling equality*, catalog completeness (every family has a Default-badged entry); (d) no screenshot infra exists — visual QA is manual per PR, the golden value tests are the substitute.

### Critical files

- `crates/app_ui/src/main.rs` — hazard_color/build_hazard_overlay_shapes, freshness_ring_color, draw_mode_chip, color_table_panel/picker, render_color_tables_for_product, draw_spc_*/draw_glm/draw_surface_obs, settings_panel, layer_row
- `crates/color_tables/src/lib.rs` — parser/dialect, built-ins + new `to_gr_pal`/catalog/`family_for_product_code`
- `crates/settings/src/lib.rs` — `palette_by_family` (dead today), config-dir helpers to expose, new `style_profile`/`palette_by_product`
- `crates/styles/src/lib.rs` — **new crate**: StyleSettings/StyleRegistry/profiles
- `crates/app_ui/src/guide.rs` — new Customize section; also `docs/ui-refresh-proposal.md` for interleave coordination
