//! On-demand acquisition of BowEcho's exact legacy five-role S-band
//! property-aware T-matrix research tables.
//!
//! Production binaries contain no LUT/config payloads. The byte-exact tables
//! are a separately versioned release asset, streamed into the existing
//! property-T-matrix cache, archive- and member-hash qualified, safely
//! extracted through a temporary directory, and decoded only on first use.

#![cfg_attr(test, allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use radar_scattering::{ResearchTMatrixLut, Sha256Digest};
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::wrf_tmatrix_scene::WrfTMatrixLutBundle;

pub const LEGACY_PROPERTY_PACK_RELEASE_TAG: &str = "v0.34.1";
pub const LEGACY_PROPERTY_PACK_ASSET_NAME: &str =
    "bowecho-property-tmatrix-sband-pytmatrix-0.3.3-research-v1.zip";
pub const LEGACY_PROPERTY_PACK_URL: &str = concat!(
    "https://github.com/FahrenheitResearch/bowecho/releases/download/v0.34.1/",
    "bowecho-property-tmatrix-sband-pytmatrix-0.3.3-research-v1.zip"
);
pub const LEGACY_PROPERTY_PACK_SHA256: &str =
    "80b3a2c65ead59c0a951d491e966694e80bb0c49eeb1d3b1fc532bcadbcf507e";
pub const LEGACY_PROPERTY_PACK_ARCHIVE_BYTES: u64 = 191_400_602;
pub const LEGACY_PROPERTY_PACK_EXPANDED_BYTES: u64 = 191_398_582;
pub const LEGACY_PROPERTY_TABLE_SOURCE_BYTES: usize = 191_397_511;

const LEGACY_PROPERTY_PACK_ID: &str = "legacy-sband-pytmatrix-0.3.3-research-v1";

#[derive(Clone, Copy)]
struct PackFileSpec {
    relative_path: &'static str,
    expected_bytes: u64,
    expected_sha256: &'static str,
}

#[derive(Clone, Copy)]
struct LegacyTableSpec {
    label: &'static str,
    lut: PackFileSpec,
    config: PackFileSpec,
}

const TABLE_SPECS: [LegacyTableSpec; 5] = [
    LegacyTableSpec {
        label: "dry oblate",
        lut: PackFileSpec {
            relative_path: "property_p3_ishmael_dry_oblate_sband_unvalidated/table.lut",
            expected_bytes: 44_365_435,
            expected_sha256: "30c8da4093b845faa415339f2cb5b4831f3450dc18afea3aacb2e2fabdcc4ad8",
        },
        config: PackFileSpec {
            relative_path: "property_p3_ishmael_dry_oblate_sband_unvalidated/config.json",
            expected_bytes: 8_274,
            expected_sha256: "e08adbe6d6e8a1b9a80ba920a0f82539c4056d9758e1e44ba11bcf907ba5cd19",
        },
    },
    LegacyTableSpec {
        label: "dry prolate",
        lut: PackFileSpec {
            relative_path: "property_p3_ishmael_dry_prolate_sband_unvalidated/table.lut",
            expected_bytes: 9_527_106,
            expected_sha256: "7a563e1103cb1a61ccb94ce72513d82b9fdd68a6faddb4aa8ae46112fb0109c0",
        },
        config: PackFileSpec {
            relative_path: "property_p3_ishmael_dry_prolate_sband_unvalidated/config.json",
            expected_bytes: 7_319,
            expected_sha256: "c2b973ab36fa26edb8d9d82f7dbb2ae5df52feec4bf93c880e304cad5aa2ff49",
        },
    },
    LegacyTableSpec {
        label: "wet oblate",
        lut: PackFileSpec {
            relative_path: "property_p3_ishmael_wet_oblate_sband_unvalidated/table.lut",
            expected_bytes: 73_279_689,
            expected_sha256: "6c376422c512ebfc37dc5b2038defea799995d1821170da74b4af87276df1dd7",
        },
        config: PackFileSpec {
            relative_path: "property_p3_ishmael_wet_oblate_sband_unvalidated/config.json",
            expected_bytes: 8_028,
            expected_sha256: "61cd6f72beaf503485168a9b43e72db8ebc49ef993389e67ff8906e0c42e9bf8",
        },
    },
    LegacyTableSpec {
        label: "wet prolate",
        lut: PackFileSpec {
            relative_path: "property_p3_ishmael_wet_prolate_sband_unvalidated/table.lut",
            expected_bytes: 62_220_152,
            expected_sha256: "9c55a51eb63a982005564eb1f35bbb24dfad5f22a65ed820ac7c1d5cf19f1040",
        },
        config: PackFileSpec {
            relative_path: "property_p3_ishmael_wet_prolate_sband_unvalidated/config.json",
            expected_bytes: 7_857,
            expected_sha256: "0fa0bd759b64c8e6f62bcf629fd2d5c2733aa433fb0e2eb2793c2cbaa58e1758",
        },
    },
    LegacyTableSpec {
        label: "standalone/residual rain",
        lut: PackFileSpec {
            relative_path: "property_rain_sband_unvalidated/table.lut",
            expected_bytes: 1_968_373,
            expected_sha256: "396ca95c58d70a9a413d90799bd790dc389179dc9a38f48152e464bf852d5e11",
        },
        config: PackFileSpec {
            relative_path: "property_rain_sband_unvalidated/config.json",
            expected_bytes: 5_278,
            expected_sha256: "387f93b5998d9f6010ffb60d081b31dc64a556cf33ae7b34256fc356d26140b5",
        },
    },
];

