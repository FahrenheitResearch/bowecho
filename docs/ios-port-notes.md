# iOS / iPadOS Port Notes

Working notes for the iPad port attempt (owner is a credentialed iOS
developer; first target = TestFlight build for M1+ iPads, though any
A12+ device is the same arm64 + Metal story).

## Why this is feasible

- The entire engine is pure Rust and already ships for arm64 Apple
  hardware (macOS Apple Silicon build): all decoders (Level II, DORADE,
  ODIM-H5, CFRadial), the region-based dealiaser, TOR tracks / TDS math,
  the CPU rasterizer, OA/composites, the georef OCR.
- Graphics is wgpu → Metal is a first-class backend. The 3D volume
  explorer's WGSL shaders and the egui paint pipeline carry over.
- Dependency tree is nearly C-free. The exceptions compile fine for iOS
  targets: `bzip2-sys` (libbz2), `ring` (rustls' crypto — has iOS
  support). No OpenSSL anywhere.

## The actual work

1. **Shell**: eframe's iOS support is not turn-key. Path: thin Xcode app
   wrapping a static lib built for `aarch64-apple-ios`, driving
   eframe/winit (winit supports iOS; mind the UIApplication lifecycle —
   suspension, scene phases). Survey current state of `eframe` iOS
   examples / `cargo-xcodebuild` / community egui-on-iOS templates
   before hand-rolling.
2. **Touch pass** (the real port): every hover and right-click needs a
   touch equivalent.
   - Inspector hover card → tap-to-pin already exists (Shift+click pins);
     make plain tap pin on touch devices.
   - Right-click menus ("lowest beam here", tab Float/Hide, FARM placing)
     → long-press.
   - egui handles pinch-zoom/two-finger natively; verify map pan vs
     annotation-draw disambiguation under touch.
   - The docking workspace + layer rail are touch-friendly already
     (buttons, sliders); density may want a touch profile (bigger
     hit targets — could be a style-registry profile, "Touch").
3. **Paths**: `settings::config_dir()` and the data dirs need iOS
   sandbox equivalents (`NSApplicationSupportDirectory` etc. via the
   `dirs` crate or objc bindings). All directory logic is centralized in
   `crates/settings` + `app_cache_root()` in main.rs — one small PR.
4. **ATS**: several live feeds are plain http (svr.guru images are
   https; some GR2A poll roots like the WILU/radar-side Furuno hosts are
   http) — needs `NSAppTransportSecurity` exceptions in Info.plist, or
   scope to https feeds initially.
5. **Lifecycle**: live polling/follow threads suspend on background —
   accept (standard mobile), re-sync on foreground (the pollers already
   handle gaps; verify the GLM/sat follow engines resume cleanly).
6. **Distribution**: TestFlight first. Note the GBW-derived annotation
   vocabulary credit (README Credits) must ride along into the App Store
   listing's acknowledgements.

## Build sketch

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
# static lib target in a new thin crate (crates/app_ios) exposing a
# C ABI start fn; Xcode project links it. Or evaluate eframe's own
# ios template if one has matured.
```

## Open questions for the Mac session

- eframe-on-iOS maturity as of mid-2026 (check eframe CHANGELOG/issues).
- Whether to reuse the egui App wholesale on day 1 (desktop layout on a
  13" iPad is plausibly fine) and defer the touch profile, vs. doing the
  touch pass first.
- Metal performance of the CPU rasterizer path: the texture upload path
  is the same as desktop; profile on-device early.
