//! Compiles the Slint UI and, for Windows targets, embeds the application
//! manifest, the icons and the version information in the executable.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    // The Windows look on every host, so the UI previewed elsewhere matches.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/main.slint", config).expect("the Slint UI compiles");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

/// Writes a resource script for `res/audiosync.exe.manifest`, the icons in
/// `res/` and the version information, and has it linked into the executable.
fn embed_windows_resources() {
    let res = cargo_path("CARGO_MANIFEST_DIR").join("res");
    // The resource compiler reads a backslash in a string as an escape.
    let resource = |name: &str| {
        let path = res.join(name);
        println!("cargo::rerun-if-changed={}", path.display());
        path.to_str()
            .expect("the resource path is UTF-8")
            .replace('\\', "/")
    };
    let manifest = resource("audiosync.exe.manifest");
    // Icon 1 is the app icon Explorer shows; 2 to 5 are the tray icons
    // (src/tray.rs): for a light and a dark taskbar, streaming and off.
    let icons = [
        "audiosync.ico",
        "tray-light.ico",
        "tray-dark.ico",
        "tray-light-off.ico",
        "tray-dark-off.ico",
    ]
    .iter()
    .enumerate()
    .map(|(index, name)| format!("{} ICON \"{}\"\n", index + 1, resource(name)))
    .collect::<String>();

    let version = cargo_var("CARGO_PKG_VERSION");
    let numeric_version = format!(
        "{},{},{},0",
        cargo_var("CARGO_PKG_VERSION_MAJOR"),
        cargo_var("CARGO_PKG_VERSION_MINOR"),
        cargo_var("CARGO_PKG_VERSION_PATCH"),
    );

    // 24 is RT_MANIFEST, and resource 1 is the manifest Windows reads when it
    // starts the process. FILEOS 0x40004 is VOS_NT_WINDOWS32, FILETYPE 0x1 is
    // VFT_APP, and 0x409/1200 (block 040904B0) is US English in UTF-16.
    let script = format!(
        r#"1 24 "{manifest}"

{icons}
1 VERSIONINFO
FILEVERSION {numeric_version}
PRODUCTVERSION {numeric_version}
FILEFLAGSMASK 0x3F
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "FileDescription", "AudioSync"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "audiosync"
            VALUE "OriginalFilename", "audiosync.exe"
            VALUE "ProductName", "AudioSync"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    );
    let script_path = cargo_path("OUT_DIR").join("audiosync.rc");
    fs::write(&script_path, script).expect("the resource script is written");

    let result = embed_resource::compile(&script_path, embed_resource::NONE);
    // A Windows host builds what ships, so the manifest must be in. Other
    // hosts can only check the code (they have no Windows linker), so a
    // missing resource compiler is fine there.
    let embedded = if cfg!(windows) {
        result.manifest_required()
    } else {
        result.manifest_optional()
    };
    if let Err(error) = embedded {
        panic!("the Windows resources did not compile: {error}");
    }
}

fn cargo_var(key: &str) -> String {
    env::var(key).unwrap_or_else(|_| panic!("Cargo sets {key}"))
}

fn cargo_path(key: &str) -> PathBuf {
    PathBuf::from(cargo_var(key))
}
