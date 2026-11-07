fn main() {
    println!("cargo:rerun-if-changed=assets/hx6zip.rc");
    println!("cargo:rerun-if-changed=assets/hx6zip.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var_os("CARGO_FEATURE_GUI").is_some()
    {
        let version = std::env::var("CARGO_PKG_VERSION").expect("missing package version");
        let parameters = [
            format!("HX6ZIP_VERSION_MAJOR={}", env!("CARGO_PKG_VERSION_MAJOR")),
            format!("HX6ZIP_VERSION_MINOR={}", env!("CARGO_PKG_VERSION_MINOR")),
            format!("HX6ZIP_VERSION_PATCH={}", env!("CARGO_PKG_VERSION_PATCH")),
            format!("HX6ZIP_VERSION_STRING=\"{version}\""),
        ];
        embed_resource::compile_for("assets/hx6zip.rc", ["hx6zip"], &parameters)
            .manifest_required()
            .expect("failed to embed the Hx6Zip application icon and version");
    }
}