const PYTMATRIX_LICENSE: PackFileSpec = PackFileSpec {
    relative_path: "PYTMATRIX-LICENSE.txt",
    expected_bytes: 1_071,
    expected_sha256: "be9109e8cf7842d4e789a6d314c011b4a1773059020895bc1f032882a03bae1d",
};

pub(crate) struct LegacyPropertyTMatrixLuts {
    pub(crate) dry_oblate: ResearchTMatrixLut,
    pub(crate) dry_prolate: ResearchTMatrixLut,
    pub(crate) wet_oblate: ResearchTMatrixLut,
    pub(crate) wet_prolate: ResearchTMatrixLut,
    pub(crate) rain: ResearchTMatrixLut,
}

impl LegacyPropertyTMatrixLuts {
    pub(crate) fn bundle(&self) -> WrfTMatrixLutBundle<'_> {
        WrfTMatrixLutBundle::new(
            &self.dry_oblate,
            &self.dry_prolate,
            &self.wet_oblate,
            &self.wet_prolate,
            &self.rain,
        )
    }
}

static LEGACY_PROPERTY_LUTS: OnceLock<LegacyPropertyTMatrixLuts> = OnceLock::new();
static LEGACY_PROPERTY_LOAD_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn load_or_download_legacy_property_luts(
    pack_cache_root: &Path,
    progress: &(impl Fn(&str) + ?Sized),
) -> Result<&'static LegacyPropertyTMatrixLuts, String> {
    if let Some(tables) = LEGACY_PROPERTY_LUTS.get() {
        return Ok(tables);
    }
    let _guard = LEGACY_PROPERTY_LOAD_LOCK
        .lock()
        .map_err(|_| "property T-matrix research data-pack lock was poisoned".to_owned())?;
    if let Some(tables) = LEGACY_PROPERTY_LUTS.get() {
        return Ok(tables);
    }

    let tables = acquire_legacy_property_luts(pack_cache_root, progress)?;
    let _ = LEGACY_PROPERTY_LUTS.set(tables);
    Ok(LEGACY_PROPERTY_LUTS
        .get()
        .expect("qualified property T-matrix tables were installed"))
}

