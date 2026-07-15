fn main() {
    let config = lumen_platform::default_data_dir().join("context-browser/host.json");
    if let Err(error) = lumen_context::run_native_browser_host_with_config(Some(config)) {
        eprintln!("native browser host failed: {error}");
        std::process::exit(1);
    }
}
