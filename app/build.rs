fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/main.slint", config).expect("failed to compile Slint UI");

    // The icon Explorer shows for bardo.exe; the window icon is set in the UI.
    println!("cargo:rerun-if-changed=assets/bardo.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/bardo.ico")
            .set("FileDescription", "bardo, a voice-only Discord client")
            .set("ProductName", "bardo")
            .set("OriginalFilename", "bardo.exe")
            .set("LegalCopyright", "MIT License")
            .compile()
            .expect("failed to embed the Windows resources");
    }
}
