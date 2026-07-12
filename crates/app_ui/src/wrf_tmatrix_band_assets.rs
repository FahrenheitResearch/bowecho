//! Local, fail-closed discovery and loading for external research T-matrix
//! frequency packs.
//!
//! A pack is selected only at one declared frequency. S-, C-, and X-band
//! tables are never substituted for one another and frequency interpolation is
//! deliberately unsupported. This module performs storage/provenance gates;
//! the scattering operator remains responsible for validating the scientific
//! contract of the selected five-table bundle.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use radar_scattering::{
    AxisKind, ResearchTMatrixLut, Sha256Digest, SpheroidConvention, TMatrixPopulationRole,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::wrf_tmatrix_scene::WrfTMatrixLutBundle;

pub const EXTERNAL_TMATRIX_PACK_SCHEMA: u32 = 1;
pub const S_BAND_RESEARCH_FREQUENCY_HZ: f64 = 2_800_000_000.0;
pub const C_BAND_RESEARCH_FREQUENCY_HZ: f64 = 5_600_000_000.0;
pub const X_BAND_RESEARCH_FREQUENCY_HZ: f64 = 9_400_000_000.0;
pub const LOCAL_PACK_MANIFEST_FILE: &str = "pack.json";
pub const LEGACY_EMBEDDED_S_RESEARCH_V1_ID: &str = "legacy-embedded-s-research-v1";

const MAX_PACK_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TMatrixResearchBand {
    S,
    C,
    X,
}

impl TMatrixResearchBand {
    #[must_use]
    pub const fn exact_frequency_hz(self) -> f64 {
        match self {
            Self::S => S_BAND_RESEARCH_FREQUENCY_HZ,
            Self::C => C_BAND_RESEARCH_FREQUENCY_HZ,
            Self::X => X_BAND_RESEARCH_FREQUENCY_HZ,
        }
    }

    pub fn from_exact_frequency_hz(frequency_hz: f64) -> Result<Self, TMatrixBandPackError> {
        [Self::S, Self::C, Self::X]
            .into_iter()
            .find(|band| band.exact_frequency_hz().to_bits() == frequency_hz.to_bits())
            .ok_or(TMatrixBandPackError::UnsupportedExactFrequency { frequency_hz })
    }
}

impl std::fmt::Display for TMatrixResearchBand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::S => "S",
            Self::C => "C",
            Self::X => "X",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TMatrixBandPackRole {
    DryOblate,
    DryProlate,
    WetOblate,
    WetProlate,
    RainStandaloneAndResidual,
}

impl TMatrixBandPackRole {
    pub const ALL: [Self; 5] = [
        Self::DryOblate,
        Self::DryProlate,
        Self::WetOblate,
        Self::WetProlate,
        Self::RainStandaloneAndResidual,
    ];
}

impl std::fmt::Display for TMatrixBandPackRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::DryOblate => "dry oblate",
            Self::DryProlate => "dry prolate",
            Self::WetOblate => "wet oblate",
            Self::WetProlate => "wet prolate",
            Self::RainStandaloneAndResidual => "standalone/residual rain",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TMatrixBandPackValidationStatus {
    ValidatedResearch,
    UnvalidatedResearch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TMatrixBandRoleFile {
    pub role: TMatrixBandPackRole,
    pub lut_path: String,
    pub lut_sha256: Sha256Digest,
    pub lut_bytes: u64,
    pub config_path: String,
    pub config_sha256: Sha256Digest,
    pub config_bytes: u64,
}

/// Strict schema-v1 manifest. The three provenance hashes identify the exact
/// generator source, solver build, and ODF specification used to produce the
/// role files; a changed value therefore produces a different pack identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TMatrixBandPackManifest {
    pub pack_schema: u32,
    pub pack_id: String,
    pub band: TMatrixResearchBand,
    pub frequency_hz: f64,
    pub science_revision: String,
    pub validation_status: TMatrixBandPackValidationStatus,
    pub generator_sha256: Sha256Digest,
    pub solver_sha256: Sha256Digest,
    pub odf_sha256: Sha256Digest,
    pub role_files: Vec<TMatrixBandRoleFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalTMatrixBandPackIdentity {
    pack_id: String,
    band: TMatrixResearchBand,
    frequency_bits: u64,
    pack_schema: u32,
    science_revision: String,
    generator_sha256: Sha256Digest,
    solver_sha256: Sha256Digest,
    odf_sha256: Sha256Digest,
    manifest_sha256: Sha256Digest,
}

impl ExternalTMatrixBandPackIdentity {
    #[must_use]
    pub fn pack_id(&self) -> &str {
        &self.pack_id
    }

    #[must_use]
    pub const fn band(&self) -> TMatrixResearchBand {
        self.band
    }

    #[must_use]
    pub fn exact_frequency_hz(&self) -> f64 {
        f64::from_bits(self.frequency_bits)
    }

    #[must_use]
    pub const fn pack_schema(&self) -> u32 {
        self.pack_schema
    }

    #[must_use]
    pub fn science_revision(&self) -> &str {
        &self.science_revision
    }

    #[must_use]
    pub const fn generator_sha256(&self) -> Sha256Digest {
        self.generator_sha256
    }

    #[must_use]
    pub const fn solver_sha256(&self) -> Sha256Digest {
        self.solver_sha256
    }

    #[must_use]
    pub const fn odf_sha256(&self) -> Sha256Digest {
        self.odf_sha256
    }

    #[must_use]
    pub const fn manifest_sha256(&self) -> Sha256Digest {
        self.manifest_sha256
    }
}

/// Legacy embedded S v1 is intentionally a distinct source. It is not an
/// external validated S pack and cannot satisfy an external-pack lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TMatrixBandPackIdentity {
    ExternalValidated(ExternalTMatrixBandPackIdentity),
    LegacyEmbeddedSResearchV1,
}

