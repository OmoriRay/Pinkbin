fn main() {
    let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    for relative in [
        "tauri.conf.json",
        "../package.json",
        "../../../package.json",
    ] {
        let path = manifest.join(relative);
        println!("cargo:rerun-if-changed={}", path.display());
        let content = std::fs::read_to_string(&path).expect("无法读取版本配置文件");
        let json: serde_json::Value =
            serde_json::from_str(&content).expect("版本配置文件不是有效 JSON");
        assert_eq!(
            json["version"].as_str(),
            Some(version.as_str()),
            "版本号与 Cargo workspace 不一致：{}",
            path.display()
        );
    }
    println!("cargo:rerun-if-changed=src");
    let build_date = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 60 * 60).unwrap())
        .format("%Y-%m-%d");
    println!("cargo:rustc-env=PINKBIN_BUILD_DATE={build_date}");
    tauri_build::build()
}
