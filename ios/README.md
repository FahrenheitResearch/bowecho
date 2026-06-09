# BowEcho for iOS

A native **SwiftUI + MapKit** radar app that reuses the existing BowEcho Rust
engine (Level 2 fetch, decode, and CPU rendering) compiled as a static library.

## Architecture

```
┌─────────────────────────────────────────────┐
│ SwiftUI + MapKit (ios/BowEcho/Sources)        │
│   ContentView · RadarMapView · RadarViewModel │
├─────────────────────────────────────────────┤
│ RadarEngine.swift  →  C ABI (ios/include)     │
├─────────────────────────────────────────────┤
│ crates/bowecho_ffi  (staticlib, C ABI)        │
│   reuses: data_source · nexrad_io · render2d  │
│           radar_core · color_tables           │
└─────────────────────────────────────────────┘
```

The Rust engine fetches the latest volume from the NEXRAD S3 buckets, decodes
it, and rasterizes a moment to a square RGBA image centered on the radar.
`RadarImageOverlay` georeferences that image onto the MapKit basemap. The huge
`app_ui` desktop crate (egui + embedded basemap) is **not** used on iOS.

## Prerequisites

- Xcode 16+, iOS 17+ SDK
- Rust with iOS targets:
  `rustup target add aarch64-apple-ios aarch64-apple-ios-sim`
- `xcodegen` (`brew install xcodegen`)

## Build & run (simulator)

```bash
cd ios
xcodegen generate
xcodebuild -project BowEcho.xcodeproj -scheme BowEcho \
  -sdk iphonesimulator \
  -destination 'platform=iOS Simulator,name=iPhone 16 Pro Max' \
  -derivedDataPath build CODE_SIGNING_ALLOWED=NO build
xcrun simctl boot "iPhone 16 Pro Max" 2>/dev/null || true
xcrun simctl install booted build/Build/Products/Debug-iphonesimulator/BowEcho.app
xcrun simctl launch booted research.fahrenheit.bowecho
```

The Xcode project has a pre-build script that runs `cargo build -p bowecho_ffi`
for the right target automatically, so opening `BowEcho.xcodeproj` in Xcode and
hitting Run also works.

## Run on a physical iPhone

Open `BowEcho.xcodeproj` in Xcode, select your device, and Run. Signing uses
team `X65S282G57` with automatic provisioning (bundle id
`research.fahrenheit.bowecho`). The first device build will also produce the
`aarch64-apple-ios` slice via the pre-build script.

## Status / roadmap

- [x] Rust engine cross-compiles to iOS (incl. bzip2 / libdeflate / rustls)
- [x] Fetch → decode → render latest volume over the C ABI
- [x] MapKit basemap with georeferenced radar overlay
- [x] Site picker (preset list) + product switch (REF/VEL/CC/ZDR/SW)
- [ ] Full site discovery via the engine (`bowecho_list_sites`)
- [ ] Tilt selection + derived products (composite, echo tops, VIL)
- [ ] Loop / animation over recent volumes
- [ ] Auto-refresh for live data; gesture-based site nearest-to-center
- [ ] Proper Mercator reprojection (tile overlay) for edge accuracy
