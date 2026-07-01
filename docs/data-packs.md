# Data Packs

Data packs are ready-made radar scenes: press Load, BowEcho fetches the
required scans, builds the master timeline, selects low-level tilts, applies
the view, and starts the loop without manual archive hunting.

## Goals

- Load tornado and severe-weather moments with the correct NEXRAD frames.
- Preserve SAILS/MESO-SAILS low-level revisits as timeline frames instead of
  hiding them inside a volume scan.
- Keep panes, overlays, model layers, and optional second radars synced to one
  master cursor.
- Make site and point-motion following feel smooth by preparing the next frame
  before committing the visible state.
- Support offline-ready packs by validating every required object in the
  existing shared Level II cache.

## MVP Workflow

1. A user opens Data > Data packs and chooses a pack.
2. BowEcho validates the manifest schema and app-version requirement.
3. For explicit-object packs, BowEcho checks each object by size/hash and
   downloads missing files with the existing Level II cache.
4. For time-window packs, BowEcho lists every UTC date touched by the event
   window, parses Level II object times, and selects the padded/capped frame
   range with `data_source::level2_objects_for_window`.
5. The selected objects decode into history frames (oldest first), which
   are the master frame list; low-tilt mode filters playback to the
   allowed low cuts inside the existing loop player.
6. BowEcho pauses live polling, applies the layout/view/overlays, selects the
   requested frame, and starts playback when requested.

## Built-In V1 Packs

The first in-app pack set should stay dual-pol first:

| Pack | Radar | UTC window | Anchor | Focus | Default view |
| --- | --- | --- | --- | --- | --- |
| Moore EF5 | KTLX | 2013-05-20 19:50-20:50 | 20:21 | 35.339, -97.486 | REF, dealiased velocity, CC, ZDR |
| El Reno Wedge | KTLX | 2013-05-31 22:55-23:55 | 23:33 | 35.500, -97.980 | REF, dealiased velocity, CC, ZDR |
| Rochelle-Fairdale EF4 | KLOT | 2015-04-09 23:15-2015-04-10 01:00 | 00:13 | 42.100, -88.940 | REF, dealiased velocity, CC, ZDR |
| Mayfield EF4 | KPAH | 2021-12-11 02:45-04:05 | 03:27 | 36.740, -88.640 | REF, dealiased velocity, CC, ZDR |
| Rolling Fork EF4 | KDGX | 2023-03-25 00:50-02:15 | 01:07 | 32.906, -90.878 | REF, dealiased velocity, CC, ZDR |

Tuscaloosa 2011 remains an important velocity case, but it should not be in
the first dual-pol set because KBMX had not been upgraded yet.

## Manifest Shape

```json
{
  "schema_version": 1,
  "id": "moore-2013-ktlx-dual-pol",
  "label": "Moore 2013 KTLX Dual-Pol Review",
  "window": {
    "start_utc": "2013-05-20T19:50:00Z",
    "end_utc": "2013-05-20T20:50:00Z",
    "anchor_utc": "2013-05-20T20:21:00Z",
    "pad_scans": 2,
    "max_frames": 24
  },
  "radars": [
    { "site_id": "KTLX", "role": "Primary" }
  ],
  "timeline_mode": "LowTilt",
  "low_tilt": {
    "max_elevation_hundredths_deg": 100,
    "prefer_complete": true
  },
  "view": {
    "focus_lat": 35.339,
    "focus_lon": -97.486,
    "range_km": 90.0,
    "follow_target": null
  }
}
```

V1 should support explicit object lists first for curated case studies. The
time-window resolver above is the second path and should be used for event
clicks and user-created packs.

## Scan Selection Rules

- Use real UTC timestamps parsed from Level II object names.
- List every UTC date touched by the event window, not only the start date.
- Start at the scan active at `window.start_utc`, then subtract context scans.
- End at the scan active at `window.end_utc`, then add context scans.
- When capped, trim outer context from the beginning so the event tail stays
  loaded.
- Select the frame nearest `anchor_utc`.
- Decode before final low-tilt selection because cut completeness, moments, and
  SAILS revisits are only reliable after decode.

## Low-Tilt Timeline

The current live low-level constants in `app_ui` are good defaults:

- max elevation: 1.0 degree
- complete low-level cut: at least 720 radials
- complete azimuth coverage: about 350 degrees

In data-pack low-tilt mode, every displayable low-level cut can become a frame.
That is the important tornado workflow: users can scrub from one low-level
revisit to the next with velocity, CC, SRV, dealiased velocity, and rotation
markers following the same cursor.

## Smooth Site And Follow Transitions

The current app switches immediately per pane. For packs, BowEcho should move
toward a transaction:

1. Resolve each visible pane to a frame, product, and stable tilt policy.
2. Queue the render requests.
3. Keep old textures anchored to their old radar site while replacements render.
4. Commit the new visible scene when required panes are ready, or after a short
   timeout with per-pane placeholders.
5. For cross-site follow, interpolate the map center toward the next target
   while the new site frame is prepared.

This avoids the jank where a site switch briefly clears or reuses an image in
the wrong spatial context.

## Dev Verification Mode

Pack QA needs a dev-only capture mode that renders the exact scene a user would
see after pressing Load. The capture target should be the pack scene itself:
pack id, selected frame, products, cut policy, map center, scale, overlays, and
pane layout.

The verifier should save:

- full-window screenshot
- map-canvas screenshot
- one screenshot per visible pane
- selected frame timestamps and product/cut labels
- per-frame map center, radar site, and selected low-tilt/cut index

That gives vision agents a stable checklist: is the tornado near the intended
point, did CC/ZDR land in the correct panes, do panes stay synced, and does the
loop avoid frame-to-frame jumps during follow or site switches.

## Code Map

- `crates/timeline`: pure manifest, archive-window planning, timeline sync, and
  low-tilt frame selection.
- `crates/data_source`: public NEXRAD listing and exact object selection for
  event windows.
- `crates/app_ui/src/event_explorer.rs`: existing track/report selection logic
  that should call the shared window selector instead of per-day labels.
- `crates/app_ui/src/main.rs`: existing decode/history/render/pane orchestration.
  The pack loader should reuse `decode_archive_history_object` and
  `install_decoded_load_batch` first, then later extract a dedicated
  `timeline_controller.rs`.
- `crates/app_ui/src/guide.rs`: user-facing pack docs once the UI is wired.

## Release And Trust

- Ship a few small JSON sample packs with screenshots.
- Show Download, Resume, Load, Ready offline, and Repair states.
- Verify object size for every public S3 object. Use SHA-256 when a pack ships
  bundled/non-S3 assets.
- Do not duplicate radar files into each pack by default; record ownership in a
  pack state file and reuse the shared Level II cache.