impl TMatrixBandPackIdentity {
    #[must_use]
    pub fn pack_id(&self) -> &str {
        match self {
            Self::ExternalValidated(identity) => identity.pack_id(),
            Self::LegacyEmbeddedSResearchV1 => LEGACY_EMBEDDED_S_RESEARCH_V1_ID,
        }
    }

    #[must_use]
    pub const fn band(&self) -> TMatrixResearchBand {
        match self {
            Self::ExternalValidated(identity) => identity.band(),
            Self::LegacyEmbeddedSResearchV1 => TMatrixResearchBand::S,
        }
    }

    #[must_use]
    pub fn exact_frequency_hz(&self) -> f64 {
        match self {
            Self::ExternalValidated(identity) => identity.exact_frequency_hz(),
            Self::LegacyEmbeddedSResearchV1 => S_BAND_RESEARCH_FREQUENCY_HZ,
        }
    }

    #[must_use]
    pub const fn is_external_validated(&self) -> bool {
        matches!(self, Self::ExternalValidated(_))
    }
}

impl From<ExternalTMatrixBandPackIdentity> for TMatrixBandPackIdentity {
    fn from(identity: ExternalTMatrixBandPackIdentity) -> Self {
        Self::ExternalValidated(identity)
    }
}

#[must_use]
pub const fn legacy_embedded_s_research_v1_identity() -> TMatrixBandPackIdentity {
    TMatrixBandPackIdentity::LegacyEmbeddedSResearchV1
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredTMatrixBandPack {
    pub identity: ExternalTMatrixBandPackIdentity,
    pub manifest: TMatrixBandPackManifest,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TMatrixBandPackDiscovery {
    Available(Box<DiscoveredTMatrixBandPack>),
    Invalid {
        provider_pack_id: String,
        error: String,
    },
}

/// Storage seam used by both the local directory implementation and pure test
/// providers. No implementation in this module performs network I/O.
pub trait TMatrixBandPackProvider: Send + Sync {
    fn list_pack_ids(&self) -> Result<Vec<String>, String>;
    fn read_manifest(&self, pack_id: &str) -> Result<Vec<u8>, String>;
    fn read_pack_file(&self, pack_id: &str, relative_path: &str) -> Result<Vec<u8>, String>;
}

#[derive(Clone, Debug)]
pub struct LocalDirectoryTMatrixBandPackProvider {
    root: PathBuf,
}

impl LocalDirectoryTMatrixBandPackProvider {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolved_pack_file(&self, pack_id: &str, relative_path: &str) -> Result<PathBuf, String> {
        validate_pack_id(pack_id).map_err(|error| error.to_string())?;
        validate_relative_path(relative_path).map_err(|error| error.to_string())?;
        let root = fs::canonicalize(&self.root)
            .map_err(|error| format!("canonicalize pack root {}: {error}", self.root.display()))?;
        let pack_root = fs::canonicalize(root.join(pack_id))
            .map_err(|error| format!("canonicalize pack {pack_id}: {error}"))?;
        if !pack_root.starts_with(&root) {
            return Err(format!(
                "pack {pack_id} resolves outside {}",
                root.display()
            ));
        }
        let resolved = fs::canonicalize(pack_root.join(relative_path))
            .map_err(|error| format!("canonicalize {pack_id}/{relative_path}: {error}"))?;
        if !resolved.starts_with(&pack_root) {
            return Err(format!(
                "pack file {pack_id}/{relative_path} resolves outside its pack directory"
            ));
        }
        let metadata = fs::metadata(&resolved)
            .map_err(|error| format!("inspect {}: {error}", resolved.display()))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a regular file", resolved.display()));
        }
        Ok(resolved)
    }
}

impl TMatrixBandPackProvider for LocalDirectoryTMatrixBandPackProvider {
    fn list_pack_ids(&self) -> Result<Vec<String>, String> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.root)
            .map_err(|error| format!("read pack root {}: {error}", self.root.display()))?
        {
            let entry = entry.map_err(|error| format!("read pack directory entry: {error}"))?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?;
            if !file_type.is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_pack_id(&id).is_ok() {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    fn read_manifest(&self, pack_id: &str) -> Result<Vec<u8>, String> {
        let path = self.resolved_pack_file(pack_id, LOCAL_PACK_MANIFEST_FILE)?;
        fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))
    }

    fn read_pack_file(&self, pack_id: &str, relative_path: &str) -> Result<Vec<u8>, String> {
        let path = self.resolved_pack_file(pack_id, relative_path)?;
        fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))
    }
}

