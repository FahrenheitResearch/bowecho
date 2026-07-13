# SimSat in BowEcho

BowEcho embeds SimSat v0.2.0 as a first-class simulated-satellite workspace.
Open **Windows > SimSat** to render WRF or HRRR native-level atmospheres into
visible, thermal, water-vapor, and derived satellite products.

Durable renders enter BowEcho's normal Satellite store and player. They can
loop by source run, follow onto the radar map, open in the native plotter, and
use Save PNG. A separate visible-only GPU preview is intentionally temporary
and does not enter saved loops.

## Quick start

1. Choose **Local WRF / GRIB**, **Downloaded HRRR**, or **Download HRRR**.
2. Choose a product.
3. Choose **Geostationary (from space)** or **Top-down map**.
4. For geostationary output, choose GOES-East, GOES-West, or Himawari.
5. Choose Model native, ABI 1 km, or ABI 2 km output resolution.
6. Start with **Recommended** Quick mode, or select **High Quality** / **Sensor
   QA** for their explicitly described use cases.
7. Choose **Render to Satellite**.
8. Inspect the completed run in Satellite, or choose **Open native plot** for
   plotting and PNG output.

Use **GPU preview** only when iterating on the appearance of one visible frame.
Use **Render to Satellite** for any frame that should persist or join a loop.

## Quick modes and render intent

Quick modes apply reviewed settings without changing the selected source or
product. They also preserve earth margin, the forced/automatic Blue Marble
month, and the sun override. Recommended and High Quality preserve the current
view, satellite, and navigation; Sensor QA selects the geometry it requires.
The displayed Quick-mode name covers the controls owned by that preset. For an
actual-time baseline, separately choose an automatic ground month and turn off
the what-if sun override. All individual controls remain visible, and edits to
preset-owned controls make the current mode **Custom**.

- **Recommended Display** restores the reviewed visible baseline: CPU offline
  quality, Model native, CompactU8, exposure 1.5, AOD 0.05, cloud OD 0.15,
  the deterministic two-subcolumn fractional-cloud closure, fixed particle
  optics, the reviewed 4.0 low-sun land-normalization bound, exposed-edge
  feathering and top-down ground-shadow anti-aliasing on, and experimental
  footprint/transport/post-light controls off.
- **High Quality Visible** starts from Recommended Display, then selects the
  deterministic four-subcolumn reference and a 0.45 highlight knee. It is
  slower and is not a blanket claim of greater model accuracy.
- **Sensor QA** accepts Visible or GOES-East IR Band 13 only. Visible uses the
  explicitly limited `simsat-fast-gray-v1` operator on exact GOES-R navigation
  at ABI 1 km. GOES-East IR uses exact navigation, ABI 2 km, and the official
  FM4/GOES-19 Band 13 spectral response. Incompatible products and satellites
  are refused rather than silently relabeled.

The **Intent** selector remains independently available. **Display** preserves
the reviewed SimSat look. **Sensor Fast Gray** neutralizes display-only
exposure, land, edge, highlight, granulation, and reconstruction transforms on
a temporary request. After a successful render the SimSat pane reports every
adjustment and science warning. Those notices are not embedded in Satellite
store frames or PNG metadata. Sensor Fast Gray requires CPU and is not a
complete ABI/AHI channel observation operator.

## Input modes

### Local WRF / GRIB

The local path can point to:

- one readable WRF/NetCDF file
- an ordinary extensionless **wrfout_*** file from any domain
- a multi-time file
- an HRRR native-level GRIB2 file
- a SimSat **run.json** manifest
- a folder containing a sequence

The File picker is deliberately unfiltered because normal WRF output often has
no extension. Folder inputs are probed and sorted by valid time. Files that do
not look like WRF/GRIB sequence candidates are ignored. A mixed folder can skip
an unreadable candidate; if no readable frame remains, BowEcho reports an
actionable empty-input error. During the job, BowEcho shows the latest frame
error and a final completed/failed count while retaining successful frames; it
does not keep an on-screen ledger of every earlier failure.

### Downloaded HRRR

This mode discovers reusable HRRR native-level inputs in:

- BowEcho's SimSat input directory
- BowEcho's model download cache

Use **Refresh** after another process writes a new file.

### Download HRRR

Choose a UTC date, cycle, and forecast hour. **Latest candidate** fills the
newest likely available cycle for that forecast hour. Rendering first acquires
the NOAA HRRR **wrfnat** product using resumable cache reuse, then ingests and
renders it.

### Why wrfnat is required

