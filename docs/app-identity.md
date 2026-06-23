# App Identity / Brand Kit

## Data model

`settings::BrandConfig` is the persisted source of truth. It contains the display/short names, organization and tagline; website, repository, releases, support, donate, contact, and privacy links; an opt-in storage namespace; screenshot filename prefix and output-folder label; ten palette tokens; optional PNG/ICO/logo/watermark/share-background paths; domain feature labels; and social-card settings for original, 16:9, 1:1, and 9:16 layouts.

Built-in presets are **BowEcho default** and **California Wildfire Tracking (CWT)**. CWT uses the supplied community framing and ships without third-party logos or an official-government claim. User-provided assets remain path references in Brand Kit JSON; import/export preserves those paths rather than copying files unexpectedly.

## UI location

Settings > **App Identity / Brand Kit** provides the preset selector, editable identity and links, namespace opt-in/import, validated hex palette fields, feature labels, asset pickers, social metadata/layout controls, previews, and Brand Kit JSON import/export.

Runtime identity is applied to the native title, top-bar name/logo, feature-tab labels, settings/backup dialogs, update source and release link, diagnostics heading, capture status text, screenshot/loop filename prefix and output folder, launch icon on the next start, and optional screenshot/loop watermark/share-card overlay.

## Migration and backwards compatibility

`brand` is `serde(default)` and omitted when it is exactly the BowEcho default. Therefore old `config.json` files, including `{}`, load with the original BowEcho names, links, palette, `bowecho` filename prefix, `Pictures/BowEcho` output folder, and legacy storage paths.

Selecting CWT does **not** move data or enable a new namespace. A custom namespace is a separate opt-in setting applied after restart. The import action performs a copy-only tree import: it skips symlinks and existing destination files and never deletes or moves the source. `config.json` and `styles.json` remain in the legacy BowEcho config root so the namespace can always be discovered and reverted. A branded package may set `BOWECHO_DEFAULT_BRAND` and, separately, `BOWECHO_STORAGE_NAMESPACE` for first-run defaults; an existing readable config remains authoritative.

## Files changed

- `crates/settings/src/brand.rs` — Brand Kit model, presets, validation, helpers, and regression tests.
- `crates/settings/src/lib.rs` — persisted `brand`, first-run distribution defaults, opt-in storage root/import, branded media folder helper, and compatibility tests.
- `crates/app_ui/src/brand.rs` — egui asset cache, title/release helpers, share overlay, previews, and UI-level tests.
- `crates/app_ui/src/main.rs` — startup/title/icon wiring, branded top bar/settings/update links/dialogs, Brand Kit editor, palette application, and namespace activation.
- `crates/app_ui/src/media.rs` — branded filenames/folders/status, capture-only watermark/share card, aspect-ratio letterboxing, and media tests.
- `crates/app_ui/build.rs` — environment-configurable Windows VERSIONINFO and executable icon with unchanged BowEcho defaults.
- `docs/app-identity.md` — this design and migration note.

## Build-time only

Runtime Brand Kits cannot rewrite an already-built executable. The Windows `.ico` resource, VERSIONINFO, executable/internal filename, code signing, installer/product naming, bundle identifiers, and installer artwork remain packaging-time concerns. `build.rs` accepts `BOWECHO_APP_ICON_ICO` and the `BOWECHO_BRAND_*`, `BOWECHO_EXE_NAME`, `BOWECHO_INTERNAL_NAME`, and `BOWECHO_LEGAL_COPYRIGHT` variables. Signing certificates and installer configuration remain outside this source patch.
