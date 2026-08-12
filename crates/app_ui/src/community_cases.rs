//! Deliberate owner-facing case publication and verified artifact viewing.
//!
//! This module is intentionally separate from passive model/radar browsing.
//! Nothing is uploaded merely because the panel is opened, a run is viewed,
//! or a local WRF/ArWen file exists. A case is assembled from a closed typed
//! payload union, confirmed, and only then published through authenticated
//! HTTPS. There is no arbitrary-file, raw-directory, `wrfout`, full-run, peer,
//! ICE, STUN, or direct-connectivity path here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use eframe::egui;
use rw_community_protocol::{
    AnnotationArtifact, AttributionNotice, CASE_ARTIFACT_PUBLICATION_SCHEMA, CASE_SCHEMA,
    CaseArtifactPayload, CaseArtifactRef, CaseArtifactType, CaseModelSource, CaseRoomManifest,
    DataOrigin, DerivedTableArtifact, FixedCoordinate, OverlayArtifact, OverlayFeature,
    OverlayGeometry, PUBLICATION_OWNER_PARAMETER, ProtocolLimits, PublicationGrant,
    PublishCaseArtifactRequest, REQUEST_SCHEMA, RecipeIdentity, RenderedImageArtifact,
    RenderedImageFormat, ShareQuery, ShareRequest, SourceProvenance, TableCell,
    canonical_case_manifest_bytes, request_sha256,
};

use crate::community_cache::{
    CommunityCacheClient, CommunityCacheError, CommunityCaseBrowser, DeliveryTier,
    VerifiedCaseArtifact,
};

const MAX_CLIENT_ANNOTATION_BYTES: usize = 64 * 1024;
const MAX_CLIENT_TABLE_COLUMNS: usize = 64;
const MAX_CLIENT_TABLE_ROWS: usize = 10_000;
const MAX_CLIENT_TABLE_CELLS: usize = 100_000;
const MAX_CLIENT_OVERLAY_COORDINATES: usize = 10_000;
const MAX_CLIENT_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CLIENT_IMAGE_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_BROWSER_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_RENDERED_TABLE_ROWS: usize = 250;
const MAX_RENDERED_TABLE_COLUMNS: usize = 32;
const MAX_RENDERED_OVERLAY_COORDINATES: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactEditorKind {
    Annotation,
    DerivedTable,
    Overlay,
    RenderedImage,
}

impl ArtifactEditorKind {
    fn label(self) -> &'static str {
        match self {
            Self::Annotation => "Annotation",
            Self::DerivedTable => "Derived table",
            Self::Overlay => "Fixed-coordinate overlay",
            Self::RenderedImage => "PNG / WebP image",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayEditorKind {
    Points,
    Polyline,
    Polygon,
}

impl OverlayEditorKind {
    fn label(self) -> &'static str {
        match self {
            Self::Points => "Points",
            Self::Polyline => "Polyline",
            Self::Polygon => "Polygon",
        }
    }
}

#[derive(Debug, Clone)]
struct SelectedImage {
    display_name: String,
    format: RenderedImageFormat,
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
struct ArtifactDraft {
    artifact_id: String,
    kind: ArtifactEditorKind,
    title: String,
    annotation_text: String,
    annotation_event: String,
    table_tsv: String,
    overlay_kind: OverlayEditorKind,
    overlay_coordinates: String,
    image_alt_text: String,
    image: Option<SelectedImage>,
}

impl Default for ArtifactDraft {
    fn default() -> Self {
        Self {
            artifact_id: "analysis-note".into(),
            kind: ArtifactEditorKind::Annotation,
            title: "Analysis note".into(),
            annotation_text: String::new(),
            annotation_event: String::new(),
            table_tsv: "field\tvalue\n".into(),
            overlay_kind: OverlayEditorKind::Points,
            overlay_coordinates: String::new(),
            image_alt_text: String::new(),
            image: None,
        }
    }
}

#[derive(Debug, Clone)]
struct AttributionDraft {
    notice: String,
    source_url: String,
    license: String,
    license_url: String,
    terms_url: String,
    disclaimer: String,
}

impl AttributionDraft {
    fn noaa() -> Self {
        Self {
            notice: "Contains NOAA public weather-model data.".into(),
            source_url: "https://www.noaa.gov/".into(),
            license: "United States Government public data".into(),
            license_url: "https://www.weather.gov/disclaimer".into(),
            terms_url: "https://www.weather.gov/disclaimer".into(),
            disclaimer: "NOAA does not endorse this derived BowEcho publication.".into(),
        }
    }

    fn ecmwf() -> Self {
        let notice = AttributionNotice::ecmwf_open_data();
        Self {
            notice: notice.notice,
            source_url: notice.source_url,
            license: notice.license,
            license_url: notice.license_url,
            terms_url: notice.terms_url,
            disclaimer: notice.disclaimer,
        }
    }

