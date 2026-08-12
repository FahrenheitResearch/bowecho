# Live precipitation type

BowEcho's live precipitation-type layer combines two different observations of a storm:

- the displayed radar reflectivity locates precipitation;
- a time-matched HRRR or RAP thermodynamic column estimates whether that precipitation reaches the ground as rain, snow, freezing rain, or ice pellets.

The phase calculation uses the revised Modified Bourgouin energy method. It integrates melting and refreezing energy through the wet-bulb profile instead of treating a single temperature level as the answer.

## Using the layer

Open the **Layers** rail, then choose **+ Add layer > Live precipitation type**. The layer window controls the thermodynamic model, current-surface correction, presentation mode, opacity, and mixed-phase threshold. Press **Refresh analysis** after changing an input option. BowEcho matches the model valid hour to the displayed radar scan, including archive scans. Ordinary scans within that hour update only the precipitation footprint; they do not redownload or reclassify the model columns.

Historical RTMA is not available from BowEcho's operational NOMADS adapter. Archive scenes therefore retain the time-matched HRRR/RAP model surface and label that choice instead of mixing a historical radar scan with today's surface analysis.

The status and cursor inspector report all contributing times and sources: model cycle and valid time, surface-analysis source and valid time, radar source and scan time, input ages, model-grid spacing, and the algorithm version. The product is drawn at the radar footprint, but its phase prior remains at the stated model resolution.

## Display modes

- **Dominant** shows the highest-confidence phase.
- **Probability blend** blends the four independently regridded phase scores.
- **Ice hazard** emphasizes freezing rain and ice pellets.
- **Diagnostics** exposes confidence and quality-control details in the inspector.

BowEcho interpolates the four phase scores separately and derives the displayed category afterward. It never interpolates categorical phase codes. A **Mixed** pixel means no single phase reaches the configured confidence threshold; it is expected near transition zones.

## Limits

- The result depends on the quality and age of the model temperature/moisture profile and current-surface correction.
- The method cannot distinguish freezing drizzle from freezing rain.
- A permissive reflectivity occurrence threshold is used so light snow is not erased.
- Missing or stale radar/model inputs are labelled explicitly. BowEcho does not present an unavailable source as current.

The implementation follows Birk, Lenning, Donofrio, and Friedlein (2021), *A Revised Bourgouin Precipitation-Type Algorithm*, Weather and Forecasting 36, 425–438, doi:10.1175/WAF-D-20-0118.1.
