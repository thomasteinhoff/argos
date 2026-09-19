use std::path::PathBuf;
use std::process::Command;

const CANDIDATES: [&str; 2] = [
    r"C:\Program Files\Radmin VPN\Radmin VPN.exe",
    r"C:\Program Files (x86)\Radmin VPN\Radmin VPN.exe",
];

pub fn find_exe() -> Option<PathBuf> {
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

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