    fn build(&self, provider: &str) -> AttributionNotice {
        AttributionNotice {
            provider: provider.to_owned(),
            notice: self.notice.clone(),
            source_url: self.source_url.clone(),
            license: self.license.clone(),
            license_url: self.license_url.clone(),
            terms_url: self.terms_url.clone(),
            disclaimer: self.disclaimer.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct CaseDraft {
    case_id: String,
    title: String,
    event_start: String,
    event_end: String,
    retention_days: u16,
    model: String,
    run: String,
    snapshot_id: String,
    grid_hash: String,
    variables_csv: String,
    provider: String,
    roles_csv: String,
    products_csv: String,
    data_origin: DataOrigin,
    attribution: AttributionDraft,
    modification_notice: String,
    explicit_owner_publication: bool,
    redistribution_rights_confirmed: bool,
    final_confirmation: bool,
}

impl Default for CaseDraft {
    fn default() -> Self {
        let now = chrono::Utc::now();
        Self {
            case_id: format!("case-{}", now.format("%Y%m%d-%H%M")),
            title: String::new(),
            event_start: format_case_time(now - chrono::Duration::hours(6)),
            event_end: format_case_time(now),
            retention_days: 30,
            model: "hrrr".into(),
            run: String::new(),
            snapshot_id: String::new(),
            grid_hash: String::new(),
            variables_csv: "reflectivity".into(),
            provider: "noaa-aws-public-data".into(),
            roles_csv: "surface".into(),
            products_csv: "analysis".into(),
            // A client-authored case artifact is always an explicit owner
            // publication. NOAA/ECMWF lineage remains in provenance and
            // attribution; the origin must not certify client-supplied bytes
            // as a native public-provider object.
            data_origin: DataOrigin::UserProvided,
            attribution: AttributionDraft::noaa(),
            modification_notice:
                "Created in BowEcho from the identified immutable source snapshot.".into(),
            explicit_owner_publication: false,
            redistribution_rights_confirmed: false,
            final_confirmation: false,
        }
    }
}

#[derive(Debug, Clone)]
struct PreparedArtifact {
    publication: PublishCaseArtifactRequest,
}

impl PreparedArtifact {
    fn artifact_ref(
        &self,
        signed: &rw_community_protocol::SignedObjectManifest,
    ) -> CaseArtifactRef {
        let ShareQuery::CaseArtifact {
            artifact_id,
            artifact_type,
            ..
        } = &self.publication.request.query
        else {
            unreachable!("prepared artifacts are preflight-validated")
        };
        CaseArtifactRef {
            artifact_id: artifact_id.clone(),
            artifact_type: *artifact_type,
            request_sha256: signed.manifest.request_sha256.clone(),
            object_sha256: signed.manifest.object_sha256.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct CasePublishSnapshot {
    case_id: String,
    title: String,
    event_start_unix: i64,
    event_end_unix: i64,
}

#[derive(Debug)]
struct PublicationSuccess {
    case: rw_community_protocol::SignedCaseRoomManifest,
    artifacts: Vec<rw_community_protocol::SignedObjectManifest>,
}

#[derive(Debug)]
struct PublicationFailure {
    message: String,
    artifacts: Vec<rw_community_protocol::SignedObjectManifest>,
    outcome_may_have_committed: bool,
}

struct RunningPublication {
    receiver: mpsc::Receiver<Result<PublicationSuccess, PublicationFailure>>,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug)]
struct ArtifactFetchResult {
    key: (String, String),
    artifact: VerifiedCaseArtifact,
}

struct RunningArtifactFetch {
    key: (String, String),
    receiver: mpsc::Receiver<Result<ArtifactFetchResult, CommunityCacheError>>,
    cancel: Arc<AtomicBool>,
}

struct LoadedArtifact {
    artifact: VerifiedCaseArtifact,
    texture: Option<egui::TextureHandle>,
    status: Option<String>,
}

/// Session-only case-room interaction state. No publication draft, local
/// source identity, selected image bytes, or owner principal is persisted.
#[derive(Default)]
pub(crate) struct CommunityCaseWorkspace {
    draft: CaseDraft,
    artifact_draft: ArtifactDraft,
    prepared: Vec<PreparedArtifact>,
    remotely_published: Vec<rw_community_protocol::SignedObjectManifest>,
    publication_task: Option<RunningPublication>,
    publication_status: Option<String>,
    artifact_fetch: Option<RunningArtifactFetch>,
    loaded_artifacts: BTreeMap<(String, String), LoadedArtifact>,
    artifact_status: Option<String>,
}

impl CommunityCaseWorkspace {
    pub(crate) fn has_background_work(&self) -> bool {
        self.publication_task.is_some() || self.artifact_fetch.is_some()
    }

    pub(crate) fn poll(&mut self, browser: &mut CommunityCaseBrowser) -> bool {
        let mut changed = false;
        if let Some(task) = &self.publication_task {
            match task.receiver.try_recv() {
                Ok(result) => {
                    self.publication_task = None;
                    changed = true;
                    match result {
                        Ok(success) => {
                            let title = success.case.manifest.title.clone();
                            browser.upsert_verified_case(success.case);
                            self.prepared.clear();
                            self.remotely_published.clear();
                            self.draft.final_confirmation = false;
                            self.publication_status = Some(format!(
                                "Published “{title}” with {} verified artifact{}.",
                                success.artifacts.len(),
                                if success.artifacts.len() == 1 {
                                    ""
                                } else {
                                    "s"
                                }
                            ));
                        }
                        Err(failure) => {
                            self.remotely_published = failure.artifacts;
                            let suffix = if failure.outcome_may_have_committed {
                                " The last HTTPS mutation may have reached the origin; refresh the signed directory before abandoning or retrying this exact case."
                            } else {
                                " The prepared case remains available to resume."
                            };
                            self.publication_status =
                                Some(format!("{}{}", failure.message, suffix));
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.publication_task = None;
                    self.publication_status = Some(
                        "Case publication worker stopped; the origin outcome may be unknown. Refresh the signed directory before retrying."
                            .into(),
                    );
                    changed = true;
                }
            }
        }
        if let Some(task) = &self.artifact_fetch {
            match task.receiver.try_recv() {
                Ok(result) => {
                    self.artifact_fetch = None;
                    changed = true;
                    match result {
                        Ok(result) => {
                            self.loaded_artifacts.insert(
                                result.key.clone(),
                                LoadedArtifact {
                                    artifact: result.artifact,
                                    texture: None,
                                    status: None,
                                },
                            );
                            self.artifact_status =
                                Some("Loaded and verified case artifact.".into());
                        }
                        Err(error) => {
                            // Existing verified content is deliberately not
                            // cleared on a refresh failure.
                            self.artifact_status =
                                Some(format!("Case artifact load failed: {error}"));
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.artifact_fetch = None;
                    self.artifact_status =
                        Some("Case artifact worker stopped unexpectedly.".into());
                    changed = true;
                }
            }
        }
        changed
    }

    pub(crate) fn artifact_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        settings: &settings::CommunityCacheSettings,
        signed_case: &rw_community_protocol::SignedCaseRoomManifest,
        artifact_ref: &CaseArtifactRef,
    ) {
        let key = (
            signed_case.manifest.case_id.clone(),
            artifact_ref.artifact_id.clone(),
        );
        let is_loading = self
            .artifact_fetch
            .as_ref()
            .is_some_and(|task| task.key == key);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!is_loading, egui::Button::new("Load verified artifact"))
                .on_hover_text(
                    "Fetch only the exact hash referenced by this verified signed case, then validate its typed payload before display",
                )
                .clicked()
            {
                match CommunityCacheClient::from_settings(
                    settings,
                    settings::community_cache_dir(),
                )
                .and_then(|client| {
                    self.start_artifact_fetch(client, signed_case.clone(), artifact_ref.clone())
                }) {
                    Ok(()) => self.artifact_status = Some("Loading signed case artifact…".into()),
                    Err(error) => {
                        self.artifact_status = Some(format!("Case artifact load failed: {error}"));
                    }
                }
            }
            if is_loading {
                ui.spinner();
                if ui.button("Cancel").clicked()
                    && let Some(task) = &self.artifact_fetch
                {
                    task.cancel.store(true, Ordering::Release);
                    self.artifact_status = Some("Cancelling case artifact read…".into());
                }
            }
        });
        if let Some(status) = &self.artifact_status {
            ui.weak(status);
        }
        if let Some(loaded) = self.loaded_artifacts.get_mut(&key) {
            ui.weak(format!(
                "Verified via {}",
                match loaded.artifact.tier {
                    DeliveryTier::LocalCache => "local cache",
                    DeliveryTier::R2 => "R2/CDN",
                    DeliveryTier::Origin => "Rusty Weather HTTPS origin",
                }
            ));
            render_loaded_artifact(ui, ctx, loaded, &key);
        } else {
            ui.label(
                egui::RichText::new(format!(
                    "request {}\nobject  {}",
                    artifact_ref.request_sha256, artifact_ref.object_sha256
                ))
                .monospace(),
            );
        }
    }

    pub(crate) fn publication_ui(
        &mut self,
        ui: &mut egui::Ui,
        settings: &settings::CommunityCacheSettings,
    ) {
        ui.separator();
        egui::CollapsingHeader::new("Publish a case")
            .default_open(false)
            .show(ui, |ui| {
                ui.weak(
                    "Deliberate publication only. BowEcho never publishes a search, open run, local directory, raw wrfout, or complete generation from this workflow.",
                );
                let enabled = settings.phase1_active() && settings.explicit_case_rooms;
                if !enabled {
                    ui.colored_label(
                        egui::Color32::from_rgb(244, 194, 92),
                        "Enable Community Cache and Published case rooms in Settings before publishing.",
                    );
                }
                self.case_identity_ui(ui);
                ui.separator();
                self.source_ui(ui);
                ui.separator();
                self.artifact_editor_ui(ui, settings, enabled);
                ui.separator();
                self.final_publish_ui(ui, settings, enabled);
            });
    }

    fn case_identity_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Case identity").strong());
        let identity_locked = !self.prepared.is_empty() || !self.remotely_published.is_empty();
        ui.add_enabled_ui(!identity_locked, |ui| {
            labeled_text(ui, "Case ID", &mut self.draft.case_id);
            ui.weak("Opaque public identifier: letters, numbers, '-' and '_' only.");
        });
        labeled_text(ui, "Title", &mut self.draft.title);
        ui.horizontal_wrapped(|ui| {
            ui.label("Event start (UTC)");
            ui.text_edit_singleline(&mut self.draft.event_start);
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Event end (UTC)");
            ui.text_edit_singleline(&mut self.draft.event_end);
        });
        ui.weak("Use YYYY-MM-DD HH:MM. Event bounds describe the weather case, not upload time.");
        ui.add_enabled_ui(!identity_locked, |ui| {
            ui.horizontal(|ui| {
                ui.label("Retention");
                ui.add(
                    egui::DragValue::new(&mut self.draft.retention_days)
                        .range(1..=366)
                        .suffix(" days"),
                );
            });
        });
        if identity_locked {
            ui.weak(
                "Case ID, source, and retention are locked after the first artifact is prepared.",
            );
        }
    }

    fn source_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Immutable source and rights").strong());
        let source_locked = !self.prepared.is_empty() || !self.remotely_published.is_empty();
        ui.add_enabled_ui(!source_locked, |ui| {
            egui::ComboBox::from_id_salt("community-case-origin")
                .selected_text(data_origin_label(self.draft.data_origin))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.draft.data_origin,
                        DataOrigin::PrivateWrf,
                        "Private/local WRF",
                    );
                    ui.selectable_value(
                        &mut self.draft.data_origin,
                        DataOrigin::PrivateArwen,
                        "Private/local ArWen",
                    );
                    ui.selectable_value(
                        &mut self.draft.data_origin,
                        DataOrigin::UserProvided,
                        "Owner-provided / derived",
                    );
                });
            labeled_text(ui, "Model", &mut self.draft.model);
            labeled_text(ui, "Run", &mut self.draft.run);
            labeled_text(ui, "Snapshot SHA-256", &mut self.draft.snapshot_id);
            labeled_text(ui, "Grid SHA-256", &mut self.draft.grid_hash);
            labeled_text(ui, "Variables", &mut self.draft.variables_csv);
            ui.weak("Variables are comma-separated canonical Rusty Weather names.");
            labeled_text(ui, "Provider", &mut self.draft.provider);
            labeled_text(ui, "Source roles", &mut self.draft.roles_csv);
            labeled_text(ui, "Source products", &mut self.draft.products_csv);
            ui.weak("Roles and products are comma-separated provenance labels.");
        });

        ui.colored_label(
            egui::Color32::from_rgb(244, 194, 92),
            "Every case artifact is an explicit owner publication. Public-model lineage is attribution, not an origin-attested identity. Private/local WRF and ArWen remain non-shareable by default; only the typed artifacts below are published, never raw run files.",
        );
        ui.checkbox(
            &mut self.draft.explicit_owner_publication,
            "I explicitly choose to publish this exact case and its typed artifacts",
        );
        ui.checkbox(
            &mut self.draft.redistribution_rights_confirmed,
            "I confirm I have the right to redistribute every source and derived artifact",
        );
        ui.weak(
            "Withdrawal blocks future BowEcho/origin retrieval, but cannot erase bytes someone already downloaded while publication was active.",
        );

        ui.label(egui::RichText::new("Attribution").strong());
        ui.horizontal_wrapped(|ui| {
            if ui.button("NOAA template").clicked() && !source_locked {
                self.draft.provider = "noaa-aws-public-data".into();
                self.draft.attribution = AttributionDraft::noaa();
            }
            if ui.button("ECMWF required notice").clicked() && !source_locked {
                self.draft.provider = "ecmwf-open-data".into();
                self.draft.attribution = AttributionDraft::ecmwf();
                self.draft.modification_notice =
                    "Modified and rendered by BowEcho from ECMWF Open Data.".into();
            }
        });
        ui.add_enabled_ui(!source_locked, |ui| {
            labeled_text(ui, "Notice", &mut self.draft.attribution.notice);
            labeled_text(ui, "Source URL", &mut self.draft.attribution.source_url);
            labeled_text(ui, "License", &mut self.draft.attribution.license);
            labeled_text(ui, "License URL", &mut self.draft.attribution.license_url);
            labeled_text(ui, "Terms URL", &mut self.draft.attribution.terms_url);
            labeled_text(ui, "Disclaimer", &mut self.draft.attribution.disclaimer);
            labeled_text(
                ui,
                "Modification notice",
                &mut self.draft.modification_notice,
            );
        });
    }

