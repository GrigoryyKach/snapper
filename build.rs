// Embeds the icon (assets/icon.ico), the DPI manifest (app.manifest) and version info into Snapper.exe,
// so Explorer and the taskbar show "Snapper" with its own icon.
// MSVC toolchain: needs rc.exe from the Windows SDK; GNU toolchain: windres from MinGW.
// If the tool is missing the build continues without resources.

use std::path::{Path, PathBuf};

fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = std::env::var_os("RC") {
        return Some(rc.into());
    }
    let arch = if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") { "arm64" } else { "x64" };
    for base in [r"C:\Program Files (x86)\Windows Kits\10\bin", r"C:\Program Files\Windows Kits\10\bin"] {
        let Ok(rd) = std::fs::read_dir(base) else { continue };
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs.into_iter().rev() {
            let rc = d.join(arch).join("rc.exe");
            if rc.exists() {
                return Some(rc);
            }
        }
    }
    None
}

/// The GNU (MinGW) toolchain adds its own default manifest (`default-manifest.o`), which collides with
/// ours ("multiple non-default manifests") and may drop it. GCC links that object only if it finds it in
/// its search paths, so an empty object with the same name is put first via `-B`. Not used on MSVC (CI).
fn gnu_skip_default_manifest() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu") || std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("no-default-manifest");
    let _ = std::fs::create_dir_all(&out);
    let src = out.join("empty.c");
    let obj = out.join("default-manifest.o");
    std::fs::write(&src, "/* empty: replaces default-manifest.o from MinGW */
").unwrap();
    let ok = std::process::Command::new("gcc").arg("-c").arg(&src).arg("-o").arg(&obj).status().is_ok_and(|s| s.success());
    if ok {
        println!("cargo:rustc-link-arg=-B{}", out.display());
    } else {
        println!("cargo:warning=gcc not found: MinGW will add its own manifest");
    }
}

fn main() {
    gnu_skip_default_manifest();
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets").join("icon.ico");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("app.manifest");

    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut nums: Vec<u32> = version.split('.').filter_map(|p| p.parse().ok()).collect();
    nums.resize(4, 0);
    let commas = nums.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let rc_text = format!(
        r#"1 ICON "{ico}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "041904B0"
    BEGIN
      VALUE "FileDescription", "Snapper"
      VALUE "ProductName", "Snapper"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "Snapper.exe"
      VALUE "InternalName", "Snapper"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x419, 1200
  END
END
"#,
        ico = ico.display().to_string().replace('\\', "\\\\"),
        manifest = manifest.display().to_string().replace('\\', "\\\\"),
    );
    let rc_path = out.join("app.rc");
    std::fs::write(&rc_path, rc_text).unwrap();
    if msvc {
        let res = out.join("app.res");
        let Some(rc) = find_rc() else {
            println!("cargo:warning=rc.exe not found, the exe will be built without an icon and version info");
            return;
        };
        let ok = std::process::Command::new(&rc).args(["/nologo", "/fo"]).arg(&res).arg(&rc_path).status().is_ok_and(|s| s.success());
        if ok && Path::new(&res).exists() {
            println!("cargo:rustc-link-arg-bins={}", res.display());
        } else {
            println!("cargo:warning=rc.exe failed to build the resources, the exe will have no icon");
        }
    } else {
        let obj = out.join("app_res.o");
        let windres = std::env::var("WINDRES").unwrap_or_else(|_| "windres".into());
        let ok = std::process::Command::new(&windres)
            .arg("--input")
            .arg(&rc_path)
            .arg("--output")
            .arg(&obj)
            .args(["--input-format=rc", "--output-format=coff"])
            .status()
            .is_ok_and(|s| s.success());
        if ok && Path::new(&obj).exists() {
            println!("cargo:rustc-link-arg-bins={}", obj.display());
        } else {
            println!("cargo:warning=windres not found or failed, the exe will be built without an icon and version info");
        }
    }
}
