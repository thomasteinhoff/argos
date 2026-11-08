use std::path::PathBuf;
#[cfg(windows)]
use std::process::Command;

#[cfg(windows)]
const CANDIDATES: [&str; 2] = [
    r"C:\Program Files\Radmin VPN\Radmin VPN.exe",
    r"C:\Program Files (x86)\Radmin VPN\Radmin VPN.exe",
];

#[cfg(windows)]
pub fn find_exe() -> Option<PathBuf> {
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

#[cfg(not(windows))]
pub fn find_exe() -> Option<PathBuf> {
    None
}

#[cfg(windows)]
pub fn launch() -> Result<(), String> {
    if is_running() {
        return Ok(());
    }
    let exe = find_exe().ok_or_else(|| "Radmin VPN is not installed".to_string())?;
    Command::new(exe)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn launch() -> Result<(), String> {
    Err("Radmin VPN is only supported on Windows".to_string())
}

#[cfg(windows)]
fn is_running() -> bool {
    let output = match Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Radmin VPN.exe", "/NH"])
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
    text.contains("radmin vpn")
}
