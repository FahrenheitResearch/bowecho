// SPDX-License-Identifier: Apache-2.0

//! Build identity for the stale-binary killer: the short git sha and
//! build time are baked into the binary (title bar, Sys panel, the
//! newer-build-on-disk check). "unknown" when git is unavailable —
//! never a build failure.

fn main() {
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let date = chrono::Utc::now().format("%Y-%m-%d %H:%MZ").to_string();
    println!("cargo:rustc-env=ARWEN_BUILD_SHA={sha}");
    println!("cargo:rustc-env=ARWEN_BUILD_DATE={date}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
}
