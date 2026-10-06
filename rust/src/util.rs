use std::path::PathBuf;

/// Directory the executable lives in (settings.json / webview-data live here,
/// so a copied-out release exe stays self-contained).
pub fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Host part of a URL without pulling in a full url crate.
pub fn domain_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host.split('@').next_back().unwrap_or("").to_string()
}

/// Timestamp a log line with local time (std has no local-time support).
pub fn stamp(msg: &str) -> String {
    format!("[{}]  {}", now_hms(), msg)
}

fn now_hms() -> String {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::SYSTEMTIME;
        use windows::Win32::System::SystemInformation::GetLocalTime;
        let st: SYSTEMTIME = unsafe { GetLocalTime() };
        format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
    }
    #[cfg(not(windows))]
    {
        "00:00:00".to_string()
    }
}
