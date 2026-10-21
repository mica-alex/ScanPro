fn main() {
    // Pick the widget style that matches the host OS look. Slint's `-light`/
    // `-dark`-less style names follow the system colour scheme at runtime.
    let style = match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") | Ok("ios") => "cupertino",
        _ => "fluent",
    };
    let style = std::env::var("SCANPRO_SLINT_STYLE").unwrap_or_else(|_| style.to_string());
    println!("cargo:rerun-if-env-changed=SCANPRO_SLINT_STYLE");
    let config = slint_build::CompilerConfiguration::new().with_style(style);
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI failed to compile");
}
