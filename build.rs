fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=patches");
    patch_crate::run().expect("Failed while patching");
    napi_build::setup();
}
