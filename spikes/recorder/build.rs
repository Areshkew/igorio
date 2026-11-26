fn main() {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/recorder.manifest");
    println!("cargo:rerun-if-changed={manifest}");
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTINPUT:{manifest}");
}