pub fn discover_tmatrix_band_packs<P: TMatrixBandPackProvider + ?Sized>(
    provider: &P,
) -> Result<Vec<TMatrixBandPackDiscovery>, TMatrixBandPackError> {
    let mut pack_ids =
        provider
            .list_pack_ids()
            .map_err(|detail| TMatrixBandPackError::Provider {
                operation: "list local pack identities",
                detail,
            })?;
    pack_ids.sort();
    pack_ids.dedup();
    Ok(pack_ids
        .into_iter()
        .map(|provider_pack_id| {
            let result = provider
                .read_manifest(&provider_pack_id)
                .map_err(|detail| TMatrixBandPackError::Provider {
                    operation: "read local pack manifest",
                    detail,
                })
                .and_then(|bytes| validate_manifest(&provider_pack_id, &bytes));
            match result {
                Ok(pack) => TMatrixBandPackDiscovery::Available(Box::new(pack)),
                Err(error) => TMatrixBandPackDiscovery::Invalid {
                    provider_pack_id,
                    error: error.to_string(),
                },
            }
        })
        .collect())
}

pub fn select_exact_tmatrix_band_pack(
    discoveries: &[TMatrixBandPackDiscovery],
    frequency_hz: f64,
) -> Result<&DiscoveredTMatrixBandPack, TMatrixBandPackError> {
    let band = TMatrixResearchBand::from_exact_frequency_hz(frequency_hz)?;
    let matches: Vec<_> = discoveries
        .iter()
        .filter_map(|entry| match entry {
            TMatrixBandPackDiscovery::Available(pack)
                if pack.identity.frequency_bits == frequency_hz.to_bits() =>
            {
                Some(pack.as_ref())
            }
            _ => None,
        })
        .collect();
    match matches.as_slice() {
        [] => Err(TMatrixBandPackError::PackUnavailable { band, frequency_hz }),
        [pack] => Ok(pack),
        _ => Err(TMatrixBandPackError::AmbiguousExactPack {
            band,
            frequency_hz,
            count: matches.len(),
        }),
    }
}

#[derive(Debug)]
struct ValidatedRoleBytes {
    lut: Vec<u8>,
    config: Vec<u8>,
}

#[derive(Debug)]
struct ValidatedPackBytes {
    roles: BTreeMap<TMatrixBandPackRole, ValidatedRoleBytes>,
    retained_bytes: usize,
}

fn read_validated_pack_bytes<P: TMatrixBandPackProvider + ?Sized>(
    provider: &P,
    selected: &DiscoveredTMatrixBandPack,
) -> Result<ValidatedPackBytes, TMatrixBandPackError> {
    let manifest_bytes = provider
        .read_manifest(selected.identity.pack_id())
        .map_err(|detail| TMatrixBandPackError::Provider {
            operation: "re-read selected pack manifest",
            detail,
        })?;
    let actual_manifest_sha256 = Sha256Digest::compute(&manifest_bytes);
    if actual_manifest_sha256 != selected.identity.manifest_sha256() {
        return Err(TMatrixBandPackError::ManifestChanged {
            expected: selected.identity.manifest_sha256(),
            actual: actual_manifest_sha256,
        });
    }
    let revalidated = validate_manifest(selected.identity.pack_id(), &manifest_bytes)?;
    if revalidated.identity != selected.identity {
        return Err(TMatrixBandPackError::ManifestIdentityChanged);
    }

    let mut retained_bytes = 0_usize;
    let mut roles = BTreeMap::new();
    for file in &revalidated.manifest.role_files {
        let lut = read_and_validate_file(
            provider,
            selected.identity.pack_id(),
            file.role,
            "LUT",
            &file.lut_path,
            file.lut_bytes,
            file.lut_sha256,
        )?;
        let config = read_and_validate_file(
            provider,
            selected.identity.pack_id(),
            file.role,
            "config",
            &file.config_path,
            file.config_bytes,
            file.config_sha256,
        )?;
        retained_bytes = retained_bytes
            .checked_add(lut.len())
            .and_then(|value| value.checked_add(config.len()))
            .ok_or(TMatrixBandPackError::PackByteCountOverflow)?;
        roles.insert(file.role, ValidatedRoleBytes { lut, config });
    }
    Ok(ValidatedPackBytes {
        roles,
        retained_bytes,
    })
}