SimSat needs the native three-dimensional cloud, condensate, moisture,
temperature, height, terrain, and related fields. BowEcho's smaller
pressure-level plus surface model pair cannot reconstruct that atmosphere.
Selecting those files should fail with a clear input explanation; it must not
pretend to produce equivalent cloud physics.

Downloaded wrfnat files are retained separately so later renders and cache
upgrades do not require another download.

## Products

| Product | Stored output | Interpretation |
|---|---|---|
| Visible true color | RGB | Physically lit visible atmosphere, cloud, terrain, ground, and water |
| SimSat day / night color (GeoColor style) | RGB | Broad RGB by day, band-13 IR by night, blended through the terminator; not yet sensor-derived ABI GeoColor |
| Sandwich | RGB | Visible texture combined with enhanced cold cloud tops |
| IR 10.3 micrometers (band 13) | Kelvin scalar | Clean-window brightness temperature |
| WV 6.2 micrometers (band 8) | Kelvin scalar | Upper-tropospheric water vapor |
| WV 6.9 micrometers (band 9) | Kelvin scalar | Mid-tropospheric water vapor |
| WV 7.3 micrometers (band 10) | Kelvin scalar | Lower-tropospheric water vapor |
| Precipitable water | Millimeters | Integrated atmospheric water column |
| Cloud-top temperature | Kelvin | Retrieved cloud-top temperature |
| Cloud optical depth | Dimensionless | Quantitative model-derived optical depth |

Visible-family display calibration does not change raw IR, water-vapor, or
derived scalar values. Derived products use fixed physical palettes across
frames, which keeps a loop comparable instead of rescaling each image
independently.

BowEcho recolors stored Kelvin IR/WV frames live in the Satellite window.
SimSat v0.2.0 adds **Natural (NOAA heritage)** for longwave IR: NOAA's
continuous bi-linear grayscale display transfer. It is available in BowEcho's
IR-enhancement picker, while existing BowEcho settings retain their persisted
**CIMSS** default instead of being silently migrated. Natural is a display
palette only; the stored Band-13 brightness-temperature plane remains Kelvin.
For water-vapor bands, Natural deliberately uses each band's scaled grayscale
rather than forcing the Band-13 breakpoint.

## View geometry

### Geostationary

The output uses the selected satellite's fixed-grid viewpoint:

- GOES-East
- GOES-West
- Himawari

**Earth margin** controls how much real surrounding ground is visible around
the finite model domain. Weather outside the model domain is clear, not
extrapolated.

**Navigation** selects either the shipped WRF/model sphere or the opt-in exact
GOES-R ellipsoid/sweep-x fixed grid. Exact GOES-R navigation is CPU-only and is
not available for Himawari. It improves registration geometry; it does not by
itself make the radiometry sensor-exact.

### Top-down map

Top-down output is registered to the model's own projection and can be placed
directly on a map. Satellite choice is ignored because rays are vertical
rather than emitted from a geostationary viewpoint.

SimSat v0.2.0 now integrates the top-down camera-to-cloud atmospheric column
directly. The final cloud composite includes surface transmission, cloud
radiance transmission, and the airlight in front of the cloud. This replaces
the older top-down simplification that omitted front airlight and is mirrored
by the CPU and GPU render paths. It remains SimSat's approximate atmosphere,
not a measured or line-by-line sensor retrieval.

The BowEcho integration exposes geostationary and top-down views. SimSat
Studio's separate free perspective camera is not exposed as a BowEcho
satellite-store product.

## Resolution and quality

Resolution modes:

- **Model native**: one output pixel per source grid cell
- **ABI 1 km**: physical 1 km spacing for top-down or corresponding scan pitch
  for geostationary
- **ABI 2 km**: physical 2 km spacing or corresponding scan pitch

Top-down output preserves physical aspect ratio when the output limit is
reached.

Quality modes:

- **Final (384 steps)**: normal stored-frame quality
- **Preview (192 steps)**: faster CPU integration with the same product
  contract

Both quality choices use the durable CPU renderer when **Render to Satellite**
is selected.

## Science, precision, and thermal response

### Brick precision

**CompactU8** is the production default and writes SSB v6. The optional
**ScienceCloudF16** profile stores hydrometeor extinction as bounded log2-f16
values in an isolated v7 cache. It is CPU-only, uses more disk, and re-ingests
the retained source after switching. It can be selected for visible, thermal,
water-vapor, or derived output.

### Band 13 spectral response

