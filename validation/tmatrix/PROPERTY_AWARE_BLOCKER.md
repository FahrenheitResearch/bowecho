# Property-aware frozen-particle LUT blocker

The generated assets are conventional rain/dry-ice/wet-hail research tables.
A scheme-native or generic property-aware frozen-particle LUT was **not**
generated in this checkpoint.

In particular, no shipped table jointly covers the closure coordinates needed
for property-aware P3/ISHMAEL-style states: equivolume diameter, bulk density,
rime mass fraction/rime density, minor-to-major ratio, liquid mass fraction,
frequency, and a frozen-particle orientation treatment. The conventional wet
table has a liquid-mass-fraction axis, and the frozen tables have deterministic
Gaussian canting, but their solid-ice density is fixed. Relabeling those axes or
categories as scheme-native properties would be scientifically invalid.

A follow-up table needs a reviewed porous air/ice dielectric rule across bulk
density, a topology/mixing treatment for rime and liquid water, compatible
terminal-speed closures, dense resonance-region axes, and a new off-grid report
selected only after that grid is frozen. It must remain
`research_only_unvalidated` until genuinely independent scientific evidence is
available.