fn acquire_legacy_property_luts(
    pack_cache_root: &Path,
    progress: &(impl Fn(&str) + ?Sized),
) -> Result<LegacyPropertyTMatrixLuts, String> {
    fs::create_dir_all(pack_cache_root).map_err(|error| {
        format!(
            "create property T-matrix pack cache {}: {error}",
            pack_cache_root.display()
        )
    })?;
    let installed_dir = pack_cache_root.join(LEGACY_PROPERTY_PACK_ID);
    let archive_path = pack_cache_root.join(LEGACY_PROPERTY_PACK_ASSET_NAME);
    let partial_archive_path = archive_path.with_extension("download");
    match load_legacy_property_luts_from_dir(&installed_dir) {
        Ok(tables) => {
            for leftover in [&archive_path, &partial_archive_path] {
                if let Err(error) = remove_cache_entry(leftover) {
                    progress(&format!(
                        "cached research tables are ready, but a redundant download artifact could not be removed: {error}"
                    ));
                }
            }
            progress(&format!(
                "using cached {:.1} MiB optional property T-matrix research pack from {}",
                LEGACY_PROPERTY_PACK_EXPANDED_BYTES as f64 / 1_048_576.0,
                installed_dir.display()
            ));
            return Ok(tables);
        }
        Err(error) if installed_dir.exists() => {
            progress(&format!(
                "cached property T-matrix research pack failed validation ({error}); replacing {}",
                installed_dir.display()
            ));
            remove_cache_entry(&installed_dir)?;
        }
        Err(_) => {}
    }

    let archive_is_qualified = match validate_archive(&archive_path) {
        Ok(()) => true,
        Err(error) if archive_path.exists() => {
            progress(&format!(
                "cached property T-matrix ZIP failed pinned validation ({error}); downloading a clean copy"
            ));
            remove_cache_entry(&archive_path)?;
            false
        }
        Err(_) => false,
    };
    if !archive_is_qualified {
        progress(&format!(
            "downloading optional {:.1} MiB property T-matrix research pack from BowEcho {} to {}",
            LEGACY_PROPERTY_PACK_ARCHIVE_BYTES as f64 / 1_048_576.0,
            LEGACY_PROPERTY_PACK_RELEASE_TAG,
            pack_cache_root.display()
        ));
        progress(
            "PyTMatrix 0.3.3 is MIT-licensed; these derived tables remain research-only, are not independently validated, and are not an operational calibration.",
        );
        data_source::gdex::download_to_path(LEGACY_PROPERTY_PACK_URL, &archive_path).map_err(
            |error| {
                format!(
                    "download optional property T-matrix research pack from {LEGACY_PROPERTY_PACK_URL} into {}: {error}",
                    pack_cache_root.display()
                )
            },
        )?;
        progress("download complete; verifying the pinned archive SHA-256");
        if let Err(error) = validate_archive(&archive_path) {
            let _ = remove_cache_entry(&archive_path);
            return Err(format!(
                "downloaded property T-matrix research pack failed pinned validation and was removed: {error}"
            ));
        }
    }

    progress("extracting 11 exact research-pack members through a temporary cache directory");
    let temporary_dir = pack_cache_root.join(format!(
        ".{LEGACY_PROPERTY_PACK_ID}.extracting-{}",
        std::process::id()
    ));
    if temporary_dir.exists() {
        remove_cache_entry(&temporary_dir)?;
    }
    fs::create_dir(&temporary_dir).map_err(|error| {
        format!(
            "create temporary property T-matrix extraction directory {}: {error}",
            temporary_dir.display()
        )
    })?;
    let specs = expected_pack_files();
    if let Err(error) = extract_validated_zip(&archive_path, &temporary_dir, &specs) {
        let _ = remove_cache_entry(&temporary_dir);
        return Err(error);
    }
    fs::rename(&temporary_dir, &installed_dir).map_err(|error| {
        let _ = remove_cache_entry(&temporary_dir);
        format!(
            "atomically install property T-matrix research pack at {}: {error}",
            installed_dir.display()
        )
    })?;

    progress("decoding and cross-validating the exact five-role property T-matrix bundle");
    let tables = match load_legacy_property_luts_from_dir(&installed_dir) {
        Ok(tables) => tables,
        Err(error) => {
            let _ = remove_cache_entry(&installed_dir);
            return Err(format!(
                "installed property T-matrix research pack failed typed LUT validation and was removed: {error}"
            ));
        }
    };
    // The extracted tables are the steady-state cache. Avoid retaining a
    // second ~183 MiB copy on disk after the atomic install succeeds.
    if let Err(error) = remove_cache_entry(&archive_path) {
        progress(&format!(
            "qualified tables are ready, but the temporary downloaded ZIP could not be removed: {error}"
        ));
    }
    if let Err(error) = remove_cache_entry(&partial_archive_path) {
        progress(&format!(
            "qualified tables are ready, but a redundant partial download could not be removed: {error}"
        ));
    }
    progress(&format!(
        "qualified optional property T-matrix research pack {} (archive SHA-256 {})",
        LEGACY_PROPERTY_PACK_ID, LEGACY_PROPERTY_PACK_SHA256
    ));
    Ok(tables)
}

