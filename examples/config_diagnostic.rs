//! Read-only diagnostic. Never prints credential or configuration values.
#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;

fn main() -> anyhow::Result<()> {
    println!("config_path={}", config::config_path().display());
    let saved = config::Config::load_with_notice()?.0;
    println!("has_base_url={}", !saved.base_url.trim().is_empty());
    println!("has_model={}", !saved.model.trim().is_empty());
    println!("loaded=true");
    Ok(())
}
