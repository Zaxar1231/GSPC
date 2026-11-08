//! Экспорт `NvOptimusEnablement` и `AmdPowerXpressRequestHighPerformance` из exe: по ним
//! драйверы ноутбуков с двумя GPU выбирают дискретную видеокарту.

fn main() {
    let windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|v| v == "windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|v| v == "msvc");
    if windows && msvc {
        println!("cargo:rustc-link-arg-bins=/EXPORT:NvOptimusEnablement,DATA");
        println!("cargo:rustc-link-arg-bins=/EXPORT:AmdPowerXpressRequestHighPerformance,DATA");
    }
}
