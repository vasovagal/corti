// Required: processes tauri.conf.json + generates the context env consumed by `tauri::generate_context!()`.
// Without this, generate_context!() fails to compile.
fn main() {
    tauri_build::build()
}
