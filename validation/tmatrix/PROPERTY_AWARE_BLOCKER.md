# Property-aware LUT status and remaining blockers

The earlier coordinate-coverage blocker is resolved by a five-table research
bundle for bounded P3/ISHMAEL-style characteristic-particle states: separate
dry/wet oblate/prolate frozen tables plus standalone/residual rain. The strict
runtime binds closure-derived diameter, temperature, density or condensed
volume fraction, liquid mass fraction, shape, exact frequency, orientation,
view geometry, coexistence allocation, and no-extrapolation policies.

This does not resolve the scientific production blocker. The bundle evaluates
one characteristic monodisperse particle at exactly 1 m^-3 rather than either
scheme's native PSD. Rime mass and rime density influence the closed density
and shape but are not independent dielectric axes. Symmetric Bruggeman
air/ice/water morphology, Gaussian20 orientation, terminal policies, radar
basis/covariance phase, propagation conventions, and wet-particle behavior
still lack reviewed independent comparisons or laboratory evidence.

The native PyTMatrix solver remains resolution-sensitive at shape ratio 0.1.
An all-grid `ndgs=8` to `10` candidate and the first refined-grid `ndgs=12` to
`14` candidate failed and are retained. The provenance-valid final exact
`ndgs=14` configs pass an exhaustive `ndgs=12` to `14` comparison at all
2,180,240 Cartesian points, with role-specific bounds of 89 mm dry oblate,
exactly 32.31174267785264 mm dry prolate, 15 mm wet oblate, and 6.3 mm wet
prolate. The dry-prolate cap is the last all-grid-converged node before an
unresolved near-zero KDP resonance; order/ddelt and failed node-removal probes
are retained. Requests outside their own role bound are rejected instead of
clamped or silently reassigned.

The frozen-particle Schiller-Naumann law also has an explicit Re=1000 boundary
policy for the small jump between its low- and high-Re drag branches. This
resolves terminal-moment computation only; runtime property evaluation still
replaces stored normalized fall moments with closure-derived moments.

Until independent Mie/PSD, covariance/basis, propagation-unit, morphology, and
operational comparisons pass review, every asset and runtime path must remain
opt-in `research_only_unvalidated`.