    fn artifact_editor_ui(
        &mut self,
        ui: &mut egui::Ui,
        settings: &settings::CommunityCacheSettings,
        enabled: bool,
    ) {
        ui.label(egui::RichText::new("Typed artifacts").strong());
        ui.horizontal_wrapped(|ui| {
            ui.label("Type");
            egui::ComboBox::from_id_salt("community-case-artifact-kind")
                .selected_text(self.artifact_draft.kind.label())
                .show_ui(ui, |ui| {
                    for kind in [
                        ArtifactEditorKind::Annotation,
                        ArtifactEditorKind::DerivedTable,
                        ArtifactEditorKind::Overlay,
                        ArtifactEditorKind::RenderedImage,
                    ] {
                        ui.selectable_value(&mut self.artifact_draft.kind, kind, kind.label());
                    }
                });
        });
        labeled_text(ui, "Artifact ID", &mut self.artifact_draft.artifact_id);
        labeled_text(ui, "Artifact title", &mut self.artifact_draft.title);
        match self.artifact_draft.kind {
            ArtifactEditorKind::Annotation => {
                ui.label("Plain-text annotation");
                ui.add(
                    egui::TextEdit::multiline(&mut self.artifact_draft.annotation_text)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY),
                );
                labeled_text(
                    ui,
                    "Optional event time (UTC)",
                    &mut self.artifact_draft.annotation_event,
                );
            }
            ArtifactEditorKind::DerivedTable => {
                ui.label("Tab-separated table (first row is the header)");
                ui.add(
                    egui::TextEdit::multiline(&mut self.artifact_draft.table_tsv)
                        .code_editor()
                        .desired_rows(8)
                        .desired_width(f32::INFINITY),
                );
                ui.weak(
                    "Up to 64 columns, 10,000 rows, and 100,000 cells. No file import or formulas.",
                );
            }
            ArtifactEditorKind::Overlay => {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Geometry");
                    egui::ComboBox::from_id_salt("community-case-overlay-kind")
                        .selected_text(self.artifact_draft.overlay_kind.label())
                        .show_ui(ui, |ui| {
                            for kind in [
                                OverlayEditorKind::Points,
                                OverlayEditorKind::Polyline,
                                OverlayEditorKind::Polygon,
                            ] {
                                ui.selectable_value(
                                    &mut self.artifact_draft.overlay_kind,
                                    kind,
                                    kind.label(),
                                );
                            }
                        });
                });
                ui.label("Coordinates (one longitude,latitude pair per line)");
                ui.add(
                    egui::TextEdit::multiline(&mut self.artifact_draft.overlay_coordinates)
                        .code_editor()
                        .desired_rows(8)
                        .desired_width(f32::INFINITY),
                );
                ui.weak("Fixed 1e-7-degree coordinates only; polygons must repeat the first coordinate at the end.");
            }
            ArtifactEditorKind::RenderedImage => {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Choose PNG or WebP…").clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("Published image", &["png", "webp"])
                            .set_title("Choose a typed case-room image")
                            .pick_file()
                    {
                        match read_selected_image(&path) {
                            Ok(image) => {
                                self.artifact_draft.image = Some(image);
                                self.publication_status =
                                    Some("Image passed client-side type and size checks.".into());
                            }
                            Err(error) => self.publication_status = Some(error),
                        }
                    }
                    if let Some(image) = &self.artifact_draft.image {
                        ui.weak(format!(
                            "{} · {}×{} · {:.1} MiB",
                            image.display_name,
                            image.width,
                            image.height,
                            image.bytes.len() as f64 / (1024.0 * 1024.0)
                        ));
                    }
                });
                labeled_text(ui, "Alt text", &mut self.artifact_draft.image_alt_text);
                ui.weak("Only a validated still PNG/WebP payload is retained in memory. Its local path is never published.");
            }
        }

        if ui
            .add_enabled(
                enabled && self.publication_task.is_none(),
                egui::Button::new("Add typed artifact to case"),
            )
            .on_hover_text("Validate and prepare locally; this button does not upload")
            .clicked()
        {
            let result =
                CommunityCacheClient::from_settings(settings, settings::community_cache_dir())
                    .and_then(|client| client.owner_principal_sha256())
                    .map_err(|error| error.to_string())
                    .and_then(|owner| {
                        build_artifact_publication(&self.draft, &self.artifact_draft, &owner)
                    });
            match result {
                Ok(publication) => {
                    let duplicate = self.prepared.iter().any(|prepared| {
                        prepared.publication.request.query == publication.request.query
                    });
                    if duplicate {
                        self.publication_status = Some(
                            "That artifact ID is already prepared for this case; choose a unique ID."
                                .into(),
                        );
                    } else if let Some(first) = self.prepared.first()
                        && !publication_identity_matches(&first.publication, &publication)
                    {
                        self.publication_status = Some(
                            "Prepared artifacts must share one exact case ID, source snapshot, origin policy, attribution, and retention. Discard the local prepared set to change them."
                                .into(),
                        );
                    } else {
                        self.prepared.push(PreparedArtifact { publication });
                        self.artifact_draft.artifact_id =
                            format!("artifact-{}", self.prepared.len() + 1);
                        self.artifact_draft.annotation_text.clear();
                        self.artifact_draft.overlay_coordinates.clear();
                        self.artifact_draft.image = None;
                        self.draft.final_confirmation = false;
                        self.publication_status = Some(
                            "Typed artifact prepared locally. Nothing has been uploaded yet."
                                .into(),
                        );
                    }
                }
                Err(error) => {
                    self.publication_status = Some(format!("Artifact was not prepared: {error}"));
                }
            }
        }

        for prepared in &self.prepared {
            let ShareQuery::CaseArtifact {
                artifact_id,
                artifact_type,
                ..
            } = &prepared.publication.request.query
            else {
                continue;
            };
            let already_remote = self
                .remotely_published
                .iter()
                .any(|signed| signed.manifest.request == prepared.publication.request);
            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "{} · {}",
                    artifact_id,
                    artifact_type_label(*artifact_type)
                ));
                ui.weak(if already_remote {
                    "origin-verified; awaiting case manifest"
                } else {
                    "local only"
                });
            });
        }
        if !self.prepared.is_empty()
            && self.remotely_published.is_empty()
            && self.publication_task.is_none()
            && ui.button("Discard locally prepared artifacts").clicked()
        {
            self.prepared.clear();
            self.draft.final_confirmation = false;
            self.publication_status =
                Some("Discarded local preparation; nothing was uploaded.".into());
        }
    }

    fn final_publish_ui(
        &mut self,
        ui: &mut egui::Ui,
        settings: &settings::CommunityCacheSettings,
        enabled: bool,
    ) {
        ui.label(egui::RichText::new("Final publication").strong());
        ui.checkbox(
            &mut self.draft.final_confirmation,
            format!(
                "Publish this exact case and all {} prepared artifact{}",
                self.prepared.len(),
                if self.prepared.len() == 1 { "" } else { "s" }
            ),
        );
        ui.weak(
            "The origin signs immutable identities. A network failure can make the last mutation's outcome uncertain; BowEcho will keep this exact draft for reconciliation.",
        );
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    enabled
                        && !self.prepared.is_empty()
                        && self.draft.final_confirmation
                        && self.publication_task.is_none(),
                    egui::Button::new(if self.remotely_published.is_empty() {
                        "Publish case"
                    } else {
                        "Resume exact publication"
                    }),
                )
                .clicked()
            {
                match CommunityCacheClient::from_settings(
                    settings,
                    settings::community_cache_dir(),
                )
                .map_err(|error| error.to_string())
                .and_then(|client| self.start_publication(client))
                {
                    Ok(()) => {
                        self.publication_status = Some(
                            "Publishing typed artifacts, then the signed case manifest…".into(),
                        );
                    }
                    Err(error) => {
                        self.publication_status =
                            Some(format!("Case publication did not start: {error}"));
                    }
                }
            }
            if let Some(task) = &self.publication_task {
                ui.spinner();
                if ui.button("Cancel after current request").clicked() {
                    task.cancel.store(true, Ordering::Release);
                    self.publication_status = Some(
                        "Cancellation requested. Any request already accepted by the origin will still be reported and retained for an exact resume."
                            .into(),
                    );
                }
            }
        });
        if let Some(status) = &self.publication_status {
            let warning = status.contains("failed")
                || status.contains("not ")
                || status.contains("unknown")
                || status.contains("must ");
            if warning {
                ui.colored_label(egui::Color32::from_rgb(244, 194, 92), status);
            } else {
                ui.weak(status);
            }
        }
    }

    fn start_publication(&mut self, client: CommunityCacheClient) -> Result<(), String> {
        if self.publication_task.is_some() || self.prepared.is_empty() {
            return Err("another case publication is active or no artifacts are prepared".into());
        }
        let snapshot = build_case_snapshot(&self.draft)?;
        let prepared = self.prepared.clone();
        let existing = self.remotely_published.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("community-case-publish".into())
            .spawn(move || {
                let result = run_publication(client, snapshot, prepared, existing, &worker_cancel);
                let _ = sender.send(result);
            })
            .map_err(|_| "BowEcho could not start the case publication worker".to_owned())?;
        self.publication_task = Some(RunningPublication { receiver, cancel });
        Ok(())
    }

    fn start_artifact_fetch(
        &mut self,
        client: CommunityCacheClient,
        signed_case: rw_community_protocol::SignedCaseRoomManifest,
        artifact_ref: CaseArtifactRef,
    ) -> Result<(), CommunityCacheError> {
        if self.artifact_fetch.is_some() {
            return Err(CommunityCacheError::Quota);
        }
        let key = (
            signed_case.manifest.case_id.clone(),
            artifact_ref.artifact_id.clone(),
        );
        let worker_key = key.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("community-case-artifact".into())
            .spawn(move || {
                if worker_cancel.load(Ordering::Acquire) {
                    let _ = sender.send(Err(CommunityCacheError::Cancelled));
                    return;
                }
                let result = client.fetch_case_artifact(&signed_case, &artifact_ref);
                if worker_cancel.load(Ordering::Acquire) {
                    let _ = sender.send(Err(CommunityCacheError::Cancelled));
                    return;
                }
                let _ = sender.send(result.map(|artifact| ArtifactFetchResult {
                    key: worker_key,
                    artifact,
                }));
            })
            .map_err(|_| CommunityCacheError::Network)?;
        self.artifact_fetch = Some(RunningArtifactFetch {
            key,
            receiver,
            cancel,
        });
        Ok(())
    }
}

