mod recorder;

use recorder::Recorder;
use tauri::{Manager, State};

#[tauri::command]
fn start_recording(state: State<Recorder>, app: tauri::AppHandle) -> Result<String, String> {
    let base = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings");
    state
        .start(base)
        .map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn stop_recording(state: State<Recorder>) -> Result<String, String> {
    state.stop().map(|p| p.to_string_lossy().into_owned())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(Recorder::new())
        .invoke_handler(tauri::generate_handler![start_recording, stop_recording])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
