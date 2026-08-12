// SPDX-License-Identifier: Apache-2.0

//! Standalone ArWen Studio entry point retained for its acceptance matrix.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(arwen_studio::run_standalone(&args));
}
