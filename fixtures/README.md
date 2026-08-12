# Contract fixtures

The first table below is HAND-WRITTEN to the contract; the second is
REAL `gpuwm run-plan` output. Which is which is stated per file, because
"is this the engine's bytes or ours?" is the only question that matters
when a fixture and the engine disagree.

Hand-written fixtures matching the REAL `gpuwm run-plan` contract:
gpuwm/runplan.py + docs/run-plan.md @ the engine lane's wt-runplan
worktree, tip 795999cc8 (contract strings final). Studio develops
against these until live mode is switched on; swapping fixture → live is
configuration (`StudioSettings.contract_mode`), never code. The Rust
types in `crates/arwen-plan` parse every file here in their test suite,
so the fixtures and the types cannot drift apart silently.

| file                 | contract surface                                    |
| -------------------- | --------------------------------------------------- |
| plan.json            | `gpuwm.run-plan.v1` plan Studio submits              |
| probe.json           | `--probe --no-readiness` reply (`…probe.v1`)         |
| estimate.json        | `--estimate` reply (`…estimate.v1`)                  |
| resolve.json         | `--resolve` reply (`…resolved.v1`)                   |
| events.jsonl         | happy-path `gpuwm.run-plan.event.v1` stream          |
| events-failure.jsonl | failure-path event stream                            |
| run-progress.json    | `gpuwm.run-progress/v1` heartbeat (mid-run)          |
| run-manifest.json    | `gpuwm.run-manifest.v1` reattach entry point         |

REAL ENGINE CAPTURES (not hand-written): the moving-nest set below is
`gpuwm run-plan`'s own output, captured 2026-08-08 from the RELEASED
gpuwm 1.8.6 in Studio's own engine venv — the same binary the live cells
drive. (They were first captured a few hours earlier from the unshipped
`lane/run-plan-corridor` @ 4d49469d6, and re-captured on release: every
structured value was identical, one resolution NOTE gained the words
"the rw-wps prepare stage (gpuwm.source_cli)". Provenance is the shipped
artifact now, so nothing here depends on a worktree that may be gone.)

| file                            | what it is                                                        |
| ------------------------------- | ----------------------------------------------------------------- |
| generated-config-gfs-nested.toml| `gpuwm domain --ladder 12-3 --source gfs`'s emission (204×162 root @12 km + 408×320 ratio-4 child) — the fixture surface for a nested GFS plan |
| resolve-moving-nest.json        | `--resolve` of that config + a `[relocation]` itinerary: the `moving_nest` decision (`prepared:go`) + the `statics_corridor` resolution |
| resolve-moving-nest-hrrr.json   | the same, on the HRRR chain: `prepared:hrrr`, which 1.8.6 also feeds with a sealed corridor |
| estimate-corridor.json          | `--estimate` of the GFS follow plan: `corridor` priced at 410_323_968 host bytes (d02, 816×648) |
| estimate-corridor-hrrr.json     | the HRRR arm: 330_594_624 host bytes (d02, 732×582) — a DIFFERENT corridor, which is why the fixture dispatch keys on the chain |
| estimate-corridor-still.json    | the IDENTICAL GFS config WITHOUT `[relocation]` — the zero-corridor arm; its `vram` is byte-identical to the follow arm's, which is how "a corridor is not VRAM" is held true rather than asserted |
| refusal-moving-nest-hrrr.txt    | the CLI's stderr, verbatim, for a nested-HRRR follow plan (exit 2), captured at 1.8.5. HISTORICAL: 1.8.6 seals a corridor on that chain and no longer says this. Kept because "an engine refusal reaches the user verbatim" is a permanent property of the launch guard, and it must be tested on a sentence the ENGINE wrote |

`resolve-nested.json` and `estimate.json` are captures from the RELEASED
1.8.5 engine and carry no `moving_nest`/`corridor` at all. That absence
is load-bearing and these files are NOT to be refreshed: it is the
negative arm of the capability probe the prepared-route moving-nest gate
reads, and cell f12 walks both engines by feeding one capture and then
the other. The gate is open on 1.8.6; an older venv or a rollback lands
back on the shut side, and that side has to stay tested.

Fixture caveats, stated so nobody mistakes them for contract:

- `sequence` is dense from 1, exactly as the engine emits.
- The REAL stream emits `model_progress` on EVERY outer step (1440 for
  this 6 h / dt 15 s run); the fixture samples every 60th step so the
  file stays reviewable. Consumers must budget for full step cadence.
- `valid_time` on `output_committed` is `datetime.isoformat()` — naive,
  no `Z` — and Studio parses it as UTC.
- The engine currently REFUSES the intent-level inline config in
  plan.json (the experiment route wants a complete config incl.
  `[case_data]`); the fixture pretends the open intent-route engine
  request (docs/END-GAME.md §4) has landed so the whole Studio flow is
  exercisable. resolve.json's `configuration.case_data` is likewise a
  plausible sketch, and Studio only reads `configuration.experiment`.
