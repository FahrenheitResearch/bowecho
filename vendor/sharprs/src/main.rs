//! CLI entry point for rendering SHARPpy-style sounding analysis images.
//!
//! Usage:
//!
//! ```sh
//! cargo run -- tests/soundings/SHV_20250402_00Z.csv output.png
//! ```

use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 3 {
        eprintln!("Usage: {} <sounding_file> <output.png>", args[0]);
        eprintln!();
        eprintln!("Supported formats:");
        eprintln!("  .csv     - Simple CSV (pres,hght,tmpc,dwpc,wdir,wspd)");
        eprintln!("  .txt     - SHARPpy %%RAW%% format or U. Wyoming text");
        eprintln!();
        eprintln!("Example:");
        eprintln!("  cargo run -- tests/soundings/SHV_20250402_00Z.csv output.png");
        process::exit(1);
    }

    let input_path = &args[1];
    let output_path = &args[2];

    // Read the sounding file
    let text = match fs::read_to_string(input_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error reading {}: {}", input_path, e);
            process::exit(1);
        }
    };

    // Parse the sounding (auto-detect format)
    let profile = if input_path.ends_with(".csv") {
        sharprs::Profile::from_csv(&text)
    } else if text.contains("%RAW%") {
        sharprs::Profile::from_sharppy_text(&text)
    } else {
        // Try Wyoming format, fall back to CSV
        sharprs::Profile::from_wyoming(&text).or_else(|_| sharprs::Profile::from_csv(&text))
    };

    let profile = match profile {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error parsing sounding: {}", e);
            process::exit(1);
        }
    };

    eprintln!(
        "Loaded sounding: {} levels, station={}, sfc={:.1} hPa",
        profile.num_levels(),
        profile.station.station_id,
        profile.sfc_pressure(),
    );

    // Compute all parameters
    eprintln!("Computing parameters...");
    let params = sharprs::render::compute_all_params(&profile);

    eprintln!(
        "  SBCAPE={:.0}  MLCAPE={:.0}  MUCAPE={:.0}",
        if params.sfcpcl.bplus.is_finite() {
            params.sfcpcl.bplus
        } else {
            0.0
        },
        if params.mlpcl.bplus.is_finite() {
            params.mlpcl.bplus
        } else {
            0.0
        },
        if params.mupcl.bplus.is_finite() {
            params.mupcl.bplus
        } else {
            0.0
        },
    );

    // Render the full sounding image (2400x1800, 2× for crisp output)
    eprintln!(
        "Rendering {}x{} image...",
        sharprs::render::compositor::IMG_W,
        sharprs::render::compositor::IMG_H
    );
    let png_bytes = sharprs::render::render_full_sounding(&profile, &params);

    // Write output
    match fs::write(output_path, &png_bytes) {
        Ok(()) => {
            eprintln!(
                "Wrote {} ({} bytes, {}x{} px)",
                output_path,
                png_bytes.len(),
                sharprs::render::compositor::IMG_W,
                sharprs::render::compositor::IMG_H,
            );
        }
        Err(e) => {
            eprintln!("Error writing {}: {}", output_path, e);
            process::exit(1);
        }
    }
}
