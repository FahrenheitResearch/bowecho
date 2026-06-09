fn main() {
    // Windows: embed VERSIONINFO metadata + the app icon. Proper resources
    // make the exe look like the legitimate desktop app it is (fewer Defender
    // / SmartScreen heuristic flags than a bare, metadata-less binary).
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("bowecho.ico");
        let mut resource = winresource::WindowsResource::new();
        if icon.exists() {
            resource.set_icon(&icon.to_string_lossy());
        }
        resource.set("ProductName", "BowEcho");
        resource.set(
            "FileDescription",
            "BowEcho — fast NEXRAD Level II radar viewer",
        );
        resource.set("CompanyName", "Fahrenheit Research");
        resource.set(
            "LegalCopyright",
            "Copyright (c) 2026 Fahrenheit Research. MIT OR Apache-2.0.",
        );
        resource.set("OriginalFilename", "bowecho.exe");
        resource.set("InternalName", "bowecho");
        if let Err(error) = resource.compile() {
            // Non-fatal: a missing rc.exe should not break source builds.
            println!("cargo:warning=windows resource embedding skipped: {error}");
        }
    }
}