fn expected_pack_files() -> Vec<PackFileSpec> {
    let mut files = Vec::with_capacity(TABLE_SPECS.len() * 2 + 1);
    for table in TABLE_SPECS {
        files.push(table.lut);
        files.push(table.config);
    }
    files.push(PYTMATRIX_LICENSE);
    files
}

fn validate_archive(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("open research-pack ZIP {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "research-pack ZIP {} is not a regular file",
            path.display()
        ));
    }
    if metadata.len() != LEGACY_PROPERTY_PACK_ARCHIVE_BYTES {
        return Err(format!(
            "research-pack ZIP has {} bytes, expected {}",
            metadata.len(),
            LEGACY_PROPERTY_PACK_ARCHIVE_BYTES
        ));
    }
    let actual = sha256_file(path)?;
    if actual != LEGACY_PROPERTY_PACK_SHA256 {
        return Err(format!(
            "research-pack ZIP SHA-256 is {actual}, expected {LEGACY_PROPERTY_PACK_SHA256}"
        ));
    }
    Ok(())
}

fn extract_validated_zip(
    archive_path: &Path,
    output_dir: &Path,
    expected_files: &[PackFileSpec],
) -> Result<(), String> {
    let file = File::open(archive_path).map_err(|error| {
        format!(
            "open property T-matrix ZIP {}: {error}",
            archive_path.display()
        )
    })?;
    let mut archive = ZipArchive::new(BufReader::new(file))
        .map_err(|error| format!("read property T-matrix ZIP directory: {error}"))?;
    if archive.len() != expected_files.len() {
        return Err(format!(
            "property T-matrix ZIP has {} members, expected exactly {}",
            archive.len(),
            expected_files.len()
        ));
    }
    let expected = expected_files
        .iter()
        .map(|spec| (spec.relative_path, *spec))
        .collect::<BTreeMap<_, _>>();
    if expected.len() != expected_files.len() {
        return Err(
            "internal property T-matrix pack specification contains duplicate paths".to_owned(),
        );
    }
    let mut seen = BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("open research-pack ZIP member {index}: {error}"))?;
        let name = entry.name().to_owned();
        let spec = expected.get(name.as_str()).ok_or_else(|| {
            format!("research-pack ZIP contains unexpected or unsafe member `{name}`")
        })?;
        if entry.is_dir() || !seen.insert(name.clone()) {
            return Err(format!(
                "research-pack ZIP member `{name}` is a directory or duplicate"
            ));
        }
        if entry.size() != spec.expected_bytes {
            return Err(format!(
                "research-pack ZIP member `{name}` declares {} bytes, expected {}",
                entry.size(),
                spec.expected_bytes
            ));
        }
        let destination = output_dir.join(spec.relative_path);
        let parent = destination
            .parent()
            .ok_or_else(|| format!("research-pack member `{name}` has no safe parent directory"))?;
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "create research-pack member directory {}: {error}",
                parent.display()
            )
        })?;
        let output = File::create(&destination).map_err(|error| {
            format!(
                "create extracted research-pack member {}: {error}",
                destination.display()
            )
        })?;
        let mut output = BufWriter::new(output);
        let mut digest = Sha256::new();
        let mut written = 0_u64;
        let mut buffer = [0_u8; 1024 * 1024];
        loop {
            let count = entry
                .read(&mut buffer)
                .map_err(|error| format!("read ZIP member `{name}`: {error}"))?;
            if count == 0 {
                break;
            }
            written = written
                .checked_add(count as u64)
                .ok_or_else(|| format!("ZIP member `{name}` expanded-size overflow"))?;
            if written > spec.expected_bytes {
                return Err(format!(
                    "ZIP member `{name}` expanded beyond its fixed {}-byte limit",
                    spec.expected_bytes
                ));
            }
            output
                .write_all(&buffer[..count])
                .map_err(|error| format!("write extracted ZIP member `{name}`: {error}"))?;
            digest.update(&buffer[..count]);
        }
        output
            .flush()
            .map_err(|error| format!("flush extracted ZIP member `{name}`: {error}"))?;
        drop(output);
        if written != spec.expected_bytes {
            return Err(format!(
                "ZIP member `{name}` expanded to {written} bytes, expected {}",
                spec.expected_bytes
            ));
        }
        let actual_sha256 = format!("{:x}", digest.finalize());
        if actual_sha256 != spec.expected_sha256 {
            return Err(format!(
                "ZIP member `{name}` SHA-256 is {actual_sha256}, expected {}",
                spec.expected_sha256
            ));
        }
    }
    for path in expected.keys() {
        if !seen.contains(*path) {
            return Err(format!(
                "research-pack ZIP is missing exact member `{path}`"
            ));
        }
    }
    Ok(())
}