fn read_and_validate_file<P: TMatrixBandPackProvider + ?Sized>(
    provider: &P,
    pack_id: &str,
    role: TMatrixBandPackRole,
    kind: &'static str,
    path: &str,
    expected_bytes: u64,
    expected_sha256: Sha256Digest,
) -> Result<Vec<u8>, TMatrixBandPackError> {
    let bytes = provider.read_pack_file(pack_id, path).map_err(|detail| {
        TMatrixBandPackError::Provider {
            operation: "read selected pack role file",
            detail,
        }
    })?;
    if bytes.len() as u64 != expected_bytes {
        return Err(TMatrixBandPackError::FileSizeMismatch {
            role,
            kind,
            path: path.to_owned(),
            expected: expected_bytes,
            actual: bytes.len() as u64,
        });
    }
    let actual_sha256 = Sha256Digest::compute(&bytes);
    if actual_sha256 != expected_sha256 {
        return Err(TMatrixBandPackError::FileSha256Mismatch {
            role,
            kind,
            path: path.to_owned(),
            expected: expected_sha256,
            actual: actual_sha256,
        });
    }
    Ok(bytes)
}

/// Owned five-table external bundle. Calling [`Self::borrowed_bundle`] does
/// not run the current operator's band-specific scene contract; integration
/// must validate that contract before evaluation.
pub struct OwnedTMatrixBandBundle {
    identity: ExternalTMatrixBandPackIdentity,
    dry_oblate: ResearchTMatrixLut,
    dry_prolate: ResearchTMatrixLut,
    wet_oblate: ResearchTMatrixLut,
    wet_prolate: ResearchTMatrixLut,
    rain_standalone_and_residual: ResearchTMatrixLut,
    retained_source_bytes: usize,
}

impl OwnedTMatrixBandBundle {
    #[must_use]
    pub const fn identity(&self) -> &ExternalTMatrixBandPackIdentity {
        &self.identity
    }

    #[must_use]
    pub const fn retained_source_bytes(&self) -> usize {
        self.retained_source_bytes
    }

    #[must_use]
    pub fn borrowed_bundle(&self) -> WrfTMatrixLutBundle<'_> {
        WrfTMatrixLutBundle::new(
            &self.dry_oblate,
            &self.dry_prolate,
            &self.wet_oblate,
            &self.wet_prolate,
            &self.rain_standalone_and_residual,
        )
    }
}

pub fn load_external_tmatrix_band_pack<P: TMatrixBandPackProvider + ?Sized>(
    provider: &P,
    selected: &DiscoveredTMatrixBandPack,
) -> Result<OwnedTMatrixBandBundle, TMatrixBandPackError> {
    let mut bytes = read_validated_pack_bytes(provider, selected)?;
    let dry_oblate = load_role(&mut bytes.roles, TMatrixBandPackRole::DryOblate, selected)?;
    let dry_prolate = load_role(&mut bytes.roles, TMatrixBandPackRole::DryProlate, selected)?;
    let wet_oblate = load_role(&mut bytes.roles, TMatrixBandPackRole::WetOblate, selected)?;
    let wet_prolate = load_role(&mut bytes.roles, TMatrixBandPackRole::WetProlate, selected)?;
    let rain_standalone_and_residual = load_role(
        &mut bytes.roles,
        TMatrixBandPackRole::RainStandaloneAndResidual,
        selected,
    )?;
    Ok(OwnedTMatrixBandBundle {
        identity: selected.identity.clone(),
        dry_oblate,
        dry_prolate,
        wet_oblate,
        wet_prolate,
        rain_standalone_and_residual,
        retained_source_bytes: bytes.retained_bytes,
    })
}

fn load_role(
    roles: &mut BTreeMap<TMatrixBandPackRole, ValidatedRoleBytes>,
    role: TMatrixBandPackRole,
    selected: &DiscoveredTMatrixBandPack,
) -> Result<ResearchTMatrixLut, TMatrixBandPackError> {
    let bytes = roles
        .remove(&role)
        .ok_or(TMatrixBandPackError::MissingRole { role })?;
    let file = selected
        .manifest
        .role_files
        .iter()
        .find(|file| file.role == role)
        .ok_or(TMatrixBandPackError::MissingRole { role })?;
    let table =
        ResearchTMatrixLut::load(&bytes.lut, file.lut_sha256, &bytes.config).map_err(|error| {
            TMatrixBandPackError::TableLoad {
                role,
                detail: error.to_string(),
            }
        })?;
    validate_loaded_role(&table, role, selected.identity.exact_frequency_hz())?;
    Ok(table)
}

