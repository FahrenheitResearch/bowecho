use std::env;
use std::error::Error;
use std::fs;
use std::io::{Error as IoError, ErrorKind};

use radar_scattering::{ResearchTMatrixLut, Sha256Digest};

fn argument(
    arguments: &mut impl Iterator<Item = String>,
    name: &'static str,
) -> Result<String, IoError> {
    arguments.next().ok_or_else(|| {
        IoError::new(
            ErrorKind::InvalidInput,
            format!("missing {name}; usage: validate_tmatrix_lut TABLE.lut CONFIG.json LUT_SHA256"),
        )
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    let lut_path = argument(&mut arguments, "TABLE.lut")?;
    let config_path = argument(&mut arguments, "CONFIG.json")?;
    let digest_text = argument(&mut arguments, "LUT_SHA256")?;
    if arguments.next().is_some() {
        return Err(IoError::new(ErrorKind::InvalidInput, "unexpected extra argument").into());
    }

    let lut_bytes = fs::read(&lut_path)?;
    let config_bytes = fs::read(&config_path)?;
    let expected_digest = Sha256Digest::from_hex(&digest_text)?;
    let loaded = ResearchTMatrixLut::load(&lut_bytes, expected_digest, &config_bytes)?;

    println!("table_id={}", loaded.descriptor().table_id());
    println!("lut_sha256={}", loaded.file_sha256());
    println!(
        "population_role={:?}",
        loaded.descriptor().population_role()
    );
    println!(
        "density_applicability={:?}",
        loaded.descriptor().density_applicability()
    );
    println!("execution={:?}", loaded.descriptor().execution());
    println!("axes={:?}", loaded.offline_lut().header().axes());
    Ok(())
}