fn run_publication(
    client: CommunityCacheClient,
    snapshot: CasePublishSnapshot,
    prepared: Vec<PreparedArtifact>,
    mut signed_artifacts: Vec<rw_community_protocol::SignedObjectManifest>,
    cancel: &AtomicBool,
) -> Result<PublicationSuccess, PublicationFailure> {
    for artifact in &prepared {
        let already_complete = signed_artifacts
            .iter()
            .any(|signed| signed.manifest.request == artifact.publication.request);
        if already_complete {
            continue;
        }
        if cancel.load(Ordering::Acquire) {
            return Err(PublicationFailure {
                message: "Case publication cancelled before the next HTTPS mutation.".into(),
                artifacts: signed_artifacts,
                outcome_may_have_committed: false,
            });
        }
        match client.publish_case_artifact(&artifact.publication) {
            Ok(signed) => signed_artifacts.push(signed),
            Err(error) => {
                return Err(PublicationFailure {
                    message: format!("Typed artifact publication failed: {error}"),
                    artifacts: signed_artifacts,
                    outcome_may_have_committed: true,
                });
            }
        }
        if cancel.load(Ordering::Acquire) {
            return Err(PublicationFailure {
                message: "Case publication stopped after the origin-confirmed artifact.".into(),
                artifacts: signed_artifacts,
                outcome_may_have_committed: false,
            });
        }
    }

    let manifest = match build_case_manifest(&snapshot, &prepared, &signed_artifacts) {
        Ok(manifest) => manifest,
        Err(message) => {
            return Err(PublicationFailure {
                message,
                artifacts: signed_artifacts,
                outcome_may_have_committed: false,
            });
        }
    };
    if cancel.load(Ordering::Acquire) {
        return Err(PublicationFailure {
            message: "Case publication cancelled before the final manifest request.".into(),
            artifacts: signed_artifacts,
            outcome_may_have_committed: false,
        });
    }
    match client.publish_case(&manifest) {
        Ok(case) => Ok(PublicationSuccess {
            case,
            artifacts: signed_artifacts,
        }),
        Err(error) => Err(PublicationFailure {
            message: format!("Final case-manifest publication failed: {error}"),
            artifacts: signed_artifacts,
            outcome_may_have_committed: true,
        }),
    }
}

