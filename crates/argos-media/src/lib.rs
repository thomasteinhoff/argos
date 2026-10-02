pub mod audio;
#[cfg(feature = "capture-native")]
pub mod capture;
#[cfg(all(feature = "capture-xcap", not(feature = "capture-native")))]
#[path = "capture_xcap.rs"]
pub mod capture;
pub mod decode;
#[cfg(feature = "capture-native")]
mod dxgi;
pub mod encode;

#[cfg(not(any(feature = "capture-native", feature = "capture-xcap")))]
compile_error!("enable either the `capture-native` or the `capture-xcap` feature");
