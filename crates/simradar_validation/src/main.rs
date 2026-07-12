use std::path::{Path, PathBuf};

use simradar_validation::{
    IndependentReferenceSet, VALIDATION_SCHEMA_VERSION, ValidationCaseData, ValidationManifest,
    render_markdown, validate_cases,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("simradar-validate: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args().skip(1);
    let manifest_path = arguments.next().map(PathBuf::from).ok_or_else(usage)?;
    let mut output_json = None;
    let mut output_markdown = None;
    let mut allow_non_independent = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--json" => {
                output_json =
                    Some(PathBuf::from(arguments.next().ok_or_else(|| {
                        "--json requires an output path".to_owned()
                    })?));
            }
            "--markdown" => {
                output_markdown =
                    Some(PathBuf::from(arguments.next().ok_or_else(|| {
                        "--markdown requires an output path".to_owned()
                    })?));
            }
            "--allow-non-independent" => allow_non_independent = true,
            other => return Err(format!("unknown argument '{other}'\n{}", usage())),
        }
    }
    let manifest: ValidationManifest = read_json(&manifest_path)?;
    if manifest.schema_version != VALIDATION_SCHEMA_VERSION {
        return Err(format!(
            "manifest schema {} is unsupported; expected {}",
            manifest.schema_version, VALIDATION_SCHEMA_VERSION
        ));
    }
    let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let mut owned = Vec::with_capacity(manifest.cases.len());
    for case in &manifest.cases {
        let volume_path = resolve(base, &case.bowecho_volume);
        let reference_path = resolve(base, &case.independent_reference);
        let bytes = std::fs::read(&volume_path)
            .map_err(|error| format!("read {}: {error}", volume_path.display()))?;
        let volume = nexrad_io::decode_supported_volume_bytes(&bytes)
            .map_err(|error| format!("decode {}: {error}", volume_path.display()))?;
        let reference: IndependentReferenceSet = read_json(&reference_path)?;
        owned.push((case.id.clone(), volume, reference));
    }
    let borrowed = owned
        .iter()
        .map(|(id, volume, reference)| ValidationCaseData {
            id,
            candidate: volume,
            reference,
        })
        .collect::<Vec<_>>();
    let scorecard = validate_cases(&borrowed, manifest.tolerances, !allow_non_independent)
        .map_err(|error| error.to_string())?;
    let json = serde_json::to_string_pretty(&scorecard)
        .map_err(|error| format!("serialize scorecard: {error}"))?;
    if let Some(path) = output_json {
        std::fs::write(&path, format!("{json}\n"))
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    } else {
        println!("{json}");
    }
    if let Some(path) = output_markdown {
        std::fs::write(&path, render_markdown(&scorecard))
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
}

fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn usage() -> String {
    "usage: simradar-validate <manifest.json> [--json scorecard.json] [--markdown scorecard.md] [--allow-non-independent]".to_owned()
}
