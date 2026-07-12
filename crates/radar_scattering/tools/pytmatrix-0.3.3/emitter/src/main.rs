use std::{env, fs, io::Write, path::PathBuf, process::ExitCode};

use radar_scattering::{AdditiveScattering, Axis, GeneratorMetadata, OfflineLut, ScienceMetadata};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmitRequest {
    axes: Vec<Axis>,
    generator: GeneratorMetadata,
    generator_config_utf8: String,
    science: ScienceMetadata,
    value_f64_bits_hex: Vec<[String; AdditiveScattering::COMPONENT_COUNT]>,
}

fn decode_component(point: usize, component: usize, text: &str) -> Result<f64, String> {
    if text.len() != 16
        || text
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "point {point} component {component} is not 16 lowercase hex digits"
        ));
    }
    let bits = u64::from_str_radix(text, 16).map_err(|error| {
        format!("point {point} component {component} has invalid bits: {error}")
    })?;
    Ok(f64::from_bits(bits))
}

fn run() -> Result<(), String> {
    let mut args = env::args_os();
    let executable = args.next().unwrap_or_default();
    let request_path = args.next().map(PathBuf::from).ok_or_else(|| {
        format!(
            "usage: {} REQUEST.json OUTPUT.lut",
            PathBuf::from(&executable).display()
        )
    })?;
    let output_path = args.next().map(PathBuf::from).ok_or_else(|| {
        format!(
            "usage: {} REQUEST.json OUTPUT.lut",
            PathBuf::from(&executable).display()
        )
    })?;
    if args.next().is_some() {
        return Err("unexpected extra command-line arguments".to_owned());
    }

    let request_bytes = fs::read(&request_path)
        .map_err(|error| format!("read {}: {error}", request_path.display()))?;
    let request: EmitRequest = serde_json::from_slice(&request_bytes)
        .map_err(|error| format!("parse {}: {error}", request_path.display()))?;
    let values = request
        .value_f64_bits_hex
        .into_iter()
        .enumerate()
        .map(|(point, encoded)| {
            let mut components = [0.0; AdditiveScattering::COMPONENT_COUNT];
            for (component, (destination, text)) in components.iter_mut().zip(encoded).enumerate() {
                *destination = decode_component(point, component, &text)?;
            }
            AdditiveScattering::from_components(components)
                .map_err(|error| format!("invalid point {point}: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let lut = OfflineLut::new(
        request.axes,
        request.generator,
        request.generator_config_utf8,
        request.science,
        values,
    )
    .map_err(|error| format!("construct LUT: {error}"))?;
    let bytes = lut
        .to_bytes()
        .map_err(|error| format!("serialize LUT: {error}"))?;

    let parent = output_path
        .parent()
        .ok_or_else(|| "output path has no parent".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let mut file = fs::File::create(&output_path)
        .map_err(|error| format!("create {}: {error}", output_path.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write {}: {error}", output_path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", output_path.display()))?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("brslut-emitter: {error}");
            ExitCode::FAILURE
        }
    }
}
