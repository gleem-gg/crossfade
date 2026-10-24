fn main() {
    glib_build_tools::compile_resources(
        &["data"],
        "data/gleem-crossfade.gresource.xml",
        "gleem-crossfade.gresource",
    );

    // Windows: embed the app icon and version information into the .exe.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

fn embed_windows_resources() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let template = manifest_dir.join("packaging/windows/gleem-crossfade.rc");
    let icon = manifest_dir.join("packaging/windows/gleem-crossfade.ico");
    println!("cargo:rerun-if-changed={}", template.display());
    println!("cargo:rerun-if-changed={}", icon.display());

    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<String> = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.parse::<u16>().unwrap_or(0).to_string())
        .collect();
    parts.resize(4, "0".to_string());

    let rc = std::fs::read_to_string(&template)
        .unwrap()
        .replace("@ICON@", &icon.display().to_string().replace('\\', "\\\\"))
        .replace("@VERSION_COMMA@", &parts.join(","))
        .replace("@VERSION@", &version);
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("gleem-crossfade.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
