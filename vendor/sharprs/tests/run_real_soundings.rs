// Standalone test binary for real soundings
use std::fs;

fn main() {
    let files = vec![
        "tests/soundings/FWD_20250402_00Z.csv",
        "tests/soundings/SHV_20250402_00Z.csv",
        "tests/soundings/SGF_20250402_00Z.csv",
        "tests/soundings/JAX_20250402_00Z.csv",
        "tests/soundings/MPX_20250402_00Z.csv",
    ];

    for path in &files {
        if let Ok(contents) = fs::read_to_string(path) {
            println!("=== {} ===", path);
            let lines: Vec<&str> = contents.lines().skip(1).collect(); // skip header
            println!("Levels: {}", lines.len());
            if let Some(first) = lines.first() {
                println!("First: {}", first);
            }
        }
    }
}
