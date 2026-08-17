# sharprs

SHARPpy-equivalent sounding analysis and rendering in pure Rust.

17,784 lines. 352+ tests. Zero Python dependencies. Full Skew-T/hodograph/parameter rendering to PNG without matplotlib.

<p align="center">
  <img src="examples/SHV_example.png" width="800" alt="SHV Shreveport LA — Skew-T analysis">
</p>

## What is this?

A complete reimplementation of [SHARPpy](https://github.com/sharppy/SHARPpy)'s sounding analysis engine and display in Rust. Takes a raw sounding (pressure, height, temperature, dewpoint, wind) and produces:

- Every SPC severe weather parameter (STP, SCP, SHIP, SHERB, and 12 more)
- Full parcel analysis (CAPE, CIN, LCL, LFC, EL for SB/ML/MU/FCST parcels)
- Complete wind analysis (Bunkers, Corfidi, SRH, bulk shear, critical angle)
- Publication-quality Skew-T/log-P diagrams with hodograph
- All rendered natively in Rust — no Python, no matplotlib, no external plotting libraries

## Quick start

### CLI

```bash
cargo run --release --bin sharprs-render -- sounding.csv output.png
```

Input formats: CSV (`PRES,HGHT,TMPC,DWPC,WDIR,WSPD`), SHARPpy `%RAW%/%END%`, University of Wyoming text.

### As a library

```rust
use sharprs::profile::Profile;
use sharprs::params::cape::parcelx;
use sharprs::winds;
use sharprs::render::compositor::{compute_all_params, render_full_sounding};

// Load a sounding
let prof = Profile::from_csv("sounding.csv")?;

// Compute parameters
let params = compute_all_params(&prof);
println!("SBCAPE: {:.0} J/kg", params.sb_cape);
println!("STP:    {:.1}", params.stp_cin);
println!("SCP:    {:.1}", params.scp);

// Render to PNG
let png_bytes = render_full_sounding(&prof, &params);
std::fs::write("output.png", &png_bytes)?;
```

### Python (via PyO3)

```bash
pip install maturin
cd sharprs && maturin develop --features python
```

```python
from sharprs import Profile
from sharprs.params import cape_cin, stp_fixed, scp
from sharprs.winds import bunkers_motion, helicity

prof = Profile(
    pres=[1000, 925, 850, 700, 500, 300, 200],
    hght=[100, 800, 1500, 3100, 5600, 9200, 11800],
    tmpc=[30, 25, 20, 10, -10, -40, -60],
    dwpc=[22, 18, 15, 5, -15, -45, -65],
    wdir=[180, 190, 210, 240, 260, 270, 270],
    wspd=[10, 15, 25, 35, 45, 55, 60],
)

cape, cin = cape_cin(prof)
```

## What's computed

### Parcel analysis
| Parameter | Description |
|-----------|-------------|
| CAPE / CIN | Convective Available Potential Energy / Convective Inhibition |
| LCL / LFC / EL | Lifted Condensation Level / Level of Free Convection / Equilibrium Level |
| DCAPE | Downdraft CAPE |
| Effective Inflow Layer | Thompson et al. 2007 |

Parcel types: Surface-Based, Mixed-Layer (100mb), Most-Unstable, Forecast, User-Defined.

### Composite parameters
| Parameter | Reference |
|-----------|-----------|
| STP (fixed & CIN) | Significant Tornado Parameter — Thompson et al. 2003/2012 |
| SCP | Supercell Composite — Thompson et al. 2004 |
| SHIP | Significant Hail — Johnson & Sugden 2014 |
| SHERB | Severe Hazards in Environments with Reduced Buoyancy — Sherburn et al. 2014 |
| MMP | MCS Maintenance Probability — Coniglio et al. 2006 |
| WNDG | Wind Damage Parameter |
| DCP | Derecho Composite — Evans & Doswell 2001 |
| EHI | Energy-Helicity Index |
| SWEAT | SWEAT Index — Miller 1972 |
| ESP | Enhanced Stretching Potential |
| SIG_SEVERE | Significant Severe — Craven & Brooks 2004 |
| LHP | Large Hail Parameter — Johnson & Sugden 2014 |
| MOSHE | Modified SHERBE — Sherburn & Parker 2014 |
| MBURST | Microburst Composite Index |
| TEI | Theta-E Index |

### Wind analysis
| Parameter | Description |
|-----------|-------------|
| Bunkers Storm Motion | Right-mover, left-mover, mean wind |
| Corfidi MCS Vectors | Upshear and downshear propagation |
| Storm-Relative Helicity | 0-1km, 0-3km, effective layer |
| Bulk Wind Shear | Any layer, fixed or effective |
| Critical Angle | Esterheld & Giuliano 2008 |
| Mean Wind | Pressure-weighted and non-pressure-weighted |

### Stability indices
K-Index, Total Totals, Cross/Vertical Totals, Precipitable Water, Lapse Rates (4 layers), Mean Mixing Ratio, Mean Theta/Theta-E, Convective Temperature, Max Temperature, Mean Relative Humidity, Wet-Bulb Zero, Dendritic Growth Zone, Hail Growth Zone, Coniglio MCS Maintenance.

### Fire weather
Fosberg Fire Weather Index, Haines Index (low/mid/high elevation).

### Watch type classifier
Fuzzy-logic watch type classification: PDS Tornado, Tornado, Marginal Tornado, Severe, Marginal Severe, Flash Flood, Blizzard, Excessive Heat.

## Rendering

The renderer produces a 2400x1800 pixel PNG matching the SHARPpy/Pivotal Weather display layout:

**Skew-T/Log-P diagram:**
- Red temperature trace, green dewpoint trace, gold/orange parcel traces
- Dry adiabats, moist adiabats, mixing ratio lines, isotherms
- CAPE/CIN area shading
- LCL/LFC/EL labels with height markers
- Effective inflow layer bracket
- Cyan wind barbs
- Omega profile, wet-bulb trace

**Hodograph:**
- Height-colored wind trace (red/orange/yellow/green/blue/purple by km)
- Bunkers RM/LM markers with labels
- Corfidi upshear/downshear vectors
- Speed rings, critical angle annotation

**Parameter table:**
- Four-parcel comparison (SFC/ML/FCST/MU)
- Seven-layer shear/helicity table
- Full index suite with color-coded severity
- Storm motion vectors, composite parameters

**Diagnostic panels:**
- SARS sounding analogs
- Effective Layer STP box-and-whisker climatology
- Storm slinky (updraft tilt visualization)
- Possible hazard type classification

All rendering is done with a custom software rasterizer — Wu's antialiased lines, bitmap font text, alpha blending, wind barb drawing. No GPU required. No external font files.

## Performance

On a typical 100-200 level sounding:

| Operation | Time |
|-----------|------|
| Parse CSV + compute derived fields | < 1 ms |
| Full parameter computation | < 1 ms |
| 2400x1800 PNG render | ~50 ms |
| **Total** | **~52 ms** |

SHARPpy (Python + matplotlib) takes 3-8 seconds for the same output.

## Project structure

```
src/
├── constants.rs        Physical constants
├── error.rs            Error types
├── thermo.rs           Thermodynamic functions (18 functions)
├── interp.rs           Log-pressure interpolation
├── utils.rs            Unit conversions, wind vectors
├── winds.rs            Storm motion, SRH, shear
├── profile.rs          Sounding data structure + I/O
├── fire.rs             Fire weather indices
├── watch_type.rs       Watch type classifier
├── params/
│   ├── cape.rs         Parcel lifting, CAPE/CIN integration
│   ├── composites.rs   STP, SCP, SHIP, SHERB + 11 more
│   └── indices.rs      K-Index, TT, PW + 18 more
├── render/
│   ├── canvas.rs       RGBA software rasterizer
│   ├── skewt.rs        Skew-T/Log-P diagram
│   ├── hodograph.rs    Polar hodograph
│   ├── param_table.rs  Dense parameter table
│   ├── panels.rs       SARS, STP box, slinky, hazard
│   └── compositor.rs   Multi-panel assembly
├── python.rs           PyO3 bindings
├── main.rs             CLI binary
└── lib.rs              Library root
```

## License

MIT AND BSD-3-Clause. The Rust implementation is MIT-licensed; portions that
port or derive from SHARPpy remain subject to SHARPpy's BSD-3-Clause notice.
See [LICENSE](LICENSE) for both notices.
