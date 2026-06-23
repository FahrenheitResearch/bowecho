# BowEcho v0.25.7 GPT Pro Base

This branch is the clean base for applying the next large GPT Pro patch.

## Source

- Worktree: `C:\Users\drew\radar-work\wt-codex-gpt-pro-v0257-base`
- Branch: `codex/gpt-pro-v0257-base`
- Upstream release tag: `v0.25.7` (`c7fa4c2`)
- Applied snapshot: `C:\Users\drew\radar-work\_handoffs\bowecho-v0257-current-good-build.diff`

## Accepted Current Build

- Exe: `C:\Users\drew\Downloads\bowecho-v0257-3d-partial-volume-fix.exe`
- SHA-256: `652C00C119D1AFFF5D5C7D38CA33734C61CFC079CC3E02012ED0EC6BE3F8D523`

This build is considered good enough to preserve as the working baseline.

## Baseline Behavior To Preserve

- Advanced low-sweep controls from the v0.25.7 sweep-control work.
- Multi-radar coordinated loop fixes already present in this accepted source.
- Current-alert sorting/filtering and alert badge/focus behavior.
- 3D Volume Explorer revamp, including map-box selection, product-aware rendering, and fixed floor projection.
- 3D partial/live-volume guard: do not resample 3D from a live partial volume; use complete same-site data or wait.
- Volume/Floor 3D controls are inline panels so combo boxes do not insta-close.

## Requested Non-Goal For Now

- Do not spend patch scope on Volume Explorer basemap selection yet. It would be nice later, but this handoff should avoid touching it unless the incoming patch directly requires it.

## Patch Workflow

Use this branch as the base for GPT Pro output:

```powershell
git status --short
git apply --check C:\path\to\gpt-pro.patch
git apply C:\path\to\gpt-pro.patch
cargo fmt --all --check
cargo check -p app_ui --bin bowecho
cargo test -p app_ui vol3d
cargo test -p app_ui low_sweep
cargo clippy -p app_ui --all-targets -- -D warnings
git diff --check
```

For a quick compile while iterating, prefer:

```powershell
cargo check -p app_ui --bin bowecho
```

Only do an optimized release build once the patch is stable:

```powershell
cargo build -p app_ui --bin bowecho --release
```
