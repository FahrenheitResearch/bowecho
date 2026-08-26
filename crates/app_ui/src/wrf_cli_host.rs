//! Application-hosted `bowecho wrf render` implementation.
//!
//! The command deliberately passes through BowEcho's WRF processor and Rusty
//! Weather's production batch renderer.  This module is orchestration and
//! receipt generation; it contains no second meteorological decoder or plot
//! implementation.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use bowecho_cli::artifact::{
    ArtifactFileReceipt, ArtifactManifest, ArtifactStatus, BuildIdentity, DerivationMethod,
    FailureRecord, FiniteStatistics, GridReceipt, ProductReceipt, SourceReceipt,
    UnavailableProduct,
};
use bowecho_cli::{CliError, ExitCode, RenderOptions, RuntimeContext};
use chrono::{DateTime, Utc};
use rusty_weather::batch_render::{
    BatchHourScope, BatchProductKind, BatchRenderDomain, BatchRenderEvent, BatchRenderLimits,
    BatchRenderRequest, BatchRenderTime, inspect_renderable_products, run_batch_render,
};
use rusty_weather::render_all::StoreFieldSource;
use sha2::{Digest, Sha256};
use wrf_core::variables::{VARS, VarDim};

use crate::wrf_process::{
    WrfExactProcessSummary, WrfProcessOptions, process_scene_group_exact,
    requested_store_variable_name,
};
use app_ui::wrf_scene_adapter::inventory_wrf_paths;
use app_ui::wrf_scene_inventory::{WrfSceneInventory, parse_wrf_internal_time};

const RUSTY_WEATHER_COMMIT: &str = "68b74857780e436843cbf599c25ebccb886f7b8a";
const OUTPUT_WIDTH: u32 = 1_200;
const OUTPUT_HEIGHT: u32 = 900;

const SEVERE_PRODUCTS: &[&str] = &[
    "composite_reflectivity",
    "total_qpf",
    "10m_wind_gusts",
    "2m_temperature",
    "2m_dewpoint",
    "mslp_10m_winds",
    "sbcape",
    "sbcin",
    "mlcape",
    "mlcin",
    "mucape",
    "mucin",
    "uh_2to5km",
];

const PROCESS_ONLY: &[&str] = &[
    "t2",
    "dp2m",
    "U10",
    "V10",
    "wspd10",
    "slp",
    "PSFC",
    "maxdbz",
    "apcp",
    "UP_HELI_MAX",
    "WSPD10MAX",
    "SNOWNC",
    "GRAUPELNC",
    "sbcape",
    "sbcin",
    "mlcape",
    "mlcin",
    "mucape",
    "mucin",
    "wa",
    "max_vertical_velocity",
];

