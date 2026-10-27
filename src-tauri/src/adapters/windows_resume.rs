/// Maps the portable event-loop resume signal into the application boundary.
/// The desktop Windows build never receives it — Tauri emits `Resumed` from the
/// Android and iOS backends only — so Windows lifecycle handling lives in
/// `adapters::windows_power`, which listens for the broadcast messages that
/// Windows actually sends.
#[cfg(target_os = "windows")]
pub fn is_resume_event(event: &tauri::RunEvent) -> bool {
    matches!(event, tauri::RunEvent::Resumed)
}

#[cfg(not(target_os = "windows"))]
pub fn is_resume_event(_event: &tauri::RunEvent) -> bool {
    false
}