fn build_artifact_publication(
    case: &CaseDraft,
    artifact: &ArtifactDraft,
    owner_principal_sha256: &str,
) -> Result<PublishCaseArtifactRequest, String> {
    if !case.explicit_owner_publication {
        return Err("explicit owner publication is not confirmed".into());
    }
    if !case.redistribution_rights_confirmed {
        return Err("redistribution rights are not confirmed".into());
    }
    if case.data_origin == DataOrigin::PublicProvider {
        return Err(
            "client-authored case artifacts cannot claim public-provider identity; use owner-provided and keep public lineage in attribution"
                .into(),
        );
    }
    if case.data_origin == DataOrigin::PrivateWrf
        && !case.model.to_ascii_lowercase().contains("wrf")
    {
        return Err("a private WRF publication must identify a WRF model".into());
    }
    if case.data_origin == DataOrigin::PrivateArwen
        && !case.model.to_ascii_lowercase().contains("arwen")
    {
        return Err("a private ArWen publication must identify an ArWen model".into());
    }
    let payload = build_payload(artifact)?;
    let now = chrono::Utc::now().timestamp();
    let retention_seconds = i64::from(case.retention_days)
        .checked_mul(24 * 60 * 60)
        .ok_or_else(|| "retention overflowed".to_owned())?;
    let retain_until_unix = now
        .checked_add(retention_seconds)
        .ok_or_else(|| "retention overflowed".to_owned())?;
    let mut parameters = BTreeMap::new();
    parameters.insert(
        PUBLICATION_OWNER_PARAMETER.into(),
        owner_principal_sha256.into(),
    );
    let request = ShareRequest {
        schema: REQUEST_SCHEMA.into(),
        model: case.model.clone(),
        run: case.run.clone(),
        snapshot_id: case.snapshot_id.clone(),
        grid_hash: case.grid_hash.clone(),
        variables: parse_token_list(&case.variables_csv),
        query: ShareQuery::CaseArtifact {
            case_id: case.case_id.clone(),
            artifact_id: artifact.artifact_id.clone(),
            artifact_type: payload.artifact_type(),
        },
        recipe: RecipeIdentity {
            recipe_id: format!(
                "bowecho-case-{}",
                artifact_type_slug(payload.artifact_type())
            ),
            recipe_version: "1".into(),
            parameters,
        },
        source_provenance: vec![SourceProvenance {
            provider: case.provider.clone(),
            roles: parse_token_list(&case.roles_csv),
            products: parse_token_list(&case.products_csv),
        }],
        publication: PublicationGrant {
            data_origin: case.data_origin,
            explicit_owner_publication: true,
            redistribution_rights_confirmed: true,
        },
    }
    .normalized();
    let publication = PublishCaseArtifactRequest {
        schema: CASE_ARTIFACT_PUBLICATION_SCHEMA.into(),
        owner_principal_sha256: owner_principal_sha256.into(),
        request,
        payload,
        published_unix: now,
        retain_until_unix,
        attributions: vec![case.attribution.build(&case.provider)],
        modification_notices: vec![case.modification_notice.clone()],
    };
    publication
        .validate(&ProtocolLimits::default())
        .map_err(|error| error.to_string())?;
    Ok(publication)
}

fn build_payload(draft: &ArtifactDraft) -> Result<CaseArtifactPayload, String> {
    let payload = match draft.kind {
        ArtifactEditorKind::Annotation => {
            if draft.annotation_text.len() > MAX_CLIENT_ANNOTATION_BYTES {
                return Err("annotation exceeds the 64 KiB client limit".into());
            }
            let event_unix = if draft.annotation_event.trim().is_empty() {
                None
            } else {
                Some(parse_case_time(&draft.annotation_event)?)
            };
            CaseArtifactPayload::Annotation(AnnotationArtifact {
                title: draft.title.clone(),
                text: draft.annotation_text.clone(),
                event_unix,
            })
        }
        ArtifactEditorKind::DerivedTable => {
            CaseArtifactPayload::DerivedTable(parse_table(&draft.title, &draft.table_tsv)?)
        }
        ArtifactEditorKind::Overlay => CaseArtifactPayload::Overlay(parse_overlay(
            &draft.title,
            draft.overlay_kind,
            &draft.overlay_coordinates,
        )?),
        ArtifactEditorKind::RenderedImage => {
            let selected = draft
                .image
                .as_ref()
                .ok_or_else(|| "choose a PNG or WebP image first".to_owned())?;
            if selected.bytes.len() as u64 > MAX_CLIENT_IMAGE_BYTES {
                return Err("image exceeds the 8 MiB client publication limit".into());
            }
            CaseArtifactPayload::RenderedImage(RenderedImageArtifact {
                format: selected.format,
                width: selected.width,
                height: selected.height,
                alt_text: draft.image_alt_text.clone(),
                bytes_base64: encode_base64(&selected.bytes),
            })
        }
    };
    payload
        .validate(&ProtocolLimits::default())
        .map_err(|error| error.to_string())?;
    Ok(payload)
}

fn build_case_snapshot(draft: &CaseDraft) -> Result<CasePublishSnapshot, String> {
    if !draft.explicit_owner_publication || !draft.redistribution_rights_confirmed {
        return Err("explicit publication and redistribution rights must remain confirmed".into());
    }
    let event_start_unix = parse_case_time(&draft.event_start)?;
    let event_end_unix = parse_case_time(&draft.event_end)?;
    if event_start_unix >= event_end_unix {
        return Err("event start must be before event end".into());
    }
    Ok(CasePublishSnapshot {
        case_id: draft.case_id.clone(),
        title: draft.title.clone(),
        event_start_unix,
        event_end_unix,
    })
}

fn build_case_manifest(
    snapshot: &CasePublishSnapshot,
    prepared: &[PreparedArtifact],
    signed_artifacts: &[rw_community_protocol::SignedObjectManifest],
) -> Result<CaseRoomManifest, String> {
    if prepared.is_empty() || prepared.len() != signed_artifacts.len() {
        return Err("The origin has not verified every prepared artifact yet.".into());
    }
    let now = chrono::Utc::now().timestamp();
    let retain_until_unix = prepared
        .iter()
        .map(|artifact| artifact.publication.retain_until_unix)
        .min()
        .ok_or_else(|| "case has no artifact retention".to_owned())?;
    let data_origin = prepared[0].publication.request.publication.data_origin;
    let mut sources = Vec::new();
    let mut artifacts = Vec::new();
    let mut attributions = Vec::new();
    let mut modification_notices = Vec::new();
    let mut seen_requests = BTreeSet::new();

    for prepared_artifact in prepared {
        let request_hash = request_sha256(&prepared_artifact.publication.request)
            .map_err(|error| error.to_string())?;
        let signed = signed_artifacts
            .iter()
            .find(|signed| {
                signed.manifest.request_sha256 == request_hash
                    && signed.manifest.request == prepared_artifact.publication.request
            })
            .ok_or_else(|| "origin-verified artifact identity is missing".to_owned())?;
        if !seen_requests.insert(request_hash) {
            return Err("case contains a duplicate artifact request".into());
        }
        if signed.manifest.expires_unix < retain_until_unix
            || signed.manifest.request.publication.data_origin != data_origin
        {
            return Err("artifact retention or origin policy is inconsistent".into());
        }
        artifacts.push(prepared_artifact.artifact_ref(signed));
        let source = CaseModelSource {
            model: signed.manifest.request.model.clone(),
            run: signed.manifest.request.run.clone(),
            snapshot_id: signed.manifest.request.snapshot_id.clone(),
            grid_hash: signed.manifest.request.grid_hash.clone(),
            source_provenance: signed.manifest.request.source_provenance.clone(),
        };
        if !sources.contains(&source) {
            sources.push(source);
        }
        for attribution in &signed.manifest.attributions {
            if !attributions.contains(attribution) {
                attributions.push(attribution.clone());
            }
        }
        for notice in &signed.manifest.modification_notices {
            if !modification_notices.contains(notice) {
                modification_notices.push(notice.clone());
            }
        }
    }
    artifacts.sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
    sources.sort_by(|left, right| {
        (&left.model, &left.run, &left.snapshot_id, &left.grid_hash).cmp(&(
            &right.model,
            &right.run,
            &right.snapshot_id,
            &right.grid_hash,
        ))
    });
    let manifest = CaseRoomManifest {
        schema: CASE_SCHEMA.into(),
        case_id: snapshot.case_id.clone(),
        title: snapshot.title.clone(),
        event_start_unix: snapshot.event_start_unix,
        event_end_unix: snapshot.event_end_unix,
        published_unix: now,
        retain_until_unix,
        publication: PublicationGrant {
            data_origin,
            explicit_owner_publication: true,
            redistribution_rights_confirmed: true,
        },
        sources,
        artifacts,
        attributions,
        modification_notices,
    };
    canonical_case_manifest_bytes(&manifest, "bowecho-case-preflight-v1")
        .map_err(|error| error.to_string())?;
    Ok(manifest)
}

fn publication_identity_matches(
    first: &PublishCaseArtifactRequest,
    candidate: &PublishCaseArtifactRequest,
) -> bool {
    let (
        ShareQuery::CaseArtifact {
            case_id: first_id, ..
        },
        ShareQuery::CaseArtifact {
            case_id: candidate_id,
            ..
        },
    ) = (&first.request.query, &candidate.request.query)
    else {
        return false;
    };
    first_id == candidate_id
        && first.request.model == candidate.request.model
        && first.request.run == candidate.request.run
        && first.request.snapshot_id == candidate.request.snapshot_id
        && first.request.grid_hash == candidate.request.grid_hash
        && first.request.source_provenance == candidate.request.source_provenance
        && first.request.publication == candidate.request.publication
        && first.retain_until_unix.saturating_sub(first.published_unix)
            == candidate
                .retain_until_unix
                .saturating_sub(candidate.published_unix)
        && first.attributions == candidate.attributions
        && first.modification_notices == candidate.modification_notices
}

