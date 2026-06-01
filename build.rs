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
    res.set_version_info(winres::VersionInfo::PRODUCTVERSION, 0x0003_0000_0000_0000);
    res.set_version_info(winres::VersionInfo::FILEVERSION, 0x0003_0000_0000_0000);

    res.compile().expect(
        "Failed to embed Windows resources. \
         The manifest is required for Per-Monitor V2 DPI awareness — without it, \
         HP-monitor pixel sampling silently breaks on >100% display scaling. \
         Install MinGW (windres) or the Windows SDK (rc.exe) and rebuild.",
    );
}