fn validate_loaded_role(
    table: &ResearchTMatrixLut,
    role: TMatrixBandPackRole,
    exact_frequency_hz: f64,
) -> Result<(), TMatrixBandPackError> {
    let frequency_axis = table
        .offline_lut()
        .header()
        .axes()
        .iter()
        .find(|axis| axis.kind() == AxisKind::Frequency)
        .ok_or(TMatrixBandPackError::TableMissingFrequencyAxis { role })?;
    if frequency_axis.coordinates().len() != 1
        || frequency_axis.coordinates()[0].to_bits() != exact_frequency_hz.to_bits()
    {
        return Err(TMatrixBandPackError::TableFrequencyMismatch {
            role,
            expected: exact_frequency_hz,
            actual: frequency_axis.coordinates().to_vec(),
        });
    }
    let descriptor = table.descriptor();
    let role_matches = match role {
        TMatrixBandPackRole::DryOblate => {
            descriptor.population_role()
                == TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle
                && descriptor.spheroid() == SpheroidConvention::OblateMinorVertical
        }
        TMatrixBandPackRole::DryProlate => {
            descriptor.population_role()
                == TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle
                && descriptor.spheroid() == SpheroidConvention::ProlateMajorVertical
        }
        TMatrixBandPackRole::WetOblate => {
            descriptor.population_role()
                == TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle
                && descriptor.spheroid() == SpheroidConvention::OblateMinorVertical
        }
        TMatrixBandPackRole::WetProlate => {
            descriptor.population_role()
                == TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle
                && descriptor.spheroid() == SpheroidConvention::ProlateMajorVertical
        }
        TMatrixBandPackRole::RainStandaloneAndResidual => {
            descriptor.population_role()
                == TMatrixPopulationRole::ConventionalRainStandaloneAndResidual
        }
    };
    if !role_matches {
        return Err(TMatrixBandPackError::TableRoleMismatch {
            role,
            table_id: descriptor.table_id().to_owned(),
        });
    }
    Ok(())
}

struct OneSelectedPackCache<T> {
    slot: Mutex<Option<(Sha256Digest, Arc<T>)>>,
}

impl<T> Default for OneSelectedPackCache<T> {
    fn default() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }
}

impl<T> OneSelectedPackCache<T> {
    fn load<F, E>(&self, key: Sha256Digest, loader: F) -> Result<Arc<T>, E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        let mut slot = self.slot.lock().expect("one-pack cache mutex poisoned");
        if let Some((cached_key, value)) = slot.as_ref()
            && *cached_key == key
        {
            return Ok(Arc::clone(value));
        }
        let loaded = Arc::new(loader()?);
        *slot = Some((key, Arc::clone(&loaded)));
        Ok(loaded)
    }

    fn clear(&self) {
        *self.slot.lock().expect("one-pack cache mutex poisoned") = None;
    }
}

/// Bounded cache that retains no more than one successfully loaded external
/// pack. A failed replacement leaves the previous pack intact, but exact
/// selection is always performed first, so it can never satisfy another band.
#[derive(Default)]
pub struct TMatrixBandPackCache {
    loaded: OneSelectedPackCache<OwnedTMatrixBandBundle>,
}

impl TMatrixBandPackCache {
    pub fn load_exact_frequency<P: TMatrixBandPackProvider + ?Sized>(
        &self,
        provider: &P,
        discoveries: &[TMatrixBandPackDiscovery],
        frequency_hz: f64,
    ) -> Result<Arc<OwnedTMatrixBandBundle>, TMatrixBandPackError> {
        let selected = select_exact_tmatrix_band_pack(discoveries, frequency_hz)?;
        self.loaded.load(selected.identity.manifest_sha256(), || {
            load_external_tmatrix_band_pack(provider, selected)
        })
    }

    pub fn clear(&self) {
        self.loaded.clear();
    }
}