fn parse_table(title: &str, text: &str) -> Result<DerivedTableArtifact, String> {
    let mut lines = text.lines();
    let columns = lines
        .next()
        .ok_or_else(|| "table needs a header row".to_owned())?
        .split('\t')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if columns.is_empty() || columns.len() > MAX_CLIENT_TABLE_COLUMNS {
        return Err("table must have 1–64 columns".into());
    }
    let mut rows = Vec::new();
    for line in lines {
        if rows.len() >= MAX_CLIENT_TABLE_ROWS {
            return Err("table exceeds the 10,000-row client limit".into());
        }
        let cells = line.split('\t').map(parse_table_cell).collect::<Vec<_>>();
        if cells.len() != columns.len() {
            return Err(format!(
                "table row {} has {} cells; expected {}",
                rows.len() + 2,
                cells.len(),
                columns.len()
            ));
        }
        rows.push(cells);
    }
    if rows
        .len()
        .checked_mul(columns.len())
        .is_none_or(|cells| cells > MAX_CLIENT_TABLE_CELLS)
    {
        return Err("table exceeds the 100,000-cell client limit".into());
    }
    Ok(DerivedTableArtifact {
        title: title.to_owned(),
        columns,
        rows,
    })
}

fn parse_table_cell(value: &str) -> TableCell {
    if value.is_empty() {
        TableCell::Missing
    } else if value.eq_ignore_ascii_case("true") {
        TableCell::Boolean { value: true }
    } else if value.eq_ignore_ascii_case("false") {
        TableCell::Boolean { value: false }
    } else if let Ok(value) = value.parse::<i64>() {
        TableCell::Integer { value }
    } else if let Some(value_e6) = parse_decimal_e6(value) {
        TableCell::FixedDecimal { value_e6 }
    } else {
        TableCell::Text {
            value: value.to_owned(),
        }
    }
}

fn parse_decimal_e6(value: &str) -> Option<i64> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let (whole, fraction) = unsigned.split_once('.')?;
    if whole.is_empty()
        || fraction.is_empty()
        || fraction.len() > 6
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.parse::<i64>().ok()?;
    let fraction_digits = fraction.len();
    let fraction = fraction.parse::<i64>().ok()?;
    let scale = 10_i64.checked_pow(u32::try_from(6 - fraction_digits).ok()?)?;
    let value = whole
        .checked_mul(1_000_000)?
        .checked_add(fraction.checked_mul(scale)?)?;
    if negative {
        value.checked_neg()
    } else {
        Some(value)
    }
}

fn parse_overlay(
    title: &str,
    kind: OverlayEditorKind,
    text: &str,
) -> Result<OverlayArtifact, String> {
    let coordinates = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_coordinate)
        .collect::<Result<Vec<_>, _>>()?;
    if coordinates.is_empty() || coordinates.len() > MAX_CLIENT_OVERLAY_COORDINATES {
        return Err("overlay needs 1–10,000 coordinates".into());
    }
    let features = match kind {
        OverlayEditorKind::Points => coordinates
            .into_iter()
            .enumerate()
            .map(|(index, coordinate)| OverlayFeature {
                feature_id: format!("point-{}", index + 1),
                geometry: OverlayGeometry::Point { coordinate },
                properties: BTreeMap::new(),
            })
            .collect(),
        OverlayEditorKind::Polyline => {
            if coordinates.len() < 2 {
                return Err("a polyline needs at least two coordinates".into());
            }
            vec![OverlayFeature {
                feature_id: "line-1".into(),
                geometry: OverlayGeometry::Polyline { coordinates },
                properties: BTreeMap::new(),
            }]
        }
        OverlayEditorKind::Polygon => {
            if coordinates.len() < 4 || coordinates.first() != coordinates.last() {
                return Err(
                    "a polygon needs at least four coordinates and must repeat its first coordinate at the end"
                        .into(),
                );
            }
            vec![OverlayFeature {
                feature_id: "polygon-1".into(),
                geometry: OverlayGeometry::Polygon { ring: coordinates },
                properties: BTreeMap::new(),
            }]
        }
    };
    Ok(OverlayArtifact {
        title: title.to_owned(),
        features,
    })
}

fn parse_coordinate(line: &str) -> Result<FixedCoordinate, String> {
    let values = line
        .split([',', ' ', '\t'])
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if values.len() != 2 {
        return Err(format!("invalid longitude,latitude pair: {line}"));
    }
    let longitude = values[0]
        .parse::<f64>()
        .map_err(|_| format!("invalid longitude: {}", values[0]))?;
    let latitude = values[1]
        .parse::<f64>()
        .map_err(|_| format!("invalid latitude: {}", values[1]))?;
    if !longitude.is_finite()
        || !latitude.is_finite()
        || !(-180.0..=180.0).contains(&longitude)
        || !(-90.0..=90.0).contains(&latitude)
    {
        return Err("overlay coordinate is outside Earth bounds".into());
    }
    let longitude_e7 = (longitude * 10_000_000.0).round();
    let latitude_e7 = (latitude * 10_000_000.0).round();
    if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&longitude_e7)
        || !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&latitude_e7)
    {
        return Err("overlay fixed-coordinate conversion overflowed".into());
    }
    Ok(FixedCoordinate {
        longitude_e7: longitude_e7 as i32,
        latitude_e7: latitude_e7 as i32,
    })
}

fn read_selected_image(path: &Path) -> Result<SelectedImage, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "BowEcho could not inspect the selected image".to_owned())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Selected image must be a regular, non-symlink file".into());
    }
    if metadata.len() == 0 || metadata.len() > MAX_CLIENT_IMAGE_BYTES {
        return Err("Selected image must be non-empty and no larger than 8 MiB".into());
    }
    let mut file =
        File::open(path).map_err(|_| "BowEcho could not open the selected image".to_owned())?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_CLIENT_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "BowEcho could not read the selected image".to_owned())?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_CLIENT_IMAGE_BYTES {
        return Err("Selected image changed or exceeded the 8 MiB limit while reading".into());
    }
    let (format, width, height) = image_identity(&bytes)
        .ok_or_else(|| "Selected file is not a supported still PNG or WebP".to_owned())?;
    if u64::from(width)
        .checked_mul(u64::from(height))
        .is_none_or(|pixels| pixels > MAX_CLIENT_IMAGE_PIXELS)
    {
        return Err("Selected image dimensions exceed the client publication limit".into());
    }
    let display_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("selected image")
        .to_owned();
    Ok(SelectedImage {
        display_name,
        format,
        width,
        height,
        bytes,
    })
}

fn image_identity(bytes: &[u8]) -> Option<(RenderedImageFormat, u32, u32)> {
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR" {
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return (width > 0 && height > 0).then_some((RenderedImageFormat::Png, width, height));
    }
    if bytes.len() < 30 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let dimensions = match &bytes[12..16] {
        b"VP8X" if bytes[20] & 0x02 == 0 => {
            let width = 1
                + u32::from(bytes[24])
                + (u32::from(bytes[25]) << 8)
                + (u32::from(bytes[26]) << 16);
            let height = 1
                + u32::from(bytes[27])
                + (u32::from(bytes[28]) << 8)
                + (u32::from(bytes[29]) << 16);
            Some((width, height))
        }
        b"VP8L" if bytes.len() >= 25 && bytes[20] == 0x2f => {
            let bits = u32::from_le_bytes(bytes[21..25].try_into().ok()?);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8 " if bytes.len() >= 30 && &bytes[23..26] == b"\x9d\x01\x2a" => {
            let width = u16::from_le_bytes(bytes[26..28].try_into().ok()?) & 0x3fff;
            let height = u16::from_le_bytes(bytes[28..30].try_into().ok()?) & 0x3fff;
            (width > 0 && height > 0).then_some((u32::from(width), u32::from(height)))
        }
        _ => None,
    }?;
    Some((RenderedImageFormat::Webp, dimensions.0, dimensions.1))
}

