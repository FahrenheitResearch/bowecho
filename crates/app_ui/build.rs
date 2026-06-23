use std::path::{Path, PathBuf};

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
