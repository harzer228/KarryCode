fn main() {
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/karrycode.rc");
        println!("cargo:rerun-if-changed=assets/karrycode.ico");
        embed_resource::compile("assets/karrycode.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embedding the KarryCode application icon");
    }
}
