# ArWen Studio vendor record

BowEcho vendors the ArWen Studio Rust control-plane snapshot from commit
`fdee27fe5bd1eae6601e1d3342bde1544c15db36` (2026-08-08).

Included crates:

- `arwen-plan`: versioned `gpuwm run-plan` contract types
- `arwen-proc`: sealed child-process/query supervision and run registry
- `arwen-map`: Lambert geometry, map view, search, and generated basemap data
- `arwen-studio`: forecast planner and run monitor

The vendored ArWen-authored code is Apache-2.0. BowEcho remains MIT OR
Apache-2.0; those source files retain their Apache-2.0 SPDX headers and license
terms. The generated `arwen-map/src/basemap_data.rs` and
`arwen-map/src/basemap_towns.rs` tables do not carry that blanket SPDX claim:
they retain provenance comments for their U.S. Census Bureau and Natural Earth
inputs, which are attributed in `THIRD-PARTY-NOTICES.md` and the BowEcho Guide.

BowEcho hosts the Studio surface inside its WRF workspace, but never loads the
Python/CUDA engine into the BowEcho process. The external `gpuwm run-plan`
contract remains the scientific and capability authority. Capability gates
must use probe/resolve responses rather than version comparisons.

The initial embedded integration permits planning, resolve/estimate, and
monitoring existing runs. Launch is deliberately gated until every GPU route
has a supervised/cancellable boundary and the managed engine has a verified,
transactional installer/repair path.

Fixture captures were sanitized to remove machine-local paths before being
added to BowEcho. They otherwise retain the captured contract content.
