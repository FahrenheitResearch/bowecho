use std::path::{Path, PathBuf};

use radar_core::MomentType;
use render2d::{RasterOptions, render_moment_png};

fn main() {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let Some(input) = args.next() else {
        eprintln!(
            "usage: cargo run -p render2d --example render_reflectivity_png -- <level2-file> <out.png> [cut-index]"
        );
        std::process::exit(2);
    };
    let Some(output) = args.next() else {
        eprintln!(
            "usage: cargo run -p render2d --example render_reflectivity_png -- <level2-file> <out.png> [cut-index] [moment]"
        );
        std::process::exit(2);
    };
    let cut_index = std::env::args()
        .nth(3)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let moment = std::env::args()
        .nth(4)
        .as_deref()
        .map(parse_moment)
        .unwrap_or(MomentType::Reflectivity);

    match run(&input, &output, cut_index, moment) {
        Ok(()) => println!("wrote {}", output.display()),
        Err(err) => {
            eprintln!("render failed: {err}");
            std::process::exit(1);
        }
    }
}

fn run(
    input: &Path,
    output: &Path,
    cut_index: usize,
    moment: MomentType,
) -> Result<(), Box<dyn std::error::Error>> {
    let volume = nexrad_io::decode_volume_from_path(input)?;
    render_moment_png(&volume, cut_index, moment, output, RasterOptions::default())?;
    Ok(())
}

fn parse_moment(value: &str) -> MomentType {
    MomentType::from_nexrad_name(&value.to_ascii_uppercase())
}
