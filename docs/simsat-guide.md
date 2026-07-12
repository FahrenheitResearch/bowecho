# SimSat in BowEcho

BowEcho embeds SimSat v0.1.6 as a first-class simulated-satellite workspace.
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
6. Leave the shipped visible controls in place for a first render.
7. Choose **Render to Satellite**.
8. Inspect the completed run in Satellite, or choose **Open native plot** for
   plotting and PNG output.

Use **GPU preview** only when iterating on the appearance of one visible frame.
Use **Render to Satellite** for any frame that should persist or join a loop.

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
actionable empty-input error. Per-frame render failures remain visible in job
progress while successful frames are kept.

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
| GeoColor day / night | RGB | Visible by day, band-13 IR by night, blended through the terminator |
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

## View geometry

### Geostationary

The output uses the selected satellite's fixed-grid viewpoint:

- GOES-East
- GOES-West
- Himawari

**Earth margin** controls how much real surrounding ground is visible around
the finite model domain. Weather outside the model domain is clear, not
extrapolated.

### Top-down map

Top-down output is registered to the model's own projection and can be placed
directly on a map. Satellite choice is ignored because rays are vertical
rather than emitted from a geostationary viewpoint.

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

Finished-visible display gain. SimSat v0.1.6 ships at 1.5; 1.0 is neutral
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

Experimental transport choices are research comparisons, not upgrades that
are automatically more accurate for every cloud.

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
display calibration** returns those controls to SimSat v0.1.6 defaults.

Raw visible bands, IR, water vapor, and derived products do not consume these
display-only controls.

## Cache behavior

BowEcho uses a versioned SimSat engine cache. SimSat v0.1.6 writes SSB format
v5, which carries corrected cloud-fraction provenance.

Older brick caches cannot masquerade as v5:

- a retained WRF or HRRR source re-ingests once
- a retained wrfnat file does not need another download
- a cached-only old brick without its original source cannot be upgraded

The cache is an acceleration artifact, not the scientific source of record.

## Satellite player, map, and native plot

Durable output uses the same store/player as real imagery. Equal source run,
product, view, grid, and UTC-day values join one loop rather than creating one
sidebar run per frame.

**Map follows player** and **Show on radar map** work normally. Native plots
preserve:

- Kelvin values and physical colorbars for IR/WV
- mm, K, or optical-depth scalar values for derived products
- georeferencing and fixed cross-frame palettes
- RGB composites without a false scalar colorbar

Save PNG exports the plotted title, valid time, map context, and attribution
carried by the normal plotting surface.

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

IR uses a gray-body emission march through the model cloud/atmosphere plus a
surface term, then converts radiance to true-Kelvin brightness temperature.
Water-vapor bands use related band-averaged moisture weighting.

## Honest limitations

- Weather exists only inside the source model domain.
- Earth geometry follows WRF's sphere (radius 6370 km), not the full real ABI
  navigation ellipsoid.
- Physical plausibility is the target; pixel-for-pixel registration against a
  real satellite is not promised.
- A coarse source grid cannot produce real cloud structure below its
  resolution.
- Visible rendering is physically based but is not a full atmospheric
  chemistry or line-by-line radiative-transfer model.
- IR/WV absorption is gray and band-averaged, not line-by-line.
- GeoColor night uses the IR composite; there is no city-lights layer.
- GPU output is a visible-only preview, not a stored quality tier.
- Granulation, delta-flux transport, stratiform reconstruction, and sun
  override are opt-in and explicitly experimental or what-if.

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

SSB v5 needs the original source to rebuild corrected cloud-fraction
provenance. Point Local at the retained WRF/wrfnat file. A cached-only old brick
cannot be upgraded.

### GPU preview fails

Preview requires a compatible wgpu adapter and supports Visible true color
only. Use Render to Satellite for the normal CPU result; BowEcho will not
silently turn the failed preview into a stored frame.

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