**Fast gray** preserves the historical 10.3 micrometer center-wavelength
response. **GOES-R ABI Band 13 (FM4/GOES-19 SRF)** integrates Planck emission
through NOAA's official FM4 response and uses that response for BT inversion.
Cloud and water-vapor absorption remain SimSat's gray approximation, so the UI
and result provenance retain that science limitation.

### ABI Band 13 MTF footprint

The optional footprint applies a GOES-16-measured east-west MTF-informed
three-tap response to complete FM4 channel radiance before BT inversion. It
automatically selects geostationary exact GOES-R navigation, ABI 2 km, FM4,
and CPU. It is experimental: transfer to GOES-19 is unvalidated, while
north-south MTF, temporal integration, and detector variation are unmodeled.

## CPU render and GPU preview

### Render to Satellite

- supports every product
- can render one frame or a sequence
- writes successful frames into BowEcho's Satellite store
- groups a source sequence into one loop
- can follow onto the radar map
- retains native plot and PNG export

The first use of a raw source ingests a reusable SimSat volume brick. A full
HRRR native-level file can briefly require more than 2 GiB of system memory.

### GPU preview

- supports Visible true color only in the BowEcho pane
- renders only the first selected frame
- opens the result in Native plot
- reports every temporary compatibility substitution
- does not modify the user's saved controls
- never writes to the Satellite store
- never becomes a saved loop frame
- treats a missing/unsupported adapter as an error rather than silently
  substituting a durable CPU render

This boundary prevents a fast approximation from being mistaken for the
tested batch/stored product.

## Sequence progress and cancellation

Progress displays completed frames versus the discovered total. Use
**Cancel after current frame** to stop at a safe boundary.

- an active CPU render finishes its current frame
- an HRRR download can stop between resumable chunks
- frames already written remain valid
- cancellation does not claim an incomplete sequence is complete

## Atmosphere and surface controls

### Exposure

Finished-visible display gain. SimSat v0.2.0 ships at 1.5; 1.0 is neutral
physical reflectance. Exposure does not modify raw scalar products.

### Aerosol optical depth

Visible aerosol optical depth at 550 nm. Zero removes aerosol extinction but
keeps molecular Rayleigh scattering.

### RH aerosol swelling

Applies SimSat's documented humidity-growth multiplier to aerosol extinction.
The UI displays the effective AOD when enabled.

### Daytime aerial-veil correction

Reduces modeled daytime path airlight for finished true color. Disabling it
retains the full modeled veil.

### Terrain-height atmosphere

Shortens view and sunlight atmospheric columns to each model pixel's terrain
height. Enabled is the physical shipped path.

## Cloud controls

### Volumetric clouds

Disabling this removes the rendered cloud volume from visible-family output.

### Use model cloud fraction

When available and internally consistent:

- WRF uses CLDFRA
- HRRR uses its native 50-level cloud-fraction field

Both use maximum-overlap vertical remapping. Missing or contradictory coverage
falls back conservatively. Turning the control off restores the legacy
horizontally full-cell interpretation wherever condensate is nonzero.

### Cloud transport

- **Legacy octaves (shipped)**: established bright-anvil multiple-scatter
  approximation
- **Single scatter**: dimmer diagnostic path
- **Delta-flux v1**: experimental isotropic closure, CPU-only
- **Delta-flux v2b P1**: experimental brightness-neutral P1 closure, CPU-only
- **Delta-flux v3 memory**: experimental bounded second-order angular-memory
  closure, CPU-only

Experimental transport choices are research comparisons, not upgrades that
are automatically more accurate for every cloud.

### Fractional-cloud closure

**Deterministic 2** is the reviewed finished-display default. It performs two
fixed-stratified shared-u maximum-overlap cloud marches, averages linear
radiance, and tonemaps once. **Effective OD** remains the faster explicit
sensor-compatible/homogeneous choice. Deterministic 4, 8, and 16 retain their
higher-cost reference/convergence roles. These bounded deterministic closures
are not full stochastic or max-random McICA implementations, and additional
members do not guarantee a more accurate forecast cloud field.

### Particle optics

**Fixed radii** preserves the production v0.1.6 behavior. The opt-in NSSL MP18
and HRRR Thompson modes use scheme-native saved moments where valid and fall
back per cell. Each mode uses a distinct cache. They are visible-only because
thermal mass recovery still depends on the fixed-radius representation;
GeoColor, Sandwich, IR, and water vapor keep fixed optics.

### Cloud optical-depth scale

The visible-family scale ships at 0.15. A value of 1.00 uses unscaled model
extinction and 0 removes finished-visible cloud extinction.

The 0.15 value is an owner-selected cross-file visual calibration, not a
claimed universal physical optimum. It does not affect thermal products or
the quantitative Cloud optical depth field.

