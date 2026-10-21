use std::path::PathBuf;
use std::process::Command;

// The install path and process image name below are the VPN tool's own on-disk
// names — they must match reality or nothing gets found or launched. They are
// deliberately never echoed into any user-visible string.
const CANDIDATES: [&str; 2] = [
    r"C:\Program Files\Radmin VPN\Radmin VPN.exe",
    r"C:\Program Files (x86)\Radmin VPN\Radmin VPN.exe",
];
const IMAGE_NAME: &str = "Radmin VPN.exe";

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
    let exe = find_exe().ok_or_else(|| "No VPN tool is installed".to_string())?;
    Command::new(exe)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn is_running() -> bool {
    let filter = format!("IMAGENAME eq {IMAGE_NAME}");
    let output = match Command::new("tasklist")
        .args(["/FI", &filter, "/NH"])
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
    text.contains(&IMAGE_NAME.to_lowercase())
}
