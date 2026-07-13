//! Developer-only kernel rebuild helper. Normal BowEcho builds include the
//! checked-in CUBIN/PTX artifacts and never invoke NVRTC or require a CUDA
//! Toolkit. CUBINs keep supported installed GPUs independent of the PTX ISA
//! version understood by an older-but-compatible NVIDIA driver; PTX remains a
//! forward-compatible fallback for architectures not known to this build.

use std::{
    ffi::{CStr, CString, c_char},
    path::PathBuf,
};

use cudarc::nvrtc::{result, sys};
use serde_json::json;
use sha2::{Digest, Sha256};

const ARCHITECTURES: &[u32] = &[75, 80, 86, 87, 88, 89, 90, 100, 103, 110, 120, 121];
const COMMON_OPTIONS: &[&str] = &[
    "--ftz=false",
    "--prec-sqrt=true",
    "--prec-div=true",
    "--fmad=false",
    "--std=c++17",
];

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

unsafe fn get_cubin(program: sys::nvrtcProgram) -> Result<Vec<u8>, result::NvrtcError> {
    let mut size = 0_usize;
    unsafe { sys::nvrtcGetCUBINSize(program, &mut size) }.result()?;
    let mut cubin = vec![0_u8; size];
    unsafe { sys::nvrtcGetCUBIN(program, cubin.as_mut_ptr().cast::<c_char>()) }.result()?;
    Ok(cubin)
}

fn compile(source: &CString, architecture: &str, cubin: bool) -> Result<Vec<u8>, String> {
    let name = c"p3_lut_segments.cu";
    let program = result::create_program(source, Some(name)).map_err(|error| error.to_string())?;
    let mut options = COMMON_OPTIONS
        .iter()
        .map(|option| (*option).to_owned())
        .collect::<Vec<_>>();
    options.push(format!("--gpu-architecture={architecture}"));
    let compile_result = unsafe { result::compile_program(program, &options) };
    if let Err(error) = compile_result {
        let log = unsafe { result::get_program_log(program) }
            .ok()
            .and_then(|bytes| {
                unsafe { CStr::from_ptr(bytes.as_ptr()) }
                    .to_str()
                    .ok()
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "NVRTC returned no compiler log".to_owned());
        let _ = unsafe { result::destroy_program(program) };
        return Err(format!("{error}: {log}"));
    }
    let output = if cubin {
        unsafe { get_cubin(program) }.map_err(|error| error.to_string())?
    } else {
        let bytes = unsafe { result::get_ptx(program) }.map_err(|error| error.to_string())?;
        let mut bytes = bytes.into_iter().map(|byte| byte as u8).collect::<Vec<_>>();
        if bytes.last() == Some(&0) {
            bytes.pop();
        }
        bytes
    };
    unsafe { result::destroy_program(program) }.map_err(|error| error.to_string())?;
    Ok(output)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_path = crate_root.join("kernels/p3_lut_segments.cu");
    let source_bytes = std::fs::read(&source_path)?;
    let source = CString::new(source_bytes.as_slice())?;
    let ptx_path = crate_root.join("kernels/p3_lut_segments.ptx");
    let ptx = compile(&source, "compute_75", false).map_err(std::io::Error::other)?;
    std::fs::write(&ptx_path, &ptx)?;

    let mut artifacts = Vec::new();
    for &architecture in ARCHITECTURES {
        let label = format!("sm_{architecture}");
        let cubin = compile(&source, &label, true).map_err(std::io::Error::other)?;
        let file_name = format!("p3_lut_segments_sm{architecture}.cubin");
        std::fs::write(crate_root.join("kernels").join(&file_name), &cubin)?;
        artifacts.push(json!({
            "architecture": label,
            "file": file_name,
            "bytes": cubin.len(),
            "sha256": sha256(&cubin),
        }));
    }
    artifacts.push(json!({
        "architecture": "compute_75 PTX fallback",
        "file": "p3_lut_segments.ptx",
        "bytes": ptx.len(),
        "sha256": sha256(&ptx),
    }));
    let manifest = json!({
        "abi_revision": 1,
        "kernel": "bowecho_p3_lut_segments_v1",
        "generator": "NVIDIA NVRTC 13.0",
        "source": "p3_lut_segments.cu",
        "source_sha256": sha256(&source_bytes),
        "options": COMMON_OPTIONS,
        "ptx_virtual_architecture": "compute_75",
        "artifacts": artifacts,
    });
    let manifest_path = crate_root.join("kernels/p3_lut_segments.manifest.json");
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    println!(
        "wrote {} artifacts and {}",
        ARCHITECTURES.len() + 1,
        manifest_path.display()
    );
    Ok(())
}