### Feather exposed domain edges

Enabled by default. It fades finished visible cloud extinction across the
outer band only when the camera exposes the finite WRF/HRRR boundary. Raw
visible bands, thermal, water-vapor, and derived products ignore it.

### Beer-powder shading

Optional cloud appearance shaping. It is off in the shipped preset because the
transport model already supplies brightness buildup.

### Sub-grid cloud granulation

Experimental and off by default. It adds subtract-only edge detail for
unresolved boundary-layer clouds. It cannot create scientifically resolved
weather and does not alter thermal or derived output.

### Top-down stratiform reconstruction

Experimental, opt-in, and top-down-visible only. It can reduce source-grid
rings in broad low/liquid decks while conserving selected-area optical depth.
It cannot recover missing cloud/clear structure and is ignored by:

- geostationary output
- raw bands
- IR and water vapor
- derived scalar products

### Top-down cloud footprint

Experimental, opt-in, and top-down-visible only. It applies a bounded
seven-tap filter to the pre-tonemap cloud-radiance residual while leaving the
terrain/base map sharp. It is CPU-only and ignored by geostationary, thermal,
derived, cloud-layer, and perspective output.

### Top-down shadow anti-aliasing

Enabled in Recommended Display. It applies a normalized 5 by 5 binomial filter
to the top-down ground cloud-shadow field in **transmittance** space, leaving
the raw sun-optical-depth field and cloud radiance unchanged. The sun-OD march
also permits up to 4096 samples in v0.2.0, raised from 1024, so fine vertical
grids are less likely to produce coherent dashed or ring-shaped ground-shadow
artifacts. Geostationary, raw-band, thermal, derived, and Sensor Fast Gray
paths keep the unfiltered contract. This is an anti-aliasing and sampling
improvement, not new meteorological detail or an instrument PSF.

## Lighting and ground

### Sun position

The normal sun follows source valid time. **Override sun (what-if)** uses a
chosen elevation and azimuth and is explicitly labeled non-physical. It is
appropriate for illumination experiments, not time-faithful comparison.

### NASA Blue Marble ground

SimSat uses NASA Blue Marble Next Generation monthly imagery. Missing 2 km
months can download lazily and are verified by SHA-256. Auto blends seasonal
ground for valid date; forcing month 1-12 is a what-if surface.

### Display calibration

Ground lift, highlight knee/ceiling, solar-zenith land normalization, and the
dark-land reflectance toe affect finished visible-family RGB. **Restore shipped
display calibration** returns those controls to SimSat v0.2.0 defaults. The
reviewed maximum gain for the low-sun land-normalization operator is now 4.0;
its existing elevation ramp keeps twilight and high-sun scenes neutral.

**Post-light surface toe (CPU)** is a separate, optional experiment. It applies
a bounded color-preserving lift to land after lighting and view attenuation but
before atmospheric airlight and cloud compositing. It is off by default, does
not alter water/glint or raw products, and is disabled by Sensor Fast Gray. GPU
preview cannot reproduce this experiment; use Render to Satellite for a frame
that must include it.

Raw visible bands, IR, water vapor, and derived products do not consume these
display-only controls.

## Cache behavior

BowEcho uses a versioned SimSat engine cache. SimSat v0.2.0 keeps the compact
production cache at **SSB v6**; this release does not force a compact-cache
format bump. The experimental ScienceCloudF16 profile uses a disjoint v7 cache.

Older brick caches cannot masquerade as v6:

- a retained WRF or HRRR source re-ingests once
- a retained wrfnat file does not need another download
- a cached-only old brick without its original source cannot be upgraded

The cache is an acceleration artifact, not the scientific source of record.
Product, view, quick/science, atmosphere, cloud, lighting, and display control
choices persist in BowEcho settings. Source paths, active jobs, progress,
errors, and rendered output deliberately do not.

## Satellite player, map, and native plot

Durable output uses the same store/player as real imagery. Equal source run,
product, view, and UTC-day values join one loop rather than creating one sidebar
run per frame. Rendering the same source/product/view at another resolution can
replace the same valid-time frame; resolution is not a separate run key.

**Map follows player** and **Show on radar map** work normally. Native plots
preserve:

- Kelvin values and physical colorbars for IR/WV
- mm, K, or optical-depth scalar values for derived products
- georeferencing and fixed cross-frame palettes
- RGB composites without a false scalar colorbar

