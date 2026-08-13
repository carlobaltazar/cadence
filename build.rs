fn main() {
    // Resource embedding requires windres (GNU) or rc.exe (MSVC/Windows SDK).
    // Only attempt if the tool is available; skip gracefully otherwise.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    let mut res = winres::WindowsResource::new();
    res.set_manifest_file("assets/cadence.manifest");

    if std::path::Path::new("assets/cadence.ico").exists() {
        res.set_icon("assets/cadence.ico");
    }

    res.set("ProductName", "Cadence");
    res.set("FileDescription", "Cadence");
    res.set("LegalCopyright", "Copyright (c) 2026");

    // FILEVERSION/PRODUCTVERSION are four packed 16-bit fields. Derive them from
    // Cargo.toml so the exe's Properties dialog can't drift from the version the
    // updater and the dashboard report.
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let mut parts = ver.split('.').map(|p| {
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0)
    });
    let (major, minor, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    let packed = (major << 48) | (minor << 32) | (patch << 16);
    res.set_version_info(winres::VersionInfo::PRODUCTVERSION, packed);
    res.set_version_info(winres::VersionInfo::FILEVERSION, packed);

    res.compile().expect(
        "Failed to embed Windows resources. \
         The manifest is required for Per-Monitor V2 DPI awareness — without it, \
         HP-monitor pixel sampling silently breaks on >100% display scaling. \
         Install MinGW (windres) or the Windows SDK (rc.exe) and rebuild.",
    );

    // winres links the resource as `-lresource`, but GNU ld only pulls archive members
    // that resolve an undefined symbol — a resource object has none, so windows-gnu
    // builds silently shipped with NO resources at all (no version info and, worse, no
    // DPI manifest; only the runtime SetProcessDpiAwarenessContext fallback in main.rs
    // kept pixel sampling working). Passing the object file directly makes it
    // undroppable. MSVC (CI) links resources correctly and doesn't need this.
    if std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "gnu" {
        println!(
            "cargo:rustc-link-arg-bins={}\\resource.o",
            std::env::var("OUT_DIR").unwrap()
        );
    }
}