#[derive(Debug)]
pub(crate) struct RenderOutput {
    pub manifest: ArtifactManifest,
    pub manifest_path: PathBuf,
    /// Receipts inspected for this invocation, before cumulative watch merge.
    /// This lets the watcher journal the exact source without rehashing a
    /// multi-gigabyte wrfout or guessing which sorted receipt was newest.
    pub input_sources: Vec<SourceReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DesiredProduct {
    render_slug: String,
    receipt_name: String,
    expected_sources: Vec<String>,
}

#[derive(Debug)]
struct RenderedEvent {
    time: BatchRenderTime,
    slug: String,
    staged_path: PathBuf,
    source_fields: Vec<String>,
    units: Option<String>,
}

#[derive(Debug)]
struct PendingPublication {
    staged_path: PathBuf,
    destination: PathBuf,
}

/// Execute the one-shot render command. stdout stays pristine unless the
/// caller explicitly requested the machine-readable final manifest.
pub(crate) fn execute_render(
    options: &RenderOptions,
    context: &RuntimeContext,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<ExitCode, CliError> {
    let expanded = options.inputs.expand();
    let failure_paths = expanded.as_ref().ok().cloned().unwrap_or_default();
    let output = match expanded
        .and_then(|input_paths| render_inputs(options, context, input_paths, stderr))
    {
        Ok(output) => output,
        Err(error) => {
            if let Some(failed) =
                publish_failed_render_manifest(options, context, &failure_paths, &error)?
            {
                if options.json {
                    serde_json::to_writer(&mut *stdout, &failed.manifest).map_err(
                        |write_error| {
                            CliError::internal(format!(
                                "serialize failed artifact manifest: {write_error}"
                            ))
                        },
                    )?;
                    writeln!(stdout).map_err(|write_error| {
                        CliError::internal(format!("write failed artifact manifest: {write_error}"))
                    })?;
                }
                writeln!(
                    stderr,
                    "BowEcho WRF render: Failed, manifest {}",
                    failed.manifest_path.display()
                )
                .map_err(|write_error| {
                    CliError::internal(format!("write failed render progress: {write_error}"))
                })?;
            }
            return Err(error);
        }
    };
    if options.json {
        serde_json::to_writer(&mut *stdout, &output.manifest)
            .map_err(|error| CliError::internal(format!("serialize artifact manifest: {error}")))?;
        writeln!(stdout)
            .map_err(|error| CliError::internal(format!("write artifact manifest: {error}")))?;
    }
    writeln!(
        stderr,
        "BowEcho WRF render: {:?}, {} product(s), manifest {}",
        output.manifest.status,
        output.manifest.products.len(),
        output.manifest_path.display()
    )
    .map_err(|error| CliError::internal(format!("write render progress: {error}")))?;
    Ok(if output.manifest.failures.is_empty() {
        ExitCode::Success
    } else {
        ExitCode::Data
    })
}

/// Once `run.json` is valid, even a failed one-shot render leaves a compact,
/// atomic machine receipt. Invalid/missing run manifests cannot name a safe
/// case/run output root, so those argument failures remain stderr-only.
fn publish_failed_render_manifest(
    options: &RenderOptions,
    context: &RuntimeContext,
    input_paths: &[PathBuf],
    error: &CliError,
) -> Result<Option<RenderOutput>, CliError> {
    let loaded_run = match bowecho_cli::run_manifest::load_run_manifest(&options.run_manifest) {
        Ok(run) => run,
        Err(_) => return Ok(None),
    };
    let mut manifest = loaded_run.artifact_manifest_shell(BuildIdentity {
        version: context.bowecho_version.clone(),
        commit: context.bowecho_commit.clone(),
    });
    manifest.status = ArtifactStatus::Failed;
    manifest.provenance.additional.insert(
        "rusty_weather_commit".to_owned(),
        RUSTY_WEATHER_COMMIT.to_owned(),
    );
    manifest.provenance.additional.insert(
        "processing_identity".to_owned(),
        processing_identity(options, &loaded_run.sha256, context),
    );
    for path in input_paths {
        let Ok(hash) = bowecho_cli::fs::sha256_file(path) else {
            continue;
        };
        if hash.bytes == 0 {
            continue;
        }
        manifest.sources.push(SourceReceipt {
            path: path.clone(),
            bytes: hash.bytes,
            sha256: hash.sha256,
            domain: None,
            initialization_time: None,
            valid_times: Vec::new(),
            grid: GridReceipt::default(),
        });
    }
    manifest.failures.push(FailureRecord {
        stage: "render_invocation".to_owned(),
        path: input_paths.first().cloned(),
        reason: error.message.clone(),
    });
    let contract_failures = manifest.validate_contract();
    if !contract_failures.is_empty() {
        return Err(CliError::internal(format!(
            "failed artifact manifest did not satisfy its contract: {}",
            contract_failures.join("; ")
        )));
    }
    let manifest_path =
        bowecho_cli::paths::artifact_manifest_path(&options.output_directory, &loaded_run.manifest);
    bowecho_cli::fs::write_json_atomic(&manifest_path, &manifest).map_err(|write_error| {
        CliError::input(format!(
            "atomically publish failed artifact manifest {}: {write_error}",
            manifest_path.display()
        ))
    })?;
    let input_sources = manifest.sources.clone();
    Ok(Some(RenderOutput {
        manifest,
        manifest_path,
        input_sources,
    }))
}

pub(crate) fn render_inputs(
    options: &RenderOptions,
    context: &RuntimeContext,
    input_paths: Vec<PathBuf>,
    stderr: &mut dyn Write,
) -> Result<RenderOutput, CliError> {
    render_inputs_with_mode(options, context, input_paths, false, stderr)
}

/// Reusable host seam for `wrf watch`. Merge mode retains receipts produced
/// by earlier stable files while replacing an identical domain/time/product
/// if a producer deliberately reruns that scene.
pub(crate) fn render_inputs_with_mode(
    options: &RenderOptions,
    context: &RuntimeContext,
    input_paths: Vec<PathBuf>,
    merge_existing: bool,
    stderr: &mut dyn Write,
) -> Result<RenderOutput, CliError> {
    if input_paths.is_empty() {
        return Err(CliError::input("WRF render received no source files"));
    }
    let loaded_run = bowecho_cli::run_manifest::load_run_manifest(&options.run_manifest)?;
    let mut manifest = loaded_run.artifact_manifest_shell(BuildIdentity {
        version: context.bowecho_version.clone(),
        commit: context.bowecho_commit.clone(),
    });
    manifest.provenance.additional.insert(
        "rusty_weather_commit".to_owned(),
        RUSTY_WEATHER_COMMIT.to_owned(),
    );
    manifest.provenance.additional.insert(
        "processing_identity".to_owned(),
        processing_identity(options, &loaded_run.sha256, context),
    );

    writeln!(
        stderr,
        "Inspecting {} WRF source file(s)…",
        input_paths.len()
    )
    .map_err(|error| CliError::internal(format!("write render progress: {error}")))?;
    let (sources, source_hashes) = inspect_sources(&input_paths, context, &options.variables)?;
    manifest.sources = sources;

    let inventoried = inventory_wrf_paths(&input_paths)
        .map_err(|error| CliError::data(format!("inventory WRF scenes: {error}")))?;
    manifest.warnings.extend(
        inventoried
            .notes
            .into_iter()
            .map(|note| format!("{}: {}", note.source_name, note.message)),
    );
    if inventoried.inventory.groups.is_empty() {
        return Err(CliError::data("WRF inventory produced no model scenes"));
    }
    preflight_inventory(&inventoried.inventory, &manifest.sources)?;

    let work_hash = render_work_hash(options, &loaded_run.sha256, &source_hashes, context);
    let run_root =
        bowecho_cli::paths::artifact_run_root(&options.output_directory, &loaded_run.manifest);
    fs::create_dir_all(&run_root).map_err(|error| {
        CliError::input(format!(
            "create WRF artifact run directory {}: {error}",
            run_root.display()
        ))
    })?;
    let work_prefix = format!(".bowecho-work-{}-", &work_hash[..12]);
    let work_directory = tempfile::Builder::new()
        .prefix(&work_prefix)
        .tempdir_in(&run_root)
        .map_err(|error| {
            CliError::input(format!(
                "create scoped WRF work directory under {}: {error}",
                run_root.display()
            ))
        })?;
    let work_root = work_directory.path();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.workers)
        .thread_name(|index| format!("bowecho-wrf-{index}"))
        .build()
        .map_err(|error| CliError::internal(format!("create WRF worker pool: {error}")))?;
    let mut pending_publications = Vec::new();

    for (group_index, group) in inventoried.inventory.groups.iter().enumerate() {
        let domain = group.key.run_domain.domain.label();
        let group_token = format!(
            "{}-g{:016x}",
            domain, group.key.grid_signature.horizontal_coordinate_digest
        );
        let internal_run = bowecho_cli::paths::slug(&format!(
            "{}_{}_{}",
            loaded_run.manifest.run_id, domain, group_token
        ));
        let group_root = work_root.join(format!("{group_index:04}-{group_token}"));
        let store_root = group_root.join("store");
        let stage_root = group_root.join("render-stage");
        let process_options = WrfProcessOptions {
            core_fields: true,
            sounding_volumes: false,
            diagnostics: true,
            heavy_ecape: false,
            raw_extras: true,
            vertical_extrema: true,
            extra_variables: options.variables.clone(),
            only: PROCESS_ONLY
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            skip: Vec::new(),
        };

        let (processed, progress) = pool.install(|| {
            let mut progress = Vec::new();
            let result = process_scene_group_exact(
                group,
                &store_root,
                &internal_run,
                &process_options,
                &mut |message| progress.push(message),
            );
            (result, progress)
        });
        for message in progress {
            writeln!(stderr, "{domain}: {message}").map_err(|error| {
                CliError::internal(format!("write WRF processing progress: {error}"))
            })?;
        }
        let processed = match processed {
            Ok(summary) => summary,
            Err(reason) => {
                manifest.failures.push(FailureRecord {
                    stage: "wrf_process".to_owned(),
                    path: group.scenes.first().map(|scene| scene.path.clone()),
                    reason: format!("{domain}: {reason}"),
                });
                continue;
            }
        };
        manifest.warnings.extend(
            processed
                .process
                .notes
                .iter()
                .map(|note| format!("{domain}: {note}")),
        );

        render_group(
            &pool,
            options,
            &domain,
            &store_root,
            &internal_run,
            &stage_root,
            &run_root,
            &processed,
            &mut manifest,
            &mut pending_publications,
            stderr,
        )?;
    }

    reconcile_expected_outputs(options, &inventoried.inventory, &mut manifest);
    if manifest.products.is_empty() {
        manifest.failures.push(FailureRecord {
            stage: "production_render".to_owned(),
            path: None,
            reason: "render completed without producing any image artifacts".to_owned(),
        });
    }
    normalize_manifest(&mut manifest);
    manifest.status = manifest_status(&manifest);

    let manifest_path =
        bowecho_cli::paths::artifact_manifest_path(&options.output_directory, &loaded_run.manifest);
    let input_sources = manifest.sources.clone();
    if merge_existing && manifest_path.is_file() {
        manifest = merge_existing_manifest(&manifest_path, manifest)?;
    }
    validate_manifest_before_publication(&manifest)?;
    verify_sources_unchanged(&input_sources)?;
    publish_staged_artifacts(&pending_publications)?;
    bowecho_cli::fs::write_json_atomic(&manifest_path, &manifest).map_err(|error| {
        CliError::input(format!(
            "atomically publish artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    Ok(RenderOutput {
        manifest,
        manifest_path,
        input_sources,
    })
}

fn inspect_sources(
    paths: &[PathBuf],
    context: &RuntimeContext,
    requested_variables: &[String],
) -> Result<(Vec<SourceReceipt>, Vec<String>), CliError> {
    let mut receipts = Vec::with_capacity(paths.len());
    let mut hashes = Vec::with_capacity(paths.len());
    for path in paths {
        let (report, _code) = bowecho_cli::wrf::inspect(path, context)?;
        let inspected = report.files.into_iter().next().ok_or_else(|| {
            CliError::data(format!(
                "inspection returned no file for {}",
                path.display()
            ))
        })?;
        if !inspected.complete {
            let mut reasons = inspected.failures;
            reasons.extend(inspected.missing_metadata);
            reasons.extend(inspected.suspicious_metadata);
            return Err(CliError::data(format!(
                "WRF source {} is not complete/readable: {}",
                path.display(),
                if reasons.is_empty() {
                    "metadata completeness proof failed".to_owned()
                } else {
                    reasons.join("; ")
                }
            )));
        }
        validate_requested_variable_dimensions(
            &inspected.path,
            &inspected.variables,
            requested_variables,
        )?;
        let bytes = inspected
            .bytes
            .ok_or_else(|| CliError::data(format!("inspection did not hash {}", path.display())))?;
        let sha256 = inspected
            .sha256
            .ok_or_else(|| CliError::data(format!("inspection did not hash {}", path.display())))?;
        hashes.push(sha256.clone());
        receipts.push(SourceReceipt {
            path: inspected.path,
            bytes,
            sha256,
            domain: inspected.domain,
            initialization_time: inspected.initialization_time,
            valid_times: inspected.valid_times,
            grid: GridReceipt {
                nx: inspected.grid.nx,
                ny: inspected.grid.ny,
                nz: inspected.grid.nz,
                dx_m: inspected.grid.dx_m,
                dy_m: inspected.grid.dy_m,
                projection: inspected.projection.name,
                projection_parameters: inspected.projection.parameters,
            },
        });
    }
    Ok((receipts, hashes))
}

fn validate_requested_variable_dimensions(
    path: &Path,
    variables: &[bowecho_cli::wrf::VariableMetadata],
    requested: &[String],
) -> Result<(), CliError> {
    let mut failures = Vec::new();
    for requested_name in requested {
        if let Some(definition) = VARS
            .iter()
            .find(|definition| definition.name.eq_ignore_ascii_case(requested_name.trim()))
            && definition.dim != VarDim::TwoD
        {
            failures.push(format!(
                "requested diagnostic '{}' is {:?}; the v1 generic renderer accepts only explicit 2-D planes, so select or derive a named level before rendering",
                requested_name, definition.dim
            ));
            continue;
        }
        let Some(variable) = variables
            .iter()
            .find(|variable| variable.name.eq_ignore_ascii_case(requested_name.trim()))
        else {
            continue;
        };
        let spatial_dimensions = variable
            .dimensions
            .iter()
            .filter(|dimension| {
                !matches!(
                    dimension.name.to_ascii_lowercase().as_str(),
                    "time" | "times" | "date_strlen"
                )
            })
            .collect::<Vec<_>>();
        let vertically_resolved = spatial_dimensions.iter().any(|dimension| {
            matches!(
                dimension.name.to_ascii_lowercase().as_str(),
                "bottom_top" | "bottom_top_stag" | "level" | "lev" | "pressure"
            )
        }) || spatial_dimensions.len() > 2;
        if vertically_resolved {
            failures.push(format!(
                "requested variable '{}' is 3-D in {} (dimensions {}); the v1 generic renderer accepts only explicit 2-D planes, so select or derive a named level before rendering",
                requested_name,
                path.display(),
                variable
                    .dimensions
                    .iter()
                    .map(|dimension| format!("{}={}", dimension.name, dimension.length))
                    .collect::<Vec<_>>()
                    .join(" x ")
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(CliError::unavailable(failures.join("; ")))
    }
}

fn preflight_inventory(
    inventory: &WrfSceneInventory,
    sources: &[SourceReceipt],
) -> Result<(), CliError> {
    let run_initializations = inventory
        .groups
        .iter()
        .map(|group| canonical_initialization(&group.key.run_domain.run.0))
        .collect::<Result<Vec<_>, _>>()?;
    let source_initializations = sources
        .iter()
        .map(|source| {
            let value = source.initialization_time.as_deref().ok_or_else(|| {
                CliError::data(format!(
                    "WRF source {} has no initialization identity",
                    source.path.display()
                ))
            })?;
            canonical_initialization(value)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let scene_identities = inventory
        .groups
        .iter()
        .flat_map(|group| {
            let domain = group.key.run_domain.domain.label();
            group.scenes.iter().map(move |scene| {
                let valid = scene
                    .time
                    .valid_time()
                    .map(|time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                    .unwrap_or_else(|| "<unavailable>".to_owned());
                let locator = format!(
                    "{} time {} grid {:016x}",
                    scene.path.display(),
                    scene.time_index,
                    group.key.grid_signature.horizontal_coordinate_digest
                );
                ((domain.clone(), valid), locator)
            })
        })
        .collect::<Vec<_>>();
    validate_preflight_identity_values(
        run_initializations,
        source_initializations,
        scene_identities,
    )
}

fn validate_preflight_identity_values(
    run_initializations: impl IntoIterator<Item = String>,
    source_initializations: impl IntoIterator<Item = String>,
    scene_identities: impl IntoIterator<Item = ((String, String), String)>,
) -> Result<(), CliError> {
    let run_initializations = run_initializations.into_iter().collect::<BTreeSet<_>>();
    if run_initializations.len() != 1 {
        return Err(CliError::data(format!(
            "WRF inputs contain mixed run identities: {}",
            run_initializations
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let source_initializations = source_initializations.into_iter().collect::<BTreeSet<_>>();
    if source_initializations.len() != 1 {
        return Err(CliError::data(format!(
            "WRF inputs contain mixed initialization times: {}",
            source_initializations
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if run_initializations != source_initializations {
        return Err(CliError::data(format!(
            "WRF scene run identity {} disagrees with source initialization {}",
            run_initializations.iter().next().expect("one run identity"),
            source_initializations
                .iter()
                .next()
                .expect("one source initialization")
        )));
    }

    let mut seen = BTreeMap::<(String, String), String>::new();
    for (identity, locator) in scene_identities {
        if identity.1 == "<unavailable>" {
            return Err(CliError::data(format!(
                "WRF scene {locator} has no authoritative output time"
            )));
        }
        if let Some(previous) = seen.insert(identity.clone(), locator.clone()) {
            return Err(CliError::data(format!(
                "duplicate output scene identity {} {} from {previous} and {locator}; BowEcho will not overwrite one incompatible scene with another",
                identity.0, identity.1
            )));
        }
    }
    Ok(())
}

fn canonical_initialization(value: &str) -> Result<String, CliError> {
    let parsed = parse_wrf_internal_time(value).or_else(|| {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|time| time.with_timezone(&Utc))
    });
    parsed
        .map(|time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .ok_or_else(|| CliError::data(format!("invalid WRF initialization identity '{value}'")))
}

fn verify_sources_unchanged(sources: &[SourceReceipt]) -> Result<(), CliError> {
    let mut failures = Vec::new();
    for source in sources {
        let before = match fs::metadata(&source.path) {
            Ok(metadata) => metadata,
            Err(error) => {
                failures.push(format!(
                    "{} cannot be restatted before final hash: {error}",
                    source.path.display()
                ));
                continue;
            }
        };
        let hash = match bowecho_cli::fs::sha256_file(&source.path) {
            Ok(hash) => hash,
            Err(error) => {
                failures.push(format!(
                    "{} cannot be re-hashed: {error}",
                    source.path.display()
                ));
                continue;
            }
        };
        let after = match fs::metadata(&source.path) {
            Ok(metadata) => metadata,
            Err(error) => {
                failures.push(format!(
                    "{} cannot be restatted after final hash: {error}",
                    source.path.display()
                ));
                continue;
            }
        };
        if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
            failures.push(format!(
                "{} changed while it was being re-hashed",
                source.path.display()
            ));
        } else if hash.bytes != source.bytes || hash.sha256 != source.sha256 {
            failures.push(format!(
                "{} changed after initial inspection (expected {} bytes SHA-256 {}, got {} bytes SHA-256 {})",
                source.path.display(),
                source.bytes,
                source.sha256,
                hash.bytes,
                hash.sha256
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(CliError::data(format!(
            "WRF source stability proof failed before artifact publication: {}",
            failures.join("; ")
        )))
    }
}

fn publish_staged_artifacts(publications: &[PendingPublication]) -> Result<(), CliError> {
    let mut destinations = BTreeSet::new();
    for publication in publications {
        let key = normalized_receipt_path(&publication.destination);
        if !destinations.insert(key) {
            return Err(CliError::data(format!(
                "multiple staged products target {}; refusing a nondeterministic overwrite",
                publication.destination.display()
            )));
        }
        if !publication.staged_path.is_file() {
            return Err(CliError::data(format!(
                "staged renderer output disappeared before publication: {}",
                publication.staged_path.display()
            )));
        }
    }
    for publication in publications {
        bowecho_cli::fs::publish_file_atomic(&publication.staged_path, &publication.destination)
            .map_err(|error| {
                CliError::input(format!(
                    "atomically publish {} to {}: {error}",
                    publication.staged_path.display(),
                    publication.destination.display()
                ))
            })?;
    }
    Ok(())
}

fn validate_manifest_before_publication(manifest: &ArtifactManifest) -> Result<(), CliError> {
    let failures = manifest.validate_contract();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(CliError::data(format!(
            "artifact manifest contract validation failed before publication: {}",
            failures.join("; ")
        )))
    }
}

fn derivation_method(slug: &str, kind: BatchProductKind) -> DerivationMethod {
    if matches!(
        slug,
        "composite_reflectivity"
            | "total_qpf"
            | "2m_dewpoint"
            | "mslp_10m_winds"
            | "sbcape"
            | "sbcin"
            | "mlcape"
            | "mlcin"
            | "mucape"
            | "mucin"
            | "var:precip_interval"
            | "var:max_vertical_velocity"
    ) || matches!(kind, BatchProductKind::Derived | BatchProductKind::Heavy)
    {
        DerivationMethod::Derived
    } else {
        DerivationMethod::Direct
    }
}

fn manifest_status(manifest: &ArtifactManifest) -> ArtifactStatus {
    if manifest.products.is_empty() {
        ArtifactStatus::Failed
    } else if manifest.failures.is_empty() {
        ArtifactStatus::Complete
    } else {
        ArtifactStatus::Partial
    }
}

#[allow(clippy::too_many_arguments)]
fn render_group(
    pool: &rayon::ThreadPool,
    options: &RenderOptions,
    domain: &str,
    store_root: &Path,
    internal_run: &str,
    stage_root: &Path,
    run_root: &Path,
    processed: &WrfExactProcessSummary,
    manifest: &mut ArtifactManifest,
    pending_publications: &mut Vec<PendingPublication>,
    stderr: &mut dyn Write,
) -> Result<(), CliError> {
    let desired = desired_products(options);
    let desired_by_slug = desired
        .iter()
        .cloned()
        .map(|product| (product.render_slug.clone(), product))
        .collect::<BTreeMap<_, _>>();
    let scene_by_slot = processed
        .scenes
        .iter()
        .map(|scene| (scene.storage_slot, scene))
        .collect::<HashMap<_, _>>();
    let mut kind_by_slug = BTreeMap::<String, BatchProductKind>::new();
    let mut render_slugs = BTreeSet::<String>::new();
    let mut unavailable_render_coordinates = BTreeSet::<(u16, String)>::new();

    for scene in &processed.scenes {
        let catalog = inspect_renderable_products(
            store_root,
            &processed.process.model,
            internal_run,
            scene.storage_slot,
        )
        .map_err(|error| {
            CliError::data(format!(
                "inspect renderable products for {domain} {}: {error}",
                scene.valid_time
            ))
        })?;
        let options_by_slug = catalog
            .products
            .into_iter()
            .map(|option| (option.slug.clone(), option))
            .collect::<BTreeMap<_, _>>();
        for product in &desired {
            if let Some(available) = options_by_slug.get(&product.render_slug) {
                render_slugs.insert(product.render_slug.clone());
                kind_by_slug
                    .entry(product.render_slug.clone())
                    .or_insert(available.kind);
            } else {
                unavailable_render_coordinates
                    .insert((scene.storage_slot, product.render_slug.clone()));
                manifest.unavailable_products.push(UnavailableProduct {
                    name: product.receipt_name.clone(),
                    domain: Some(domain.to_owned()),
                    valid_time: Some(scene.valid_time.clone()),
                    reason:
                        "required fields are absent from this WRF scene's production render catalog"
                            .to_owned(),
                    source_variables: product.expected_sources.clone(),
                });
            }
        }
        for optional in ["wrf_snownc", "wrf_graupelnc"] {
            let render_slug = format!("var:{optional}");
            if let Some(available) = options_by_slug.get(&render_slug) {
                render_slugs.insert(render_slug.clone());
                kind_by_slug.entry(render_slug).or_insert(available.kind);
            } else {
                unavailable_render_coordinates.insert((scene.storage_slot, render_slug));
            }
        }
        for (name, sources, reason) in [
            (
                "surface_reflectivity",
                vec!["dbz".to_owned()],
                "not wired: a verified lowest-level or 1-km interpolation is not available",
            ),
            (
                "scheme_hail",
                vec!["scheme-native hail state".to_owned()],
                "not wired: no verified scheme-native hail diagnostic is present",
            ),
        ] {
            manifest.unavailable_products.push(UnavailableProduct {
                name: name.to_owned(),
                domain: Some(domain.to_owned()),
                valid_time: Some(scene.valid_time.clone()),
                reason: reason.to_owned(),
                source_variables: sources,
            });
        }
    }

    if render_slugs.is_empty() {
        return Ok(());
    }
    let render_slugs = render_slugs.into_iter().collect::<Vec<_>>();
    let work_items = processed
        .scenes
        .len()
        .checked_mul(render_slugs.len())
        .ok_or_else(|| CliError::internal("WRF render work-item count overflow"))?;
    let request = BatchRenderRequest {
        store_root: store_root.to_path_buf(),
        model_slug: processed.process.model.clone(),
        run_slug: internal_run.to_owned(),
        hours: BatchHourScope::AllStored,
        expected_exact_time: None,
        product_spec: render_slugs.join(","),
        out_dir: stage_root.to_path_buf(),
        domain: BatchRenderDomain::NativeGrid,
        date_yyyymmdd: None,
        cycle_utc: None,
        source: None,
        output_width: OUTPUT_WIDTH,
        output_height: OUTPUT_HEIGHT,
        limits: BatchRenderLimits {
            max_hours: processed.scenes.len().max(1),
            max_products_per_hour: render_slugs.len().max(1),
            max_work_items: work_items.max(1),
            max_output_width: OUTPUT_WIDTH,
            max_output_height: OUTPUT_HEIGHT,
            max_output_pixels: u64::from(OUTPUT_WIDTH) * u64::from(OUTPUT_HEIGHT),
        },
    };
    let (summary, events) = pool.install(|| {
        let mut events = Vec::new();
        let summary =
            run_batch_render(request, &AtomicBool::new(false), |event| events.push(event));
        (summary, events)
    });
    if let Err(error) = summary {
        manifest.failures.push(FailureRecord {
            stage: "production_render".to_owned(),
            path: None,
            reason: format!("{domain}: {error}"),
        });
        return Ok(());
    }

    for event in events {
        match event {
            BatchRenderEvent::ItemRendered {
                time: Some(time),
                slug,
                output_path,
                source_fields,
                units,
                ..
            } => {
                let rendered = RenderedEvent {
                    time,
                    slug,
                    staged_path: output_path,
                    source_fields,
                    units,
                };
                if let Err(error) = stage_rendered_receipt(
                    domain,
                    store_root,
                    internal_run,
                    run_root,
                    &scene_by_slot,
                    &desired_by_slug,
                    &kind_by_slug,
                    rendered,
                    manifest,
                    pending_publications,
                ) {
                    manifest.failures.push(FailureRecord {
                        stage: "artifact_stage".to_owned(),
                        path: None,
                        reason: error.message,
                    });
                }
            }
            BatchRenderEvent::ItemSkipped {
                time: Some(time),
                slug,
                reason,
                ..
            } => {
                if let Some(scene) = scene_by_slot.get(&time.storage_slot) {
                    let desired = desired_by_slug.get(&slug);
                    manifest.unavailable_products.push(UnavailableProduct {
                        name: desired
                            .map(|value| value.receipt_name.clone())
                            .unwrap_or_else(|| receipt_name(&slug)),
                        domain: Some(domain.to_owned()),
                        valid_time: Some(scene.valid_time.clone()),
                        reason,
                        source_variables: desired
                            .map(|value| value.expected_sources.clone())
                            .unwrap_or_default(),
                    });
                }
            }
            BatchRenderEvent::ItemFailed {
                time, slug, error, ..
            } => {
                if render_failure_is_expected_absence(time, &slug, &unavailable_render_coordinates)
                {
                    continue;
                }
                let valid = time
                    .and_then(|time| scene_by_slot.get(&time.storage_slot).copied())
                    .map(|scene| scene.valid_time.as_str())
                    .unwrap_or("unknown time");
                manifest.failures.push(FailureRecord {
                    stage: "production_render".to_owned(),
                    path: None,
                    reason: format!("{domain} {valid} {slug}: {error}"),
                });
            }
            BatchRenderEvent::HourStarted { time, index, total } => {
                let valid = scene_by_slot
                    .get(&time.storage_slot)
                    .map(|scene| scene.valid_time.as_str())
                    .unwrap_or("unknown time");
                writeln!(stderr, "{domain}: render {index}/{total} {valid}").map_err(|error| {
                    CliError::internal(format!("write render progress: {error}"))
                })?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn render_failure_is_expected_absence(
    time: Option<BatchRenderTime>,
    slug: &str,
    unavailable: &BTreeSet<(u16, String)>,
) -> bool {
    time.is_some_and(|time| unavailable.contains(&(time.storage_slot, slug.to_owned())))
}

#[allow(clippy::too_many_arguments)]
fn stage_rendered_receipt(
    domain: &str,
    store_root: &Path,
    internal_run: &str,
    run_root: &Path,
    scene_by_slot: &HashMap<u16, &crate::wrf_process::WrfProcessedScene>,
    desired_by_slug: &BTreeMap<String, DesiredProduct>,
    kind_by_slug: &BTreeMap<String, BatchProductKind>,
    rendered: RenderedEvent,
    manifest: &mut ArtifactManifest,
    pending_publications: &mut Vec<PendingPublication>,
) -> Result<(), CliError> {
    let scene = scene_by_slot
        .get(&rendered.time.storage_slot)
        .ok_or_else(|| {
            CliError::internal(format!(
                "renderer returned unknown storage slot {}",
                rendered.time.storage_slot
            ))
        })?;
    let exact = rendered.time.exact_time.ok_or_else(|| {
        CliError::internal(format!(
            "renderer omitted exact time for storage slot {}",
            rendered.time.storage_slot
        ))
    })?;
    if exact.lead_seconds != scene.lead_seconds || exact.valid_unix != scene.valid_unix {
        return Err(CliError::data(format!(
            "renderer time identity changed for {domain} slot {}",
            rendered.time.storage_slot
        )));
    }
    let desired = desired_by_slug.get(&rendered.slug);
    let name = desired
        .map(|value| value.receipt_name.clone())
        .unwrap_or_else(|| receipt_name(&rendered.slug));
    let relative =
        bowecho_cli::paths::artifact_relative_path(domain, &scene.valid_time, &name, "png")
            .map_err(CliError::data)?;
    let destination = run_root.join(&relative);
    let dimensions = image::image_dimensions(&rendered.staged_path).map_err(|error| {
        CliError::data(format!(
            "read rendered image dimensions {}: {error}",
            rendered.staged_path.display()
        ))
    })?;
    let hash = bowecho_cli::fs::sha256_file(&rendered.staged_path).map_err(|error| {
        CliError::input(format!(
            "hash staged rendered artifact {}: {error}",
            rendered.staged_path.display()
        ))
    })?;

    let store = StoreFieldSource::open(store_root, "wrf", internal_run, rendered.time.storage_slot)
        .map_err(|error| CliError::data(format!("reopen rendered store slot: {error}")))?;
    let primary = rendered.source_fields.first();
    let statistics = primary
        .and_then(|field| store.stats_2d(field).ok())
        .map(|stats| FiniteStatistics {
            available: true,
            minimum: stats.finite_min.map(f64::from),
            maximum: stats.finite_max.map(f64::from),
            finite_value_count: stats.finite_count,
            missing_value_count: stats.missing_count,
        })
        .unwrap_or_default();
    let units = rendered
        .units
        .or_else(|| {
            primary.and_then(|field| store.surface_variable(field).map(|var| var.units.clone()))
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let kind = kind_by_slug
        .get(&rendered.slug)
        .copied()
        .unwrap_or(BatchProductKind::Generic);
    let derivation = derivation_method(&rendered.slug, kind);
    manifest.products.push(ProductReceipt {
        name,
        domain: Some(domain.to_owned()),
        initialization_time: initialization_time(scene.valid_unix, scene.lead_seconds),
        valid_time: Some(scene.valid_time.clone()),
        storage_slot: Some(scene.storage_slot),
        lead_seconds: Some(scene.lead_seconds),
        units,
        source_variables: rendered.source_fields,
        derivation,
        artifact: ArtifactFileReceipt {
            relative_path: relative,
            mime_type: "image/png".to_owned(),
            width: Some(dimensions.0),
            height: Some(dimensions.1),
            bytes: hash.bytes,
            sha256: hash.sha256,
        },
        statistics,
    });
    pending_publications.push(PendingPublication {
        staged_path: rendered.staged_path,
        destination,
    });
    Ok(())
}

fn desired_products(options: &RenderOptions) -> Vec<DesiredProduct> {
    let mut products = SEVERE_PRODUCTS
        .iter()
        .map(|slug| DesiredProduct {
            render_slug: (*slug).to_owned(),
            receipt_name: (*slug).to_owned(),
            expected_sources: expected_sources(slug),
        })
        .collect::<Vec<_>>();
    for variable in ["precip_interval", "max_vertical_velocity"] {
        products.push(DesiredProduct {
            render_slug: format!("var:{variable}"),
            receipt_name: variable.to_owned(),
            expected_sources: vec![variable.to_owned()],
        });
    }
    for requested in &options.variables {
        let store_name = requested_store_variable_name(requested);
        products.push(DesiredProduct {
            render_slug: format!("var:{store_name}"),
            receipt_name: store_name,
            expected_sources: vec![requested.clone()],
        });
    }
    products.sort_by(|left, right| left.render_slug.cmp(&right.render_slug));
    products.dedup_by(|left, right| left.render_slug == right.render_slug);
    products
}

fn expected_receipt_products(options: &RenderOptions) -> Vec<DesiredProduct> {
    let mut products = desired_products(options);
    products.extend([
        DesiredProduct {
            render_slug: "surface_reflectivity".to_owned(),
            receipt_name: "surface_reflectivity".to_owned(),
            expected_sources: vec!["dbz".to_owned()],
        },
        DesiredProduct {
            render_slug: "scheme_hail".to_owned(),
            receipt_name: "scheme_hail".to_owned(),
            expected_sources: vec!["scheme-native hail state".to_owned()],
        },
    ]);
    products.sort_by(|left, right| left.receipt_name.cmp(&right.receipt_name));
    products.dedup_by(|left, right| left.receipt_name == right.receipt_name);
    products
}

fn reconcile_expected_outputs(
    options: &RenderOptions,
    inventory: &WrfSceneInventory,
    manifest: &mut ArtifactManifest,
) {
    let expected = expected_receipt_products(options);
    for group in &inventory.groups {
        let domain = group.key.run_domain.domain.label();
        for scene in &group.scenes {
            let Some(valid_time) = scene.time.valid_time() else {
                continue;
            };
            let valid_time = valid_time.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            reconcile_expected_coordinate(&domain, &valid_time, &expected, manifest);
        }
    }
}

fn reconcile_expected_coordinate(
    domain: &str,
    valid_time: &str,
    expected: &[DesiredProduct],
    manifest: &mut ArtifactManifest,
) {
    let produced = manifest
        .products
        .iter()
        .map(product_key)
        .collect::<BTreeSet<_>>();
    let mut unavailable = manifest
        .unavailable_products
        .iter()
        .map(unavailable_key)
        .collect::<BTreeSet<_>>();
    for product in expected {
        let key = (
            domain.to_owned(),
            valid_time.to_owned(),
            product.receipt_name.clone(),
        );
        if produced.contains(&key) || unavailable.contains(&key) {
            continue;
        }
        manifest.unavailable_products.push(UnavailableProduct {
            name: product.receipt_name.clone(),
            domain: Some(domain.to_owned()),
            valid_time: Some(valid_time.to_owned()),
            reason: if manifest.failures.is_empty() {
                "production renderer returned neither an artifact nor a product blocker"
                    .to_owned()
            } else {
                "processing or rendering failed before this expected product completed; see failure records"
                    .to_owned()
            },
            source_variables: product.expected_sources.clone(),
        });
        unavailable.insert(key);
    }
}

fn expected_sources(slug: &str) -> Vec<String> {
    match slug {
        "composite_reflectivity" => vec!["maxdbz".to_owned()],
        "total_qpf" => vec!["apcp".to_owned()],
        "10m_wind_gusts" => vec!["WSPD10MAX".to_owned()],
        "2m_temperature" => vec!["T2".to_owned()],
        "2m_dewpoint" => vec!["dp2m".to_owned()],
        "mslp_10m_winds" => vec!["slp".to_owned(), "U10".to_owned(), "V10".to_owned()],
        "uh_2to5km" => vec!["UP_HELI_MAX".to_owned()],
        value => vec![value.to_owned()],
    }
}

fn receipt_name(render_slug: &str) -> String {
    render_slug
        .strip_prefix("var:")
        .unwrap_or(render_slug)
        .to_owned()
}

fn initialization_time(valid_unix: i64, lead_seconds: u64) -> Option<String> {
    let lead = i64::try_from(lead_seconds).ok()?;
    DateTime::<Utc>::from_timestamp(valid_unix.checked_sub(lead)?, 0)
        .map(|time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn render_work_hash(
    options: &RenderOptions,
    run_manifest_hash: &str,
    source_hashes: &[String],
    context: &RuntimeContext,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bowecho.wrf.render.work.v1\0");
    hasher.update(run_manifest_hash.as_bytes());
    hasher.update(context.bowecho_version.as_bytes());
    hasher.update(context.bowecho_commit.as_bytes());
    hasher.update(RUSTY_WEATHER_COMMIT.as_bytes());
    hasher.update(options.preset.as_str().as_bytes());
    hasher.update(options.workers.to_le_bytes());
    for hash in source_hashes {
        hasher.update(hash.as_bytes());
    }
    for variable in &options.variables {
        hasher.update(variable.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

/// Stable science/configuration identity for cumulative watch manifests.
/// Worker count and source-file hashes are deliberately excluded: they do not
/// change requested science, and each source has its own receipt.
pub(crate) fn processing_identity(
    options: &RenderOptions,
    run_manifest_hash: &str,
    context: &RuntimeContext,
) -> String {
    let mut variables = options
        .variables
        .iter()
        .map(|variable| variable.trim().to_owned())
        .collect::<Vec<_>>();
    variables.sort();
    variables.dedup();

    let mut hasher = Sha256::new();
    hasher.update(b"bowecho.wrf.processing.v1\0");
    hasher.update(bowecho_cli::run_manifest::RUN_SCHEMA_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(bowecho_cli::artifact::ARTIFACT_SCHEMA_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(run_manifest_hash.as_bytes());
    hasher.update([0]);
    hasher.update(context.bowecho_version.as_bytes());
    hasher.update([0]);
    hasher.update(context.bowecho_commit.as_bytes());
    hasher.update([0]);
    hasher.update(RUSTY_WEATHER_COMMIT.as_bytes());
    hasher.update([0]);
    hasher.update(options.preset.as_str().as_bytes());
    for variable in variables {
        hasher.update([0]);
        hasher.update(variable.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn normalize_manifest(manifest: &mut ArtifactManifest) {
    manifest.sources.sort_by(|left, right| {
        left.path
            .to_string_lossy()
            .cmp(&right.path.to_string_lossy())
            .then_with(|| left.sha256.cmp(&right.sha256))
    });
    manifest
        .sources
        .dedup_by(|left, right| left.path == right.path && left.sha256 == right.sha256);
    manifest.products.sort_by(product_order);
    manifest
        .products
        .dedup_by(|left, right| product_key(left) == product_key(right));
    manifest.unavailable_products.sort_by(|left, right| {
        unavailable_key(left)
            .cmp(&unavailable_key(right))
            .then_with(|| left.reason.cmp(&right.reason))
    });
    manifest
        .unavailable_products
        .dedup_by(|left, right| unavailable_key(left) == unavailable_key(right));
    manifest.warnings.sort();
    manifest.warnings.dedup();
    manifest.failures.sort_by(|left, right| {
        left.stage
            .cmp(&right.stage)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.reason.cmp(&right.reason))
    });
    manifest.failures.dedup_by(|left, right| {
        left.stage == right.stage && left.path == right.path && left.reason == right.reason
    });
}

fn product_order(left: &ProductReceipt, right: &ProductReceipt) -> std::cmp::Ordering {
    product_key(left).cmp(&product_key(right))
}

fn product_key(product: &ProductReceipt) -> (String, String, String) {
    (
        product.domain.clone().unwrap_or_default(),
        product.valid_time.clone().unwrap_or_default(),
        product.name.clone(),
    )
}

fn unavailable_key(product: &UnavailableProduct) -> (String, String, String) {
    (
        product.domain.clone().unwrap_or_default(),
        product.valid_time.clone().unwrap_or_default(),
        product.name.clone(),
    )
}

fn merge_existing_manifest(
    path: &Path,
    mut current: ArtifactManifest,
) -> Result<ArtifactManifest, CliError> {
    let bytes = fs::read(path).map_err(|error| {
        CliError::input(format!(
            "read existing watch manifest {}: {error}",
            path.display()
        ))
    })?;
    let existing: ArtifactManifest = serde_json::from_slice(&bytes).map_err(|error| {
        CliError::data(format!(
            "parse existing watch manifest {}: {error}",
            path.display()
        ))
    })?;
    let identity_matches = existing.schema_version == current.schema_version
        && existing.case_id == current.case_id
        && existing.run_id == current.run_id
        && existing.member_id == current.member_id
        && existing.experiment_track == current.experiment_track
        && existing.model == current.model;
    if !identity_matches {
        return Err(CliError::data(format!(
            "existing artifact manifest {} belongs to a different run",
            path.display()
        )));
    }
    let existing_processing = existing.provenance.additional.get("processing_identity");
    let current_processing = current.provenance.additional.get("processing_identity");
    if existing_processing != current_processing {
        normalize_manifest(&mut current);
        current.status = manifest_status(&current);
        return Ok(current);
    }

    let touched_times = current
        .sources
        .iter()
        .filter_map(|source| {
            source.domain.as_ref().map(|domain| {
                source
                    .valid_times
                    .iter()
                    .cloned()
                    .map(|valid| (domain.clone(), valid))
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .chain(current.products.iter().map(|product| {
            (
                product.domain.clone().unwrap_or_default(),
                product.valid_time.clone().unwrap_or_default(),
            )
        }))
        .chain(current.unavailable_products.iter().map(|product| {
            (
                product.domain.clone().unwrap_or_default(),
                product.valid_time.clone().unwrap_or_default(),
            )
        }))
        .collect::<BTreeSet<_>>();
    let current_paths = current
        .sources
        .iter()
        .map(|source| normalized_receipt_path(&source.path))
        .collect::<BTreeSet<_>>();
    let successful = current
        .products
        .iter()
        .map(product_key)
        .collect::<BTreeSet<_>>();
    current.sources.extend(
        existing
            .sources
            .into_iter()
            .filter(|source| !current_paths.contains(&normalized_receipt_path(&source.path))),
    );
    current
        .products
        .extend(existing.products.into_iter().filter(|product| {
            let time = (
                product.domain.clone().unwrap_or_default(),
                product.valid_time.clone().unwrap_or_default(),
            );
            !touched_times.contains(&time) && !successful.contains(&product_key(product))
        }));
    current
        .unavailable_products
        .extend(existing.unavailable_products.into_iter().filter(|product| {
            let time = (
                product.domain.clone().unwrap_or_default(),
                product.valid_time.clone().unwrap_or_default(),
            );
            !touched_times.contains(&time) && !successful.contains(&unavailable_key(product))
        }));
    current.warnings.extend(existing.warnings);
    normalize_manifest(&mut current);
    current.status = manifest_status(&current);
    Ok(current)
}

fn normalized_receipt_path(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowecho_cli::RenderPreset;
    use bowecho_cli::artifact::{ExperimentTrack, ProvenanceHashes};
    use bowecho_cli::input::InputSet;
    use bowecho_cli::wrf::{DimensionMetadata, VariableMetadata};

    fn render_options(variables: Vec<String>) -> RenderOptions {
        RenderOptions {
            inputs: InputSet { specs: Vec::new() },
            preset: RenderPreset::Severe,
            output_directory: PathBuf::from("out"),
            run_manifest: PathBuf::from("run.json"),
            workers: 2,
            variables,
            json: false,
        }
    }

    fn manifest() -> ArtifactManifest {
        ArtifactManifest {
            schema_version: bowecho_cli::artifact::ARTIFACT_SCHEMA_VERSION.to_owned(),
            status: ArtifactStatus::Complete,
            case_id: "case".to_owned(),
            run_id: "run".to_owned(),
            member_id: None,
            experiment_track: ExperimentTrack::WrfParity,
            model: "wrf".to_owned(),
            microphysics_scheme: None,
            provenance: ProvenanceHashes::default(),
            bowecho: BuildIdentity {
                version: "test".to_owned(),
                commit: "test".to_owned(),
            },
            sources: Vec::new(),
            products: Vec::new(),
            warnings: Vec::new(),
            unavailable_products: Vec::new(),
            failures: Vec::new(),
        }
    }

    fn source(path: &str, hash_byte: char) -> SourceReceipt {
        SourceReceipt {
            path: PathBuf::from(path),
            bytes: 12,
            sha256: std::iter::repeat_n(hash_byte, 64).collect(),
            domain: Some("d01".to_owned()),
            initialization_time: Some("2026-07-22T00:00:00Z".to_owned()),
            valid_times: vec!["2026-07-22T00:15:00Z".to_owned()],
            grid: GridReceipt::default(),
        }
    }

    #[test]
    fn severe_mapping_includes_production_interval_extrema_and_requested_variable() {
        let desired = desired_products(&render_options(vec!["CUSTOM FIELD".to_owned()]));
        let slugs = desired
            .iter()
            .map(|product| product.render_slug.as_str())
            .collect::<BTreeSet<_>>();
        assert!(slugs.contains("composite_reflectivity"));
        assert!(slugs.contains("uh_2to5km"));
        assert!(slugs.contains("var:precip_interval"));
        assert!(slugs.contains("var:max_vertical_velocity"));
        assert!(slugs.contains("var:wrf_custom_field"));
    }

    #[test]
    fn failed_one_shot_render_publishes_a_machine_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("wrfout_d01_broken");
        fs::write(&input, b"broken").unwrap();
        let run = temp.path().join("run.json");
        fs::write(
            &run,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": bowecho_cli::run_manifest::RUN_SCHEMA_VERSION,
                "case_id": "case",
                "run_id": "run",
                "experiment_track": "wrf-parity",
                "model": "WRF"
            }))
            .unwrap(),
        )
        .unwrap();
        let mut options = render_options(Vec::new());
        options.run_manifest = run;
        options.output_directory = temp.path().join("out");
        let output = publish_failed_render_manifest(
            &options,
            &RuntimeContext::new("test", "commit"),
            std::slice::from_ref(&input),
            &CliError::data("truncated NetCDF"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(output.manifest.status, ArtifactStatus::Failed);
        assert!(output.manifest_path.is_file());
        assert_eq!(output.manifest.sources.len(), 1);
        assert_eq!(output.manifest.failures[0].stage, "render_invocation");
        assert!(output.manifest.validate_contract().is_empty());
    }

    #[test]
    fn cumulative_merge_replaces_rewritten_source_and_stale_success_for_touched_time() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("artifact-manifest.json");
        let mut existing = manifest();
        existing.sources.push(source("run/wrfout_d01", 'a'));
        existing.products.push(ProductReceipt {
            name: "composite_reflectivity".to_owned(),
            domain: Some("d01".to_owned()),
            initialization_time: Some("2026-07-22T00:00:00Z".to_owned()),
            valid_time: Some("2026-07-22T00:15:00Z".to_owned()),
            storage_slot: Some(0),
            lead_seconds: Some(900),
            units: "dBZ".to_owned(),
            source_variables: vec!["composite_reflectivity".to_owned()],
            derivation: DerivationMethod::Direct,
            artifact: ArtifactFileReceipt {
                relative_path: PathBuf::from("d01/20260722T001500Z/old.png"),
                mime_type: "image/png".to_owned(),
                width: Some(1200),
                height: Some(900),
                bytes: 1,
                sha256: "c".repeat(64),
            },
            statistics: FiniteStatistics::default(),
        });
        bowecho_cli::fs::write_json_atomic(&path, &existing).unwrap();

        let mut current = manifest();
        current.sources.push(source("run\\wrfout_d01", 'b'));
        current.unavailable_products.push(UnavailableProduct {
            name: "composite_reflectivity".to_owned(),
            domain: Some("d01".to_owned()),
            valid_time: Some("2026-07-22T00:15:00Z".to_owned()),
            reason: "field absent after rewrite".to_owned(),
            source_variables: vec!["maxdbz".to_owned()],
        });

        let merged = merge_existing_manifest(&path, current).unwrap();
        assert_eq!(merged.sources.len(), 1);
        assert_eq!(merged.sources[0].sha256, "b".repeat(64));
        assert!(merged.products.is_empty());
        assert_eq!(merged.unavailable_products.len(), 1);
        assert_eq!(
            merged.unavailable_products[0].reason,
            "field absent after rewrite"
        );
    }

    #[test]
    fn preflight_rejects_mixed_runs_and_duplicate_output_coordinates() {
        let init = "2026-07-22T00:00:00Z".to_owned();
        assert!(
            validate_preflight_identity_values(
                vec![init.clone(), "2026-07-22T01:00:00Z".to_owned()],
                vec![init.clone()],
                Vec::<((String, String), String)>::new(),
            )
            .is_err()
        );
        assert!(
            validate_preflight_identity_values(
                vec![init.clone()],
                vec![init.clone(), "2026-07-22T01:00:00Z".to_owned()],
                Vec::<((String, String), String)>::new(),
            )
            .is_err()
        );
        assert!(
            validate_preflight_identity_values(
                vec![init.clone()],
                vec![init.clone()],
                vec![
                    (
                        ("d01".to_owned(), "2026-07-22T00:15:00Z".to_owned()),
                        "grid-a".to_owned(),
                    ),
                    (
                        ("d01".to_owned(), "2026-07-22T00:15:00Z".to_owned()),
                        "grid-b".to_owned(),
                    ),
                ],
            )
            .is_err()
        );

        // Normal nested domains may share a time, while a moving/remeshed
        // domain may contribute a disjoint time without colliding.
        assert!(
            validate_preflight_identity_values(
                vec![init.clone()],
                vec![init],
                vec![
                    (
                        ("d01".to_owned(), "2026-07-22T00:15:00Z".to_owned()),
                        "outer".to_owned(),
                    ),
                    (
                        ("d02".to_owned(), "2026-07-22T00:15:00Z".to_owned()),
                        "nest".to_owned(),
                    ),
                    (
                        ("d01".to_owned(), "2026-07-22T00:30:00Z".to_owned()),
                        "moved-grid".to_owned(),
                    ),
                ],
            )
            .is_ok()
        );
    }

    #[test]
    fn computed_products_are_derived_and_empty_manifests_fail() {
        assert_eq!(
            derivation_method("var:precip_interval", BatchProductKind::Generic),
            DerivationMethod::Derived
        );
        assert_eq!(
            derivation_method("var:max_vertical_velocity", BatchProductKind::Generic),
            DerivationMethod::Derived
        );
        assert_eq!(
            derivation_method("var:native_plane", BatchProductKind::Generic),
            DerivationMethod::Direct
        );
        for slug in [
            "composite_reflectivity",
            "total_qpf",
            "2m_dewpoint",
            "mslp_10m_winds",
            "sbcape",
            "mucin",
        ] {
            assert_eq!(
                derivation_method(slug, BatchProductKind::Direct),
                DerivationMethod::Derived,
                "{slug} must preserve that BowEcho computed the plotted field"
            );
        }
        assert_eq!(
            derivation_method("2m_temperature", BatchProductKind::Direct),
            DerivationMethod::Direct
        );
        let empty = manifest();
        assert_eq!(manifest_status(&empty), ArtifactStatus::Failed);
    }

    #[test]
    fn cross_time_render_failure_is_ignored_only_for_catalogued_absence() {
        let unavailable = BTreeSet::from([(0, "var:precip_interval".to_owned())]);
        let first = BatchRenderTime {
            storage_slot: 0,
            exact_time: None,
        };
        let second = BatchRenderTime {
            storage_slot: 1,
            exact_time: None,
        };
        assert!(render_failure_is_expected_absence(
            Some(first),
            "var:precip_interval",
            &unavailable
        ));
        assert!(!render_failure_is_expected_absence(
            Some(second),
            "var:precip_interval",
            &unavailable
        ));
        assert!(!render_failure_is_expected_absence(
            None,
            "var:precip_interval",
            &unavailable
        ));
    }

    #[test]
    fn final_source_hash_proof_detects_a_same_size_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("wrfout_d01");
        fs::write(&path, b"initial-bytes").unwrap();
        let initial = bowecho_cli::fs::sha256_file(&path).unwrap();
        let receipt = SourceReceipt {
            path: path.clone(),
            bytes: initial.bytes,
            sha256: initial.sha256,
            domain: Some("d01".to_owned()),
            initialization_time: Some("2026-07-22T00:00:00Z".to_owned()),
            valid_times: vec!["2026-07-22T00:15:00Z".to_owned()],
            grid: GridReceipt::default(),
        };
        assert!(verify_sources_unchanged(std::slice::from_ref(&receipt)).is_ok());
        fs::write(&path, b"changed-bytes").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), receipt.bytes);
        assert!(verify_sources_unchanged(&[receipt]).is_err());
    }

    #[test]
    fn processing_identity_is_order_and_worker_independent_but_tracks_variables() {
        let context = RuntimeContext::new("0.34.5", "abcdef0");
        let mut left = render_options(vec!["W".to_owned(), "T2".to_owned()]);
        left.workers = 1;
        let mut right = render_options(vec!["T2".to_owned(), "W".to_owned()]);
        right.workers = 64;
        assert_eq!(
            processing_identity(&left, &"a".repeat(64), &context),
            processing_identity(&right, &"a".repeat(64), &context)
        );
        right.variables.push("QVAPOR".to_owned());
        assert_ne!(
            processing_identity(&left, &"a".repeat(64), &context),
            processing_identity(&right, &"a".repeat(64), &context)
        );
    }

    #[test]
    fn cumulative_merge_starts_fresh_when_processing_identity_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("artifact-manifest.json");
        let mut existing = manifest();
        existing
            .provenance
            .additional
            .insert("processing_identity".to_owned(), "old".to_owned());
        existing.sources.push(source("old/wrfout_d01", 'a'));
        existing.products.push(ProductReceipt {
            name: "composite_reflectivity".to_owned(),
            domain: Some("d01".to_owned()),
            initialization_time: Some("2026-07-22T00:00:00Z".to_owned()),
            valid_time: Some("2026-07-22T00:15:00Z".to_owned()),
            storage_slot: Some(0),
            lead_seconds: Some(900),
            units: "dBZ".to_owned(),
            source_variables: vec!["composite_reflectivity".to_owned()],
            derivation: DerivationMethod::Direct,
            artifact: ArtifactFileReceipt {
                relative_path: PathBuf::from("d01/20260722T001500Z/old.png"),
                mime_type: "image/png".to_owned(),
                width: Some(1200),
                height: Some(900),
                bytes: 1,
                sha256: "c".repeat(64),
            },
            statistics: FiniteStatistics::default(),
        });
        bowecho_cli::fs::write_json_atomic(&path, &existing).unwrap();

        let mut current = manifest();
        current
            .provenance
            .additional
            .insert("processing_identity".to_owned(), "new".to_owned());
        current.sources.push(source("new/wrfout_d01", 'b'));
        let merged = merge_existing_manifest(&path, current).unwrap();
        assert_eq!(merged.sources.len(), 1);
        assert_eq!(merged.sources[0].path, PathBuf::from("new/wrfout_d01"));
        assert!(merged.products.is_empty());
        assert_eq!(manifest_status(&merged), ArtifactStatus::Failed);
    }

    #[test]
    fn reconciliation_accounts_for_every_expected_coordinate_once() {
        let expected = vec![
            DesiredProduct {
                render_slug: "a".to_owned(),
                receipt_name: "a".to_owned(),
                expected_sources: vec!["A".to_owned()],
            },
            DesiredProduct {
                render_slug: "b".to_owned(),
                receipt_name: "b".to_owned(),
                expected_sources: vec!["B".to_owned()],
            },
        ];
        let mut value = manifest();
        value.products.push(ProductReceipt {
            name: "a".to_owned(),
            domain: Some("d01".to_owned()),
            initialization_time: Some("2026-07-22T00:00:00Z".to_owned()),
            valid_time: Some("2026-07-22T00:15:00Z".to_owned()),
            storage_slot: Some(0),
            lead_seconds: Some(900),
            units: "1".to_owned(),
            source_variables: vec!["A".to_owned()],
            derivation: DerivationMethod::Direct,
            artifact: ArtifactFileReceipt {
                relative_path: PathBuf::from("d01/20260722T001500Z/a.png"),
                mime_type: "image/png".to_owned(),
                width: Some(1200),
                height: Some(900),
                bytes: 1,
                sha256: "c".repeat(64),
            },
            statistics: FiniteStatistics::default(),
        });
        reconcile_expected_coordinate("d01", "2026-07-22T00:15:00Z", &expected, &mut value);
        reconcile_expected_coordinate("d01", "2026-07-22T00:15:00Z", &expected, &mut value);
        assert_eq!(value.products.len(), 1);
        assert_eq!(value.unavailable_products.len(), 1);
        assert_eq!(value.unavailable_products[0].name, "b");
    }

    #[test]
    fn requested_native_three_dimensional_variable_is_rejected_clearly() {
        let variable = VariableMetadata {
            name: "QVAPOR".to_owned(),
            data_type: "f32".to_owned(),
            dimensions: vec![
                DimensionMetadata {
                    name: "Time".to_owned(),
                    length: 1,
                    unlimited: true,
                },
                DimensionMetadata {
                    name: "bottom_top".to_owned(),
                    length: 50,
                    unlimited: false,
                },
                DimensionMetadata {
                    name: "south_north".to_owned(),
                    length: 100,
                    unlimited: false,
                },
                DimensionMetadata {
                    name: "west_east".to_owned(),
                    length: 100,
                    unlimited: false,
                },
            ],
            units: Some("kg kg-1".to_owned()),
            description: None,
            staggering: None,
            netcdf_indexed: true,
        };
        let error = validate_requested_variable_dimensions(
            Path::new("wrfout_d01"),
            &[variable],
            &["QVAPOR".to_owned()],
        )
        .unwrap_err();
        assert_eq!(error.code, ExitCode::Unavailable);
        assert!(error.message.contains("3-D"));
        assert!(error.message.contains("explicit 2-D planes"));
    }

    #[test]
    fn manifest_contract_is_checked_before_publication() {
        let mut invalid = manifest();
        invalid.sources.push(source("run/wrfout_d01", 'a'));
        invalid.products.push(ProductReceipt {
            name: "bad".to_owned(),
            domain: Some("d01".to_owned()),
            initialization_time: Some("2026-07-22T00:00:00Z".to_owned()),
            valid_time: Some("2026-07-22T00:15:00Z".to_owned()),
            storage_slot: Some(0),
            lead_seconds: Some(900),
            units: "1".to_owned(),
            source_variables: Vec::new(),
            derivation: DerivationMethod::Direct,
            artifact: ArtifactFileReceipt {
                relative_path: PathBuf::from("d01/20260722T001500Z/bad.png"),
                mime_type: "image/png".to_owned(),
                width: Some(1200),
                height: Some(900),
                bytes: 1,
                sha256: "not-a-hash".to_owned(),
            },
            statistics: FiniteStatistics::default(),
        });
        assert!(validate_manifest_before_publication(&invalid).is_err());
    }
}