The native-plot title identifies SimSat and the selected product, with source,
valid time, and view context in the plot subtitles. Save PNG captures that
visible plotting surface. Full render-operator provenance, science warnings,
and NASA ground-imagery credit are documented here and in BowEcho's in-app
Guide; they are not currently burned into the PNG or stored as PNG metadata.

## Scientific implementation

Visible true color combines:

- Hillaire-style clear-sky transmittance/multiple-scattering approximations
- volumetric cloud ray marching
- Wrenninge-style multiple-scatter octaves
- directional sky ambient
- finite-sun cloud and terrain shadows
- seasonal Blue Marble ground
- Cox-Munk wind-ruffled water glint
- an ABI-like display transform

SimSat v0.2.0 also uses a shared 0--12 degree low-sun surface-help ramp for
land normalization, dark-toe recovery, ground lift, and water-albedo
assistance; direct water sunlight is no longer artificially day-gated. The
reviewed land-normalization bound is 4.0, the display cloud-shadow floor is
0.45, and finite-sun shadow softening uses the Sun's angular radius. Top-down
ground shadows additionally use the reviewed transmittance-space anti-alias
filter and the higher 4096-step sun-OD ceiling described above.

IR uses a gray-body emission march through the model cloud/atmosphere plus a
surface term, then converts radiance to true-Kelvin brightness temperature.
Water-vapor bands use related band-averaged moisture weighting.

## Honest limitations

- Weather exists only inside the source model domain.
- Model-sphere geometry follows WRF's 6370 km sphere. Exact GOES-R navigation
  is available separately, but it does not imply exact sensor radiometry.
- Physical plausibility is the target; pixel-for-pixel registration against a
  real satellite is not promised.
- A coarse source grid cannot produce real cloud structure below its
  resolution.
- Visible rendering is physically based but is not a full atmospheric
  chemistry or line-by-line radiative-transfer model.
- IR/WV absorption is gray and band-averaged, not line-by-line.
- SimSat day/night color is GeoColor-style broad RGB, not yet sensor-derived
  ABI GeoColor. Its night side uses the IR composite; there is no city-lights
  layer.
- GPU output is a visible-only preview, not a stored quality tier.
- Natural NOAA longwave grayscale is a display transfer, not a change to the
  Kelvin thermal operator or proof of sensor validation. BowEcho preserves an
  existing persisted CIMSS choice until the user changes it.
- The top-down front-atmosphere march and shadow anti-aliasing improve the
  renderer's approximation; they do not add sub-grid weather, a measured
  instrument footprint, or line-by-line radiative transfer.
- The post-light surface toe is an optional CPU-only display experiment, not a
  scientific correction to model terrain or cloud fields.
- ScienceCloudF16, native particle optics, deterministic fractional clouds,
  the ABI MTF footprint, granulation, delta-flux transport, stratiform/cloud
  footprints, and sun override are opt-in and explicitly experimental,
  reference, or what-if where appropriate.

## Troubleshooting

### “Choose a WRF/GRIB file or folder first”

The Local source path is empty or does not exist. Use File/Folder or enter a
real path.

### HRRR input opens as unsupported

Confirm that the file is the native-level **wrfnat** GRIB2 product. Pressure
and surface products do not contain the required cloud volume.

### First render uses substantial memory

The source is being ingested into a reusable 3-D brick. A full HRRR native file
can briefly exceed 2 GiB. Close other large workloads or render a smaller WRF
domain.

### Old cache cannot open

SSB v6 needs the original source to rebuild the current cloud and snow
provenance. Point Local at the retained WRF/wrfnat file. A cached-only old brick
cannot be upgraded.

### GPU preview fails

Preview requires a compatible wgpu adapter and supports Visible true color
only. Sensor Fast Gray, ScienceCloudF16, exact GOES-R geostationary navigation,
instrument footprints, and the optional post-light surface toe are CPU-only.
Use Render to Satellite for the normal CPU result; BowEcho will not silently
turn a failed preview into a stored frame.

### Broad top-down clouds show model-grid rings

Try the experimental Top-down stratiform reconstruction only for broad
low/liquid decks. It is not a general sharpening control and cannot recover
unresolved cloud structure.

## Attribution

Simulated imagery is rendered by
[FahrenheitResearch/simsat](https://github.com/FahrenheitResearch/simsat),
licensed MIT OR Apache-2.0. Ground imagery is NASA Blue Marble Next Generation,
courtesy NASA Earth Observatory.

Implementation lineage includes Hillaire atmosphere rendering,
Frostbite/Nubis volumetric clouds, Wrenninge multiple-scatter approximations,
and Cox & Munk water-glint statistics. See the SimSat repository for its
license notices and detailed implementation references.
