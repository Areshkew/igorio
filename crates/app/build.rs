fn main() -> std::io::Result<()> {
    const ICON: &str = "../../assets/igorio.ico";
    println!("cargo:rerun-if-changed=igorio.manifest");
    println!("cargo:rerun-if-changed={ICON}");

    let manifest = include_str!("igorio.manifest");
    let manifest = if cfg!(feature = "ui-access") {
        manifest.replace("uiAccess=\"false\"", "uiAccess=\"true\"")
    } else {
        manifest.to_owned()
    };
    winresource::WindowsResource::new()
        .set_manifest(&manifest)
        .set_icon(ICON)
        .set("ProductName", "Igorio")
        .set("FileDescription", "Igorio")
        .set("OriginalFilename", "igorio.exe")
        .compile_for("igorio")
}