fn validate_manifest(
    provider_pack_id: &str,
    bytes: &[u8],
) -> Result<DiscoveredTMatrixBandPack, TMatrixBandPackError> {
    validate_pack_id(provider_pack_id)?;
    let manifest: TMatrixBandPackManifest = serde_json::from_slice(bytes)
        .map_err(|error| TMatrixBandPackError::ManifestJson(error.to_string()))?;
    validate_pack_id(&manifest.pack_id)?;
    if manifest.pack_id != provider_pack_id {
        return Err(TMatrixBandPackError::PackIdMismatch {
            provider: provider_pack_id.to_owned(),
            manifest: manifest.pack_id,
        });
    }
    if manifest.pack_schema != EXTERNAL_TMATRIX_PACK_SCHEMA {
        return Err(TMatrixBandPackError::UnsupportedPackSchema {
            expected: EXTERNAL_TMATRIX_PACK_SCHEMA,
            actual: manifest.pack_schema,
        });
    }
    if manifest.validation_status != TMatrixBandPackValidationStatus::ValidatedResearch {
        return Err(TMatrixBandPackError::PackNotValidated);
    }
    if manifest.frequency_hz.to_bits() != manifest.band.exact_frequency_hz().to_bits() {
        return Err(TMatrixBandPackError::BandFrequencyMismatch {
            band: manifest.band,
            expected: manifest.band.exact_frequency_hz(),
            actual: manifest.frequency_hz,
        });
    }
    let revision = manifest.science_revision.trim();
    if revision.is_empty() || revision.len() > 128 || revision != manifest.science_revision {
        return Err(TMatrixBandPackError::InvalidScienceRevision);
    }
    for (name, digest) in [
        ("generator", manifest.generator_sha256),
        ("solver", manifest.solver_sha256),
        ("ODF", manifest.odf_sha256),
    ] {
        if digest.as_bytes().iter().all(|byte| *byte == 0) {
            return Err(TMatrixBandPackError::ZeroProvenanceDigest { name });
        }
    }

    let mut roles = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for file in &manifest.role_files {
        if !roles.insert(file.role) {
            return Err(TMatrixBandPackError::DuplicateRole { role: file.role });
        }
        validate_declared_file(file.role, "LUT", &file.lut_path, file.lut_bytes)?;
        validate_declared_file(file.role, "config", &file.config_path, file.config_bytes)?;
        for path in [&file.lut_path, &file.config_path] {
            if !paths.insert(path.as_str()) {
                return Err(TMatrixBandPackError::DuplicateFilePath {
                    path: path.to_string(),
                });
            }
        }
    }
    for role in TMatrixBandPackRole::ALL {
        if !roles.contains(&role) {
            return Err(TMatrixBandPackError::MissingRole { role });
        }
    }
    if roles.len() != TMatrixBandPackRole::ALL.len() {
        return Err(TMatrixBandPackError::WrongRoleCount {
            actual: roles.len(),
        });
    }

    let identity = ExternalTMatrixBandPackIdentity {
        pack_id: manifest.pack_id.clone(),
        band: manifest.band,
        frequency_bits: manifest.frequency_hz.to_bits(),
        pack_schema: manifest.pack_schema,
        science_revision: manifest.science_revision.clone(),
        generator_sha256: manifest.generator_sha256,
        solver_sha256: manifest.solver_sha256,
        odf_sha256: manifest.odf_sha256,
        manifest_sha256: Sha256Digest::compute(bytes),
    };
    Ok(DiscoveredTMatrixBandPack { identity, manifest })
}

fn validate_declared_file(
    role: TMatrixBandPackRole,
    kind: &'static str,
    path: &str,
    bytes: u64,
) -> Result<(), TMatrixBandPackError> {
    validate_relative_path(path)?;
    if bytes == 0 || bytes > MAX_PACK_FILE_BYTES {
        return Err(TMatrixBandPackError::InvalidDeclaredFileSize { role, kind, bytes });
    }
    Ok(())
}

