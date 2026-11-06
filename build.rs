fn main() {
    println!("cargo:rerun-if-changed=assets/mzip.rc");
    println!("cargo:rerun-if-changed=assets/mzip.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var_os("CARGO_FEATURE_GUI").is_some()
    {
        let version = std::env::var("CARGO_PKG_VERSION").expect("missing package version");
        let parameters = [
            format!("MZIP_VERSION_MAJOR={}", env!("CARGO_PKG_VERSION_MAJOR")),
            format!("MZIP_VERSION_MINOR={}", env!("CARGO_PKG_VERSION_MINOR")),
            format!("MZIP_VERSION_PATCH={}", env!("CARGO_PKG_VERSION_PATCH")),
            format!("MZIP_VERSION_STRING=\"{version}\""),
        ];
        embed_resource::compile_for("assets/mzip.rc", ["mzip"], &parameters)
            .manifest_required()
            .expect("failed to embed the MZip application icon and version");
    }
}
