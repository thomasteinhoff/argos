//! Embeds `assets/icon.ico` into the executable as its Win32 icon resource.
//!
//! The icon set at runtime in `main.rs` covers the title bar and the running
//! taskbar entry, but Explorer, a pinned shortcut, and the taskbar before the
//! first frame all read the icon out of the PE file itself, and only a resource
//! can put it there. `winresource` drives the SDK's `rc.exe`, which the MSVC
//! build already requires — this adds no new tool.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    #[cfg(windows)]
    {
        winresource::WindowsResource::new()
            .set_icon("assets/icon.ico")
            .compile()
            .expect("failed to embed assets/icon.ico");
    }
}
