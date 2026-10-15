fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/main.slint", config).expect("failed to compile Slint UI");

    // The icon Explorer shows for bardo.exe; the window icon is set in the UI.
    println!("cargo:rerun-if-changed=assets/bardo.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/bardo.ico")
            .set("FileDescription", "bardo")
            .set("ProductName", "bardo")
            .compile()
            .expect("failed to embed the Windows resources");
    }
}