fn load_legacy_property_luts_from_dir(root: &Path) -> Result<LegacyPropertyTMatrixLuts, String> {
    validate_file(root, PYTMATRIX_LICENSE)?;
    let dry_oblate = load_one_table(root, TABLE_SPECS[0])?;
    let dry_prolate = load_one_table(root, TABLE_SPECS[1])?;
    let wet_oblate = load_one_table(root, TABLE_SPECS[2])?;
    let wet_prolate = load_one_table(root, TABLE_SPECS[3])?;
    let rain = load_one_table(root, TABLE_SPECS[4])?;
    let tables = LegacyPropertyTMatrixLuts {
        dry_oblate,
        dry_prolate,
        wet_oblate,
        wet_prolate,
        rain,
    };
    tables
        .bundle()
        .validate()
        .map_err(|error| format!("validate complete property T-matrix research bundle: {error}"))?;
    Ok(tables)
}

fn load_one_table(root: &Path, spec: LegacyTableSpec) -> Result<ResearchTMatrixLut, String> {
    let lut_bytes = read_validated_file(root, spec.lut)?;
    let config_bytes = read_validated_file(root, spec.config)?;
    let expected = Sha256Digest::from_hex(spec.lut.expected_sha256)
        .map_err(|error| format!("invalid pinned {} table SHA-256: {error}", spec.label))?;
    // ResearchTMatrixLut owns its decoded representation. These two source
    // buffers are dropped as soon as this call returns.
    ResearchTMatrixLut::load(&lut_bytes, expected, &config_bytes).map_err(|error| {
        format!(
            "decode exact {} table `{}`: {error}",
            spec.label, spec.lut.relative_path
        )
    })
}

fn validate_file(root: &Path, spec: PackFileSpec) -> Result<(), String> {
    read_validated_file(root, spec).map(drop)
}