fn render_loaded_artifact(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    loaded: &mut LoadedArtifact,
    key: &(String, String),
) {
    match &loaded.artifact.payload {
        CaseArtifactPayload::Annotation(annotation) => {
            ui.label(egui::RichText::new(&annotation.title).strong());
            if let Some(event) = annotation.event_unix {
                ui.weak(format!("Event time: {}", format_unix(event)));
            }
            ui.label(&annotation.text);
        }
        CaseArtifactPayload::DerivedTable(table) => render_table(ui, table),
        CaseArtifactPayload::Overlay(overlay) => render_overlay(ui, overlay),
        CaseArtifactPayload::RenderedImage(image) => {
            ui.label(egui::RichText::new(&image.alt_text).strong());
            ui.weak(format!(
                "{} × {} · {}",
                image.width,
                image.height,
                match image.format {
                    RenderedImageFormat::Png => "PNG",
                    RenderedImageFormat::Webp => "WebP",
                }
            ));
            let bytes = decode_base64(&image.bytes_base64);
            if loaded.texture.is_none()
                && image.format == RenderedImageFormat::Png
                && u64::from(image.width).saturating_mul(u64::from(image.height))
                    <= MAX_BROWSER_IMAGE_PIXELS
                && let Ok(bytes) = &bytes
            {
                match image::load_from_memory_with_format(bytes, image::ImageFormat::Png) {
                    Ok(decoded) => {
                        let rgba = decoded.to_rgba8();
                        let dimensions = [rgba.width() as usize, rgba.height() as usize];
                        let color =
                            egui::ColorImage::from_rgba_unmultiplied(dimensions, rgba.as_raw());
                        loaded.texture = Some(ctx.load_texture(
                            format!("case-artifact-{}-{}", key.0, key.1),
                            color,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                    Err(_) => {
                        loaded.status = Some(
                            "Verified PNG could not be decoded by the local image renderer.".into(),
                        );
                    }
                }
            }
            if let Some(texture) = &loaded.texture {
                let source = texture.size_vec2();
                let maximum = egui::vec2(ui.available_width().max(64.0), 420.0);
                let scale = (maximum.x / source.x).min(maximum.y / source.y).min(1.0);
                ui.add(egui::Image::new((texture.id(), source * scale)));
            } else if image.format == RenderedImageFormat::Webp {
                ui.weak(
                    "This build verifies WebP identity and dimensions but does not decode WebP pixels in-process. Save the verified bytes to open them in a system viewer.",
                );
            } else if u64::from(image.width).saturating_mul(u64::from(image.height))
                > MAX_BROWSER_IMAGE_PIXELS
            {
                ui.weak("Image is verified but exceeds the safe in-app pixel preview limit.");
            }
            if ui.button("Save verified image…").clicked() {
                match bytes {
                    Ok(bytes) => {
                        let extension = match image.format {
                            RenderedImageFormat::Png => "png",
                            RenderedImageFormat::Webp => "webp",
                        };
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("Verified case image", &[extension])
                            .set_file_name(format!("{}.{}", key.1, extension))
                            .save_file()
                        {
                            loaded.status =
                                Some(match rw_store::atomic::atomic_write_bytes(&path, &bytes) {
                                    Ok(()) => "Saved the verified image bytes.".into(),
                                    Err(_) => "BowEcho could not save the verified image.".into(),
                                });
                        }
                    }
                    Err(error) => loaded.status = Some(error),
                }
            }
            if let Some(status) = &loaded.status {
                ui.weak(status);
            }
        }
    }
}

fn render_table(ui: &mut egui::Ui, table: &DerivedTableArtifact) {
    ui.label(egui::RichText::new(&table.title).strong());
    let column_count = table.columns.len().min(MAX_RENDERED_TABLE_COLUMNS);
    let row_count = table.rows.len().min(MAX_RENDERED_TABLE_ROWS);
    egui::ScrollArea::horizontal()
        .id_salt(("case-table", &table.title))
        .max_height(320.0)
        .show(ui, |ui| {
            egui::Grid::new(("case-table-grid", &table.title))
                .striped(true)
                .show(ui, |ui| {
                    for column in table.columns.iter().take(column_count) {
                        ui.label(egui::RichText::new(column).strong());
                    }
                    ui.end_row();
                    for row in table.rows.iter().take(row_count) {
                        for cell in row.iter().take(column_count) {
                            ui.label(table_cell_label(cell));
                        }
                        ui.end_row();
                    }
                });
        });
    if table.rows.len() > row_count || table.columns.len() > column_count {
        ui.weak(format!(
            "Preview limited to {row_count}/{} rows and {column_count}/{} columns.",
            table.rows.len(),
            table.columns.len()
        ));
    }
}

fn render_overlay(ui: &mut egui::Ui, overlay: &OverlayArtifact) {
    ui.label(egui::RichText::new(&overlay.title).strong());
    let mut coordinates = Vec::new();
    for feature in &overlay.features {
        let feature_coordinates: &[FixedCoordinate] = match &feature.geometry {
            OverlayGeometry::Point { coordinate } => std::slice::from_ref(coordinate),
            OverlayGeometry::Polyline { coordinates } => coordinates,
            OverlayGeometry::Polygon { ring } => ring,
        };
        let remaining = MAX_RENDERED_OVERLAY_COORDINATES.saturating_sub(coordinates.len());
        coordinates.extend(feature_coordinates.iter().take(remaining).copied());
        if coordinates.len() >= MAX_RENDERED_OVERLAY_COORDINATES {
            break;
        }
    }
    ui.weak(format!(
        "{} feature{} · {} preview coordinate{}",
        overlay.features.len(),
        if overlay.features.len() == 1 { "" } else { "s" },
        coordinates.len(),
        if coordinates.len() == 1 { "" } else { "s" }
    ));
    if coordinates.is_empty() {
        return;
    }
    let min_lon = coordinates
        .iter()
        .map(|coordinate| coordinate.longitude_e7)
        .min()
        .unwrap_or(0);
    let max_lon = coordinates
        .iter()
        .map(|coordinate| coordinate.longitude_e7)
        .max()
        .unwrap_or(0);
    let min_lat = coordinates
        .iter()
        .map(|coordinate| coordinate.latitude_e7)
        .min()
        .unwrap_or(0);
    let max_lat = coordinates
        .iter()
        .map(|coordinate| coordinate.latitude_e7)
        .max()
        .unwrap_or(0);
    let width = ui.available_width().clamp(160.0, 520.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 220.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    painter.rect_stroke(
        rect,
        3.0,
        ui.visuals().window_stroke(),
        egui::StrokeKind::Inside,
    );
    let project = |coordinate: FixedCoordinate| {
        let lon_span = f64::from(max_lon.saturating_sub(min_lon)).max(1.0);
        let lat_span = f64::from(max_lat.saturating_sub(min_lat)).max(1.0);
        let x = f64::from(coordinate.longitude_e7.saturating_sub(min_lon)) / lon_span;
        let y = f64::from(coordinate.latitude_e7.saturating_sub(min_lat)) / lat_span;
        egui::pos2(
            rect.left() + 8.0 + x as f32 * (rect.width() - 16.0),
            rect.bottom() - 8.0 - y as f32 * (rect.height() - 16.0),
        )
    };
    let stroke = egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(76, 178, 236));
    let mut rendered = 0usize;
    for feature in &overlay.features {
        if rendered >= MAX_RENDERED_OVERLAY_COORDINATES {
            break;
        }
        match &feature.geometry {
            OverlayGeometry::Point { coordinate } => {
                painter.circle_filled(project(*coordinate), 3.0, stroke.color);
                rendered += 1;
            }
            OverlayGeometry::Polyline { coordinates } => {
                let points = coordinates
                    .iter()
                    .take(MAX_RENDERED_OVERLAY_COORDINATES.saturating_sub(rendered))
                    .map(|coordinate| project(*coordinate))
                    .collect::<Vec<_>>();
                rendered += points.len();
                painter.add(egui::Shape::line(points, stroke));
            }
            OverlayGeometry::Polygon { ring } => {
                let points = ring
                    .iter()
                    .take(MAX_RENDERED_OVERLAY_COORDINATES.saturating_sub(rendered))
                    .map(|coordinate| project(*coordinate))
                    .collect::<Vec<_>>();
                rendered += points.len();
                painter.add(egui::Shape::closed_line(points, stroke));
            }
        }
    }
}

fn table_cell_label(cell: &TableCell) -> String {
    match cell {
        TableCell::Text { value } => value.clone(),
        TableCell::Integer { value } => value.to_string(),
        TableCell::FixedDecimal { value_e6 } => format!("{:.6}", *value_e6 as f64 / 1_000_000.0),
        TableCell::Boolean { value } => value.to_string(),
        TableCell::Missing => "—".into(),
    }
}

fn parse_case_time(value: &str) -> Result<i64, String> {
    let value = value.trim();
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(time.timestamp());
    }
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M")
        .map(|time| time.and_utc().timestamp())
        .map_err(|_| "UTC time must use YYYY-MM-DD HH:MM or RFC 3339".into())
}

fn format_case_time(time: chrono::DateTime<chrono::Utc>) -> String {
    time.format("%Y-%m-%d %H:%M").to_string()
}

fn format_unix(unix: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0)
        .map_or_else(|| "invalid signed time".into(), format_case_time)
}

fn parse_token_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn labeled_text(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
    });
}

