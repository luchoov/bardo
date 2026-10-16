fn main() {
    // Opus's CMake build compiles for the DLL C runtime and imports `floor`. With the static runtime
    // the linker still resolves that, but only once something else has pulled `floor` in, which the
    // small examples don't.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-examples=/INCLUDE:floor");
    }
}
