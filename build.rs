// Build script.
//
// In headless mode (default) it is a no-op. Under the `desktop` feature it
// delegates to `tauri_build`, which merges `tauri.conf.json` + `capabilities/`
// and emits the cfg flags that `tauri::generate_context!()` relies on at runtime.
fn main() {
    #[cfg(feature = "desktop")]
    tauri_build::build()
}
