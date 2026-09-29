fn main() {
    println!("cargo:rerun-if-changed=icons/beat.rc");
    println!("cargo:rerun-if-changed=icons/beat.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // The egui window icon is set at runtime; Explorer reads the icon
        // stored in the executable's Windows resources instead. Regenerate it
        // with `cargo run --example gen_icons`.
        if std::path::Path::new("icons/beat.ico").exists() {
            embed_resource::compile_for("icons/beat.rc", ["beat"], embed_resource::NONE)
                .manifest_optional()
                .expect("failed to embed the BEAT executable icon");
        } else {
            println!("cargo:warning=icons/beat.ico missing, run `cargo run --example gen_icons`");
        }
    }
}