fn data_origin_label(origin: DataOrigin) -> &'static str {
    match origin {
        DataOrigin::PublicProvider => "Public provider",
        DataOrigin::PrivateWrf => "Private/local WRF",
        DataOrigin::PrivateArwen => "Private/local ArWen",
        DataOrigin::UserProvided => "Other owner-provided",
    }
}

fn artifact_type_label(artifact_type: CaseArtifactType) -> &'static str {
    match artifact_type {
        CaseArtifactType::Annotation => "annotation",
        CaseArtifactType::DerivedTable => "derived table",
        CaseArtifactType::Overlay => "fixed-coordinate overlay",
        CaseArtifactType::RenderedImage => "rendered image",
    }
}

fn artifact_type_slug(artifact_type: CaseArtifactType) -> &'static str {
    match artifact_type {
        CaseArtifactType::Annotation => "annotation",
        CaseArtifactType::DerivedTable => "derived-table",
        CaseArtifactType::Overlay => "overlay",
        CaseArtifactType::RenderedImage => "rendered-image",
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(TABLE[((value >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((value >> 12) & 0x3f) as usize] as char);
        output.push(if chunk.len() >= 2 {
            TABLE[((value >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() == 3 {
            TABLE[(value & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn decode_base64(value: &str) -> Result<Vec<u8>, String> {
    if value.is_empty() || !value.len().is_multiple_of(4) {
        return Err("Verified image payload contains malformed base64.".into());
    }
    let mut output = Vec::with_capacity(value.len() / 4 * 3);
    for (chunk_index, chunk) in value.as_bytes().chunks(4).enumerate() {
        let last = chunk_index + 1 == value.len() / 4;
        let a = base64_value(chunk[0]).ok_or_else(|| "Malformed image base64.".to_owned())?;
        let b = base64_value(chunk[1]).ok_or_else(|| "Malformed image base64.".to_owned())?;
        let c = if chunk[2] == b'=' {
            if !last || chunk[3] != b'=' {
                return Err("Malformed image base64 padding.".into());
            }
            0
        } else {
            base64_value(chunk[2]).ok_or_else(|| "Malformed image base64.".to_owned())?
        };
        let d = if chunk[3] == b'=' {
            if !last {
                return Err("Malformed image base64 padding.".into());
            }
            0
        } else {
            base64_value(chunk[3]).ok_or_else(|| "Malformed image base64.".to_owned())?
        };
        let bits = (u32::from(a) << 18) | (u32::from(b) << 12) | (u32::from(c) << 6) | u32::from(d);
        output.push((bits >> 16) as u8);
        if chunk[2] != b'=' {
            output.push((bits >> 8) as u8);
        }
        if chunk[3] != b'=' {
            output.push(bits as u8);
        }
    }
    Ok(output)
}

fn base64_value(value: u8) -> Option<u8> {
    match value {
        b'A'..=b'Z' => Some(value - b'A'),
        b'a'..=b'z' => Some(value - b'a' + 26),
        b'0'..=b'9' => Some(value - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_case() -> CaseDraft {
        CaseDraft {
            case_id: "case-20260812".into(),
            title: "Central Plains analysis".into(),
            event_start: "2026-08-12 00:00".into(),
            event_end: "2026-08-12 06:00".into(),
            retention_days: 30,
            model: "hrrr".into(),
            run: "20260812_00z".into(),
            snapshot_id: "1".repeat(64),
            grid_hash: "2".repeat(64),
            variables_csv: "reflectivity, u_10m".into(),
            provider: "noaa-aws-public-data".into(),
            roles_csv: "surface".into(),
            products_csv: "wrfsfc".into(),
            data_origin: DataOrigin::UserProvided,
            attribution: AttributionDraft::noaa(),
            modification_notice: "Rendered by BowEcho.".into(),
            explicit_owner_publication: true,
            redistribution_rights_confirmed: true,
            final_confirmation: true,
        }
    }

    fn annotation() -> ArtifactDraft {
        ArtifactDraft {
            annotation_text: "Mesocyclone intensified near the warm front.".into(),
            ..ArtifactDraft::default()
        }
    }

    #[test]
    fn artifact_publication_is_owner_bound_and_canonical() {
        let owner = "a".repeat(64);
        let publication = build_artifact_publication(&valid_case(), &annotation(), &owner).unwrap();
        assert_eq!(publication.owner_principal_sha256, owner);
        assert_eq!(
            publication
                .request
                .recipe
                .parameters
                .get(PUBLICATION_OWNER_PARAMETER),
            Some(&"a".repeat(64))
        );
        assert_eq!(
            publication.request.variables,
            vec!["reflectivity".to_owned(), "u_10m".to_owned()]
        );
        publication.validate(&ProtocolLimits::default()).unwrap();
    }

    #[test]
    fn client_artifact_cannot_claim_public_provider_even_for_hrrr() {
        let owner = "a".repeat(64);
        let mut case = valid_case();
        case.data_origin = DataOrigin::PublicProvider;
        assert!(build_artifact_publication(&case, &annotation(), &owner).is_err());
    }

    #[test]
    fn private_wrf_and_arwen_require_exact_owner_rights_boundary() {
        let owner = "b".repeat(64);
        let mut case = valid_case();
        case.model = "owner-wrf".into();
        case.data_origin = DataOrigin::PrivateWrf;
        case.explicit_owner_publication = false;
        assert!(build_artifact_publication(&case, &annotation(), &owner).is_err());
        case.explicit_owner_publication = true;
        case.redistribution_rights_confirmed = false;
        assert!(build_artifact_publication(&case, &annotation(), &owner).is_err());
        case.redistribution_rights_confirmed = true;
        assert!(build_artifact_publication(&case, &annotation(), &owner).is_ok());

        case.data_origin = DataOrigin::PublicProvider;
        assert!(
            build_artifact_publication(&case, &annotation(), &owner).is_err(),
            "a WRF source may not be relabeled public"
        );
        case.model = "owner-arwen".into();
        case.data_origin = DataOrigin::PrivateArwen;
        assert!(build_artifact_publication(&case, &annotation(), &owner).is_ok());
    }

    #[test]
    fn closed_payload_parsers_enforce_client_bounds_and_no_path_text() {
        let table = parse_table("Values", "name\tvalue\nCAPE\t1234.5\nflag\ttrue").unwrap();
        assert_eq!(table.columns.len(), 2);
        assert!(matches!(table.rows[0][1], TableCell::FixedDecimal { .. }));
        assert!(parse_table("Values", "a\tb\nonly-one").is_err());

        let overlay = parse_overlay(
            "Track",
            OverlayEditorKind::Polyline,
            "-97.0,35.0\n-96.5,35.5",
        )
        .unwrap();
        assert_eq!(overlay.features.len(), 1);
        assert!(
            parse_overlay(
                "Polygon",
                OverlayEditorKind::Polygon,
                "-97,35\n-96,35\n-96,36\n-97,36"
            )
            .is_err()
        );

        let mut unsafe_annotation = annotation();
        unsafe_annotation.annotation_text = "C:\\private\\wrfout".into();
        assert!(
            build_artifact_publication(&valid_case(), &unsafe_annotation, &"c".repeat(64)).is_err()
        );
    }

    #[test]
    fn image_identity_and_base64_round_trip_are_bounded() {
        let mut png = vec![0_u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[12..16].copy_from_slice(b"IHDR");
        png[16..20].copy_from_slice(&640_u32.to_be_bytes());
        png[20..24].copy_from_slice(&480_u32.to_be_bytes());
        assert_eq!(
            image_identity(&png),
            Some((RenderedImageFormat::Png, 640, 480))
        );
        let encoded = encode_base64(&png);
        assert_eq!(decode_base64(&encoded).unwrap(), png);
        assert!(decode_base64("not-base64").is_err());
    }
}