fn read_validated_file(root: &Path, spec: PackFileSpec) -> Result<Vec<u8>, String> {
    let path = root.join(spec.relative_path);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("open research-pack member {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "research-pack member {} is not a regular file",
            path.display()
        ));
    }
    if metadata.len() != spec.expected_bytes {
        return Err(format!(
            "research-pack member `{}` has {} bytes, expected {}",
            spec.relative_path,
            metadata.len(),
            spec.expected_bytes
        ));
    }
    let bytes = fs::read(&path)
        .map_err(|error| format!("read research-pack member {}: {error}", path.display()))?;
    let actual_sha256 = format!("{:x}", Sha256::digest(&bytes));
    if actual_sha256 != spec.expected_sha256 {
        return Err(format!(
            "research-pack member `{}` SHA-256 is {actual_sha256}, expected {}",
            spec.relative_path, spec.expected_sha256
        ));
    }
    Ok(bytes)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let file = File::open(path)
        .map_err(|error| format!("open {} for SHA-256: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("hash {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn remove_cache_entry(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("inspect cache entry {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path)
            .map_err(|error| format!("remove cache file {}: {error}", path.display()))
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)
            .map_err(|error| format!("remove cache directory {}: {error}", path.display()))
    } else {
        Err(format!(
            "refusing to remove unsupported cache entry {}",
            path.display()
        ))
    }
}

#[cfg(test)]
pub(crate) fn load_repository_legacy_property_luts() -> Result<LegacyPropertyTMatrixLuts, String> {
    load_legacy_property_luts_from_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../research_only_assets/tmatrix/pytmatrix-0.3.3"),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;

    const TEST_TABLE: PackFileSpec = PackFileSpec {
        relative_path: "role/table.lut",
        expected_bytes: 11,
        expected_sha256: "625a7070a5bc1ee9631cd09a2ad775c8dc78a8748e06fb546264d99e8b0fe76a",
    };
    const TEST_CONFIG: PackFileSpec = PackFileSpec {
        relative_path: "role/config.json",
        expected_bytes: 12,
        expected_sha256: "6f39480b93bd351dc32b494eb82a5d5ad422b65f65b56450c49c0448676146f3",
    };

    #[test]
    fn production_specs_match_byte_exact_repository_inputs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../research_only_assets/tmatrix/pytmatrix-0.3.3");
        let specs = expected_pack_files();
        assert_eq!(specs.len(), 11);
        assert_eq!(
            specs.iter().map(|spec| spec.expected_bytes).sum::<u64>(),
            LEGACY_PROPERTY_PACK_EXPANDED_BYTES
        );
        for spec in specs {
            validate_file(&root, spec).unwrap_or_else(|error| panic!("{error}"));
        }
    }

    #[test]
    fn exact_zip_members_extract_and_hash_qualify() {
        let root = unique_test_dir("exact");
        let archive = root.join("pack.zip");
        let output = root.join("output");
        fs::create_dir_all(&output).expect("create extraction output");
        write_test_zip(
            &archive,
            &[
                (TEST_TABLE.relative_path, b"table-bytes"),
                (TEST_CONFIG.relative_path, b"config-bytes"),
            ],
        );
        extract_validated_zip(&archive, &output, &[TEST_TABLE, TEST_CONFIG])
            .expect("extract exact validated members");
        assert_eq!(
            fs::read(output.join(TEST_TABLE.relative_path)).expect("read table"),
            b"table-bytes"
        );
        assert_eq!(
            fs::read(output.join(TEST_CONFIG.relative_path)).expect("read config"),
            b"config-bytes"
        );
        fs::remove_dir_all(root).expect("clean exact extraction fixture");
    }

    #[test]
    fn zip_rejects_extra_and_traversal_members() {
        for (label, members) in [
            (
                "extra",
                vec![
                    (TEST_TABLE.relative_path, b"table-bytes".as_slice()),
                    (TEST_CONFIG.relative_path, b"config-bytes".as_slice()),
                    ("extra.bin", b"evil".as_slice()),
                ],
            ),
            (
                "traversal",
                vec![
                    ("../table.lut", b"table-bytes".as_slice()),
                    (TEST_CONFIG.relative_path, b"config-bytes".as_slice()),
                ],
            ),
        ] {
            let root = unique_test_dir(label);
            let archive = root.join("pack.zip");
            let output = root.join("output");
            fs::create_dir_all(&output).expect("create rejection output");
            write_test_zip(&archive, &members);
            let error = extract_validated_zip(&archive, &output, &[TEST_TABLE, TEST_CONFIG])
                .expect_err("unsafe ZIP shape must fail closed");
            assert!(
                error.contains("exactly") || error.contains("unexpected or unsafe"),
                "{error}"
            );
            fs::remove_dir_all(root).expect("clean rejection fixture");
        }
    }

    fn write_test_zip(path: &Path, members: &[(&str, &[u8])]) {
        let cursor = Cursor::new(Vec::new());
        let mut archive = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for &(name, bytes) in members {
            archive
                .start_file(name, options)
                .expect("start test ZIP member");
            archive.write_all(bytes).expect("write test ZIP member");
        }
        let cursor = archive.finish().expect("finish test ZIP");
        fs::write(path, cursor.into_inner()).expect("write test ZIP fixture");
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bowecho-property-pack-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test fixture root");
        path
    }
}
