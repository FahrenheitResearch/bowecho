use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn configured_icon(default: &Path) -> PathBuf {
    std::env::var_os("BOWECHO_APP_ICON_ICO")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or_else(|| default.to_path_buf())
}

fn expose_build_default(key: &str) {
    if let Ok(value) = std::env::var(key)
        && !value.trim().is_empty()
    {
        println!("cargo:rustc-env={key}={value}");
    }
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn expose_build_identity() {
    let git = git_output(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .and_then(|output| {
            output
                .status
                .success()
                .then(|| (!output.stdout.is_empty()).to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into());
    let built_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".into());

    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rustc-env=BOWECHO_BUILD_GIT={git}");
    println!("cargo:rustc-env=BOWECHO_BUILD_DIRTY={dirty}");
    println!("cargo:rustc-env=BOWECHO_BUILD_PROFILE={profile}");
    println!("cargo:rustc-env=BOWECHO_BUILD_UNIX={built_unix}");
}

fn main() {
    for key in [
        "BOWECHO_DEFAULT_BRAND",
        "BOWECHO_STORAGE_NAMESPACE",
        "BOWECHO_BRAND_DISPLAY_NAME",
        "BOWECHO_BRAND_SHORT_NAME",
        "BOWECHO_BRAND_ORGANIZATION",
        "BOWECHO_BRAND_TAGLINE",
        "BOWECHO_APP_ICON_ICO",
        "BOWECHO_EXE_NAME",
        "BOWECHO_INTERNAL_NAME",
        "BOWECHO_LEGAL_COPYRIGHT",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    expose_build_default("BOWECHO_DEFAULT_BRAND");
    expose_build_default("BOWECHO_STORAGE_NAMESPACE");
    expose_build_identity();

    // Runtime Brand Kit changes can update UI/export branding and the launch
    // icon on the next run. VERSIONINFO and the embedded executable .ico are
    // build artifacts, so branded distributions set these environment values.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let default_icon = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("bowecho.ico");
        let icon = configured_icon(&default_icon);
        println!("cargo:rerun-if-changed={}", icon.display());

        let preset = std::env::var("BOWECHO_DEFAULT_BRAND")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let cwt = matches!(
            preset.as_str(),
            "cwt" | "california_wildfire_tracking" | "california-wildfire-tracking"
        );
        let display_name = env_or(
            "BOWECHO_BRAND_DISPLAY_NAME",
            if cwt {
                "California Wildfire Tracking"
            } else {
                "BowEcho"
            },
        );
        let short_name = env_or(
            "BOWECHO_BRAND_SHORT_NAME",
            if cwt { "CWT" } else { "BowEcho" },
        );
        let organization = env_or(
            "BOWECHO_BRAND_ORGANIZATION",
            if cwt {
                "Community Wildfire Tracker"
            } else {
                "Fahrenheit Research"
            },
        );
        let tagline = env_or(
            "BOWECHO_BRAND_TAGLINE",
            if cwt {
                "Stay Informed. Stay Prepared. Stay Safe."
            } else {
                "fast NEXRAD Level II radar viewer"
            },
        );
        let exe_name = env_or("BOWECHO_EXE_NAME", "bowecho.exe");
        let internal_name = env_or("BOWECHO_INTERNAL_NAME", "bowecho");
        let copyright = env_or(
            "BOWECHO_LEGAL_COPYRIGHT",
            "Copyright (c) 2026 Fahrenheit Research. MIT OR Apache-2.0.",
        );

        let mut resource = winresource::WindowsResource::new();
        if icon.exists() {
            resource.set_icon(&icon.to_string_lossy());
        }
        resource.set("ProductName", &display_name);
        resource.set("FileDescription", &format!("{display_name} — {tagline}"));
        resource.set("CompanyName", &organization);
        resource.set("LegalCopyright", &copyright);
        resource.set("OriginalFilename", &exe_name);
        resource.set("InternalName", &internal_name);
        resource.set("Comments", &format!("{short_name} desktop application"));
        if let Err(error) = resource.compile() {
            // Non-fatal: a missing rc.exe should not break source builds.
            println!("cargo:warning=windows resource embedding skipped: {error}");
        }
    }
}
