pub mod audio;
pub mod capture;
pub mod cursor;
pub mod decode;
#[cfg(windows)]
mod dxgi;
pub mod encode;
pub mod hwenc;
#[cfg(target_os = "linux")]
mod linux_capture;
