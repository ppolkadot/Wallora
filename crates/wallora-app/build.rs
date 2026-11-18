fn main() {
    println!("cargo:rerun-if-changed=wallora.manifest");
    println!("cargo:rerun-if-changed=assets/wallora.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/wallora.ico");
        res.set_manifest_file("wallora.manifest");
        if let Err(err) = res.compile() {
            panic!("failed to compile Windows resources: {err}");
        }
    }
}
