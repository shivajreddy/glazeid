fn main() {
    // In release builds on Windows, set the PE subsystem to WINDOWS so the
    // executable launches without a console window.  Debug builds keep the
    // console so that `cargo run` shows output and Ctrl+C works normally.
    #[cfg(target_os = "windows")]
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        println!("cargo:rustc-link-arg=/SUBSYSTEM:WINDOWS");
        println!("cargo:rustc-link-arg=/ENTRY:mainCRTStartup");
    }
}