fn validate_pack_id(pack_id: &str) -> Result<(), TMatrixBandPackError> {
    if pack_id.is_empty()
        || pack_id.len() > 128
        || !pack_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(TMatrixBandPackError::InvalidPackId(pack_id.to_owned()));
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), TMatrixBandPackError> {
    if path.is_empty()
        || path.contains('\\')
        || path.contains(':')
        || path.starts_with('/')
        || path.ends_with('/')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(TMatrixBandPackError::UnsafeRelativePath(path.to_owned()));
    }
    let parsed = Path::new(path);
    if parsed.is_absolute()
        || parsed
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(TMatrixBandPackError::UnsafeRelativePath(path.to_owned()));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum TMatrixBandPackError {
    #[error("pack provider failed to {operation}: {detail}")]
    Provider {
        operation: &'static str,
        detail: String,
    },
    #[error("invalid pack identity {0:?}")]
    InvalidPackId(String),
    #[error("pack manifest JSON is invalid: {0}")]
    ManifestJson(String),
    #[error("provider pack identity {provider:?} differs from manifest identity {manifest:?}")]
    PackIdMismatch { provider: String, manifest: String },
    #[error("external pack schema must be {expected}, got {actual}")]
    UnsupportedPackSchema { expected: u32, actual: u32 },
    #[error("external pack is not explicitly marked validated_research")]
    PackNotValidated,
    #[error("{band}-band pack must be exactly {expected} Hz, got {actual} Hz")]
    BandFrequencyMismatch {
        band: TMatrixResearchBand,
        expected: f64,
        actual: f64,
    },
    #[error("unsupported exact T-matrix frequency {frequency_hz} Hz; no interpolation is allowed")]
    UnsupportedExactFrequency { frequency_hz: f64 },
    #[error("science revision must be non-empty, trimmed text of at most 128 bytes")]
    InvalidScienceRevision,
    #[error("{name} provenance SHA-256 cannot be the zero digest")]
    ZeroProvenanceDigest { name: &'static str },
    #[error("pack is missing required role {role}")]
    MissingRole { role: TMatrixBandPackRole },
    #[error("pack declares role {role} more than once")]
    DuplicateRole { role: TMatrixBandPackRole },
    #[error("pack must contain exactly five roles, got {actual}")]
    WrongRoleCount { actual: usize },
    #[error("pack reuses role-file path {path:?}")]
    DuplicateFilePath { path: String },
    #[error("unsafe pack-relative path {0:?}")]
    UnsafeRelativePath(String),
    #[error("{role} {kind} declares invalid size {bytes} bytes")]
    InvalidDeclaredFileSize {
        role: TMatrixBandPackRole,
        kind: &'static str,
        bytes: u64,
    },
    #[error("no validated local {band}-band pack exists at exactly {frequency_hz} Hz")]
    PackUnavailable {
        band: TMatrixResearchBand,
        frequency_hz: f64,
    },
    #[error(
        "{count} validated {band}-band packs exist at exactly {frequency_hz} Hz; selection must be explicit"
    )]
    AmbiguousExactPack {
        band: TMatrixResearchBand,
        frequency_hz: f64,
        count: usize,
    },
    #[error("selected pack manifest changed after discovery: expected {expected}, got {actual}")]
    ManifestChanged {
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    #[error("selected pack manifest identity changed after discovery")]
    ManifestIdentityChanged,
    #[error("{role} {kind} {path:?} must be {expected} bytes, got {actual}")]
    FileSizeMismatch {
        role: TMatrixBandPackRole,
        kind: &'static str,
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("{role} {kind} {path:?} SHA-256 mismatch: expected {expected}, got {actual}")]
    FileSha256Mismatch {
        role: TMatrixBandPackRole,
        kind: &'static str,
        path: String,
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    #[error("pack source byte count overflow")]
    PackByteCountOverflow,
    #[error("load {role} research table: {detail}")]
    TableLoad {
        role: TMatrixBandPackRole,
        detail: String,
    },
    #[error("loaded {role} table has no frequency axis")]
    TableMissingFrequencyAxis { role: TMatrixBandPackRole },
    #[error("loaded {role} table frequency must be exactly {expected} Hz, got {actual:?}")]
    TableFrequencyMismatch {
        role: TMatrixBandPackRole,
        expected: f64,
        actual: Vec<f64>,
    },
    #[error("loaded table {table_id:?} does not satisfy declared role {role}")]
    TableRoleMismatch {
        role: TMatrixBandPackRole,
        table_id: String,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct FakeProvider {
        manifests: BTreeMap<String, Vec<u8>>,
        files: BTreeMap<(String, String), Vec<u8>>,
    }

    impl TMatrixBandPackProvider for FakeProvider {
        fn list_pack_ids(&self) -> Result<Vec<String>, String> {
            Ok(self.manifests.keys().cloned().collect())
        }

        fn read_manifest(&self, pack_id: &str) -> Result<Vec<u8>, String> {
            self.manifests
                .get(pack_id)
                .cloned()
                .ok_or_else(|| format!("missing manifest {pack_id}"))
        }

        fn read_pack_file(&self, pack_id: &str, relative_path: &str) -> Result<Vec<u8>, String> {
            self.files
                .get(&(pack_id.to_owned(), relative_path.to_owned()))
                .cloned()
                .ok_or_else(|| format!("missing file {pack_id}/{relative_path}"))
        }
    }

    fn digest(label: &str) -> Sha256Digest {
        Sha256Digest::compute(label.as_bytes())
    }

    fn fake_manifest(pack_id: &str, band: TMatrixResearchBand) -> TMatrixBandPackManifest {
        TMatrixBandPackManifest {
            pack_schema: EXTERNAL_TMATRIX_PACK_SCHEMA,
            pack_id: pack_id.to_owned(),
            band,
            frequency_hz: band.exact_frequency_hz(),
            science_revision: "fixture-revision-1".to_owned(),
            validation_status: TMatrixBandPackValidationStatus::ValidatedResearch,
            generator_sha256: digest("generator"),
            solver_sha256: digest("solver"),
            odf_sha256: digest("odf"),
            role_files: TMatrixBandPackRole::ALL
                .into_iter()
                .map(|role| {
                    let stem = format!("{role:?}").to_ascii_lowercase();
                    let lut = format!("lut-{stem}").into_bytes();
                    let config = format!("config-{stem}").into_bytes();
                    TMatrixBandRoleFile {
                        role,
                        lut_path: format!("{stem}.lut"),
                        lut_sha256: Sha256Digest::compute(&lut),
                        lut_bytes: lut.len() as u64,
                        config_path: format!("{stem}.json"),
                        config_sha256: Sha256Digest::compute(&config),
                        config_bytes: config.len() as u64,
                    }
                })
                .collect(),
        }
    }

    fn provider_with_manifest(manifest: TMatrixBandPackManifest) -> FakeProvider {
        let mut provider = FakeProvider::default();
        for role in &manifest.role_files {
            let stem = format!("{:?}", role.role).to_ascii_lowercase();
            provider.files.insert(
                (manifest.pack_id.clone(), role.lut_path.clone()),
                format!("lut-{stem}").into_bytes(),
            );
            provider.files.insert(
                (manifest.pack_id.clone(), role.config_path.clone()),
                format!("config-{stem}").into_bytes(),
            );
        }
        provider.manifests.insert(
            manifest.pack_id.clone(),
            serde_json::to_vec(&manifest).expect("serialize fixture manifest"),
        );
        provider
    }

    #[test]
    fn selection_is_exact_and_never_interpolates_frequency() {
        let provider = provider_with_manifest(fake_manifest("s-pack", TMatrixResearchBand::S));
        let discoveries = discover_tmatrix_band_packs(&provider).expect("discover fixture pack");
        let selected = select_exact_tmatrix_band_pack(&discoveries, S_BAND_RESEARCH_FREQUENCY_HZ)
            .expect("select exact S frequency");
        assert_eq!(selected.identity.band(), TMatrixResearchBand::S);
        assert!(matches!(
            select_exact_tmatrix_band_pack(&discoveries, S_BAND_RESEARCH_FREQUENCY_HZ + 1.0),
            Err(TMatrixBandPackError::UnsupportedExactFrequency { .. })
        ));
        assert!(matches!(
            select_exact_tmatrix_band_pack(&discoveries, C_BAND_RESEARCH_FREQUENCY_HZ),
            Err(TMatrixBandPackError::PackUnavailable {
                band: TMatrixResearchBand::C,
                ..
            })
        ));
    }

    #[test]
    fn an_unvalidated_c_pack_is_unavailable_not_a_fallback_to_s() {
        let mut manifest = fake_manifest("c-pack", TMatrixResearchBand::C);
        manifest.validation_status = TMatrixBandPackValidationStatus::UnvalidatedResearch;
        let provider = provider_with_manifest(manifest);
        let discoveries = discover_tmatrix_band_packs(&provider).expect("complete discovery");
        assert!(matches!(
            discoveries.as_slice(),
            [TMatrixBandPackDiscovery::Invalid { .. }]
        ));
        assert!(matches!(
            select_exact_tmatrix_band_pack(&discoveries, C_BAND_RESEARCH_FREQUENCY_HZ),
            Err(TMatrixBandPackError::PackUnavailable {
                band: TMatrixResearchBand::C,
                ..
            })
        ));
    }

    #[test]
    fn strict_file_hash_gate_rejects_tampering_before_science_decode() {
        let mut provider = provider_with_manifest(fake_manifest("s-pack", TMatrixResearchBand::S));
        provider.files.insert(
            ("s-pack".to_owned(), "dryoblate.lut".to_owned()),
            b"lut-dryoblatf".to_vec(),
        );
        let discoveries = discover_tmatrix_band_packs(&provider).expect("discover fixture pack");
        let selected = select_exact_tmatrix_band_pack(&discoveries, S_BAND_RESEARCH_FREQUENCY_HZ)
            .expect("select S pack");
        assert!(matches!(
            read_validated_pack_bytes(&provider, selected),
            Err(TMatrixBandPackError::FileSha256Mismatch {
                role: TMatrixBandPackRole::DryOblate,
                ..
            })
        ));
    }

    #[test]
    fn manifest_missing_any_required_role_fails_closed() {
        let mut manifest = fake_manifest("x-pack", TMatrixResearchBand::X);
        manifest
            .role_files
            .retain(|file| file.role != TMatrixBandPackRole::WetProlate);
        let provider = provider_with_manifest(manifest);
        let discoveries = discover_tmatrix_band_packs(&provider).expect("complete discovery");
        assert!(matches!(
            discoveries.as_slice(),
            [TMatrixBandPackDiscovery::Invalid { error, .. }] if error.contains("wet prolate")
        ));
    }

    #[test]
    fn cache_retains_only_one_key_and_does_not_replace_on_failure() {
        let cache = OneSelectedPackCache::<String>::default();
        let loads = AtomicUsize::new(0);
        let key_s = digest("s");
        let first = cache
            .load(key_s, || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>("S".to_owned())
            })
            .expect("load S");
        let again = cache
            .load(key_s, || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>("unexpected".to_owned())
            })
            .expect("reuse S");
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(loads.load(Ordering::SeqCst), 1);

        assert!(
            cache
                .load(digest("missing-c"), || Err::<String, _>("missing"))
                .is_err()
        );
        let retained = cache
            .load(key_s, || Ok::<_, ()>("unexpected".to_owned()))
            .expect("failed replacement retained S");
        assert!(Arc::ptr_eq(&first, &retained));

        let x = cache
            .load(digest("x"), || Ok::<_, ()>("X".to_owned()))
            .expect("replace with X");
        assert_eq!(x.as_str(), "X");
        let x_again = cache
            .load(digest("x"), || Ok::<_, ()>("unexpected".to_owned()))
            .expect("reuse X");
        assert!(Arc::ptr_eq(&x, &x_again));
    }

    #[test]
    fn legacy_embedded_s_identity_is_not_an_external_pack() {
        assert_eq!(
            legacy_embedded_s_research_v1_identity(),
            TMatrixBandPackIdentity::LegacyEmbeddedSResearchV1
        );
    }
}
