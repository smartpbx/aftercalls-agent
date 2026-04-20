mod config;
mod pipeline;
mod recorder;
mod summary;
mod transcription;
mod vault;

use recorder::Recorder;
use std::path::PathBuf;
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
fn stop_recording(state: State<Recorder>, app: tauri::AppHandle) -> Result<String, String> {
    let path: PathBuf = state.stop()?;
    let session_dir = path.clone();
    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        pipeline::run(session_dir, app_clone).await;
    });
    Ok(path.to_string_lossy().into_owned())
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
