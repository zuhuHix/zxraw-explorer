use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/zxraw.ico");

    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let icon_path = fs::canonicalize("../../assets/zxraw.ico")
        .expect("the generated Windows icon must exist before building");
    let icon_path = icon_path.to_string_lossy().replace('\\', "\\\\");

    let major = env::var("CARGO_PKG_VERSION_MAJOR").expect("Cargo package major version");
    let minor = env::var("CARGO_PKG_VERSION_MINOR").expect("Cargo package minor version");
    let patch = env::var("CARGO_PKG_VERSION_PATCH").expect("Cargo package patch version");
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let version_info = format!("{major},{minor},{patch},0");

    let resource = format!(
        r#"
1 ICON "{icon_path}"

1 VERSIONINFO
 FILEVERSION {version_info}
 PRODUCTVERSION {version_info}
 FILEOS 0x00040004
 FILETYPE 0x00000001
 FILESUBTYPE 0x00000000
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "ZXraw Explorer contributors\0"
      VALUE "FileDescription", "ZXraw Explorer\0"
      VALUE "FileVersion", "{version}\0"
      VALUE "InternalName", "zxraw\0"
      VALUE "OriginalFilename", "zxraw.exe\0"
      VALUE "ProductName", "ZXraw Explorer\0"
      VALUE "ProductVersion", "{version}\0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 1200
  END
END
"#
    );
    let resource_path = out_dir.join("zxraw.rc");
    fs::write(&resource_path, resource).expect("write Windows resource script");
    println!("cargo:rerun-if-changed={}", resource_path.display());

    embed_resource::compile(resource_path, embed_resource::NONE)
        .manifest_optional()
        .expect("compile Windows icon and version resources");
}
