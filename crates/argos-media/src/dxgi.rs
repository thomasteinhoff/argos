//! Desktop Duplication plumbing.
//!
//! Kept separate from [`crate::capture`] so the session/recovery logic does not
//! have to know about COM. This module owns exactly one duplication at a time:
//! create the device, find the output whose monitor matches the requested name,
//! then pace acquires on a background thread.
//!
//! Two details are worth calling out because the old backend got them wrong:
//!
//! * `AcquireNextFrame` is given a *wait* timeout in milliseconds and is
//!   released on every path. A frame that is not going to be used is still
//!   released immediately, because holding one blocks the next acquire.
//! * The handoff to the session uses `try_send` on a depth-one channel. The
//!   recorder never waits on the consumer; a consumer that is behind simply
//!   loses frames, which the controller can then react to.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// use argos_core::lan::CursorUpdate; // kept for type compatibility in Control

use argos_core::metrics::SenderMetrics;
use windows::core::{Interface, BOOL, PCWSTR};
use windows::Win32::Foundation::{HMODULE, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW, ENUM_CURRENT_SETTINGS,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

use crate::capture::{Frame, MonitorInfo};

/// How long a single `AcquireNextFrame` may wait for a frame to appear. Capping
/// it keeps the idle poll cheap and the stop/shutdown latency bounded; a
/// present wakes the call early regardless.
const ACQUIRE_TIMEOUT_MILLIS: u32 = 16;
/// How long the recorder idles while the session is paused.
const INACTIVE_SLEEP: Duration = Duration::from_millis(20);

/// A live duplication. Owns the device it was created from; DXGI requires the
/// same device for the lifetime of the duplication.
pub struct Recorder {
    join: Option<JoinHandle<bool>>,
}

/// Session-owned state the recorder shares: teardown, pause, target cadence and
/// the metrics sink. Bundled so creating a recorder stays a three-argument call.
pub struct Control {
    pub stop: Arc<AtomicBool>,
    pub active: Arc<AtomicBool>,
    pub interval: Arc<AtomicU64>,
    pub metrics: Arc<SenderMetrics>,
    /// Cursor state published for forwarding to viewers (share side only).
    pub cursor: Arc<std::sync::Mutex<Option<argos_core::lan::CursorUpdate>>>,
}

impl Recorder {
    /// Creates a duplication for `name` and starts the capture thread.
    ///
    /// Device and duplication creation happen on the calling thread so that a
    /// missing monitor or an unavailable adapter is reported synchronously and
    /// can go through the session's normal recovery path.
    pub fn start(name: &str, tx: SyncSender<Frame>, control: Control) -> Result<Self, String> {
        let monitor =
            monitor_handle_for(name).ok_or_else(|| format!("monitor '{name}' not found"))?;
        let desktop =
            monitor_rect_for(name).ok_or_else(|| format!("monitor '{name}' has no geometry"))?;
        let (device, context, duplication) = create_duplication(monitor)?;
        let join = thread::Builder::new()
            .name("argos-dxgi".to_string())
            .spawn(move || run(device, context, duplication, tx, control, desktop))
            .map_err(|error| error.to_string())?;
        Ok(Self { join: Some(join) })
    }

    /// Waits for the capture thread to finish. Returns `true` if it exited on
    /// its own (access lost or a fatal device error) rather than because it was
    /// asked to stop. The session uses that to decide whether to reconnect.
    pub fn wait(&mut self) -> bool {
        self.join
            .take()
            .map(|join| join.join().unwrap_or(false))
            .unwrap_or(false)
    }
}

/// A staging texture reused across frames. Reallocating one per frame is the
/// GPU-side churn the old backend paid at capture rate.
struct Staging {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
}

fn run(
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    tx: SyncSender<Frame>,
    control: Control,
    desktop: RECT,
) -> bool {
    let Control {
        stop,
        active,
        interval,
        metrics,
        cursor,
    } = control;
    let mut shadow = crate::cursor::CursorShadow::new();
    let mut staging: Option<Staging> = None;
    // The very first acquire after a duplication is created reports
    // `LastPresentTime == 0` even though it carries the current desktop. Send it
    // so a newly-connected viewer gets a picture without waiting for a change.
    let mut first = true;
    let mut last = Instant::now() - Duration::from_secs(1);
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        if !active.load(Ordering::Relaxed) {
            thread::sleep(INACTIVE_SLEEP);
            last = Instant::now();
            continue;
        }
        if let Some(update) = shadow.poll(&desktop) {
            if let Ok(mut slot) = cursor.lock() {
                *slot = Some(update);
            }
        }
        let target = Duration::from_micros(interval.load(Ordering::Relaxed).max(1));
        let timeout = (target.as_millis().max(1) as u32).min(ACQUIRE_TIMEOUT_MILLIS);
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let waiting = Instant::now();
        let acquired = unsafe { duplication.AcquireNextFrame(timeout, &mut info, &mut resource) };
        match acquired {
            Ok(()) => {
                // The wait is over, so time it here rather than after the
                // readback. Folding the two together produced a number that was
                // neither the GPU's latency nor ours, and left `readback` — which
                // the pipeline readout displays — permanently zero.
                metrics.acquire.record(waiting.elapsed());
                let fresh = info.LastPresentTime != 0 || first;
                let due = last.elapsed() >= target;
                if fresh && due {
                    if let Some(resource) = resource.as_ref() {
                        let copying = Instant::now();
                        match readback(&device, &context, resource, &mut staging) {
                            Ok(frame) => {
                                metrics.readback.record(copying.elapsed());
                                first = false;
                                match tx.try_send(frame) {
                                    Ok(()) => metrics.captured.record(),
                                    // The consumer did not take the previous
                                    // frame in time. Counting it here is what
                                    // lets the controller see a sender that is
                                    // producing faster than it can hand off.
                                    Err(TrySendError::Full(_)) => metrics.captured.drop_frame(),
                                    Err(TrySendError::Disconnected(_)) => {
                                        let _ = unsafe { duplication.ReleaseFrame() };
                                        return false;
                                    }
                                }
                                last = Instant::now();
                            }
                            Err(_) => metrics.readback.record(copying.elapsed()),
                        }
                    }
                }
                // Always release: a held frame blocks the next acquire and is
                // the path to DXGI_ERROR_ACCESS_LOST.
                let _ = unsafe { duplication.ReleaseFrame() };
            }
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                metrics.acquire.record(waiting.elapsed());
            }
            Err(_) => {
                // Access lost (mode switch, secure desktop, GPU reset). The
                // session rebuilds the duplication; that is the documented
                // recovery, so report the death and let it.
                let _ = unsafe { duplication.ReleaseFrame() };
                return true;
            }
        }
    }
}

fn readback(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    resource: &IDXGIResource,
    staging: &mut Option<Staging>,
) -> Result<Frame, String> {
    unsafe {
        let source: ID3D11Texture2D = resource
            .cast()
            .map_err(|error| format!("ID3D11Texture2D: {error}"))?;
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        source.GetDesc(&mut source_desc);
        let width = source_desc.Width;
        let height = source_desc.Height;

        let needs_alloc = match staging.as_ref() {
            Some(staging) => {
                staging.width != width
                    || staging.height != height
                    || staging.format != source_desc.Format
            }
            None => true,
        };
        if needs_alloc {
            let descriptor = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: source_desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&descriptor, None, Some(&mut texture))
                .map_err(|error| format!("CreateTexture2D: {error}"))?;
            *staging = texture.map(|texture| Staging {
                texture,
                width,
                height,
                format: source_desc.Format,
            });
        }
        let staging = staging.as_ref().ok_or("staging texture missing")?;

        let destination: ID3D11Resource = staging
            .texture
            .cast()
            .map_err(|error| format!("ID3D11Resource: {error}"))?;
        let source_resource: ID3D11Resource = source
            .cast()
            .map_err(|error| format!("ID3D11Resource: {error}"))?;
        context.CopySubresourceRegion(&destination, 0, 0, 0, 0, &source_resource, 0, None);

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&destination, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|error| format!("Map: {error}"))?;

        if mapped.pData.is_null() || mapped.RowPitch == 0 {
            context.Unmap(&destination, 0);
            return Err("mapped staging texture was empty".to_string());
        }

        let row_bytes = width as usize * 4;
        let src = mapped.pData as *const u8;
        let row_pitch = mapped.RowPitch as usize;
        let mut rgba = Vec::with_capacity(row_bytes * height as usize);
        for row in 0..height as usize {
            let src_row = std::slice::from_raw_parts(src.add(row * row_pitch), row_bytes);
            for pixel in src_row.as_chunks::<4>().0 {
                // DXGI hands back BGRA; the encoder and over the wire format
                // expect RGBA (alpha is ignored but kept for completeness).
                rgba.push(pixel[2]);
                rgba.push(pixel[1]);
                rgba.push(pixel[0]);
                rgba.push(pixel[3]);
            }
        }
        context.Unmap(&destination, 0);

        Ok(Frame {
            width,
            height,
            rgba,
        })
    }
}

/// Enumerates the attached displays in the shape the picker expects.
pub fn list_monitors() -> Vec<MonitorInfo> {
    enumerate().into_iter().filter_map(monitor_info).collect()
}

/// Resolves a picker name (e.g. `\\.\DISPLAY1`) to the `HMONITOR` DXGI compares
/// its outputs against.
fn monitor_rect_for(name: &str) -> Option<RECT> {
    let handle = monitor_handle_for(name)?;
    unsafe {
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(handle, &mut info).as_bool() {
            return None;
        }
        Some(info.rcMonitor)
    }
}

fn monitor_handle_for(name: &str) -> Option<HMONITOR> {
    enumerate()
        .into_iter()
        .find(|handle| monitor_info(*handle).is_some_and(|info| info.name == name))
}

fn enumerate() -> Vec<HMONITOR> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitor_proc),
            LPARAM(&mut handles as *mut Vec<HMONITOR> as isize),
        );
    }
    handles
}

unsafe extern "system" fn enum_monitor_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let handles = &mut *(data.0 as *mut Vec<HMONITOR>);
    handles.push(monitor);
    BOOL(1)
}

fn monitor_info(monitor: HMONITOR) -> Option<MonitorInfo> {
    unsafe {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(monitor, &mut info.monitorInfo as *mut MONITORINFO).as_bool() {
            return None;
        }
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !EnumDisplaySettingsW(
            PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
        .as_bool()
        {
            return None;
        }
        Some(MonitorInfo {
            name: utf16_lossy(&info.szDevice),
            width: mode.dmPelsWidth,
            height: mode.dmPelsHeight,
            is_primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        })
    }
}

fn utf16_lossy(buffer: &[u16]) -> String {
    let len = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

type Duplication = (ID3D11Device, ID3D11DeviceContext, IDXGIOutputDuplication);

fn create_duplication(monitor: HMONITOR) -> Result<Duplication, String> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|error| format!("D3D11CreateDevice: {error}"))?;
        let device = device.ok_or_else(|| "D3D11CreateDevice returned no device".to_string())?;
        let context = context.ok_or_else(|| "D3D11CreateDevice returned no context".to_string())?;

        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|error| format!("IDXGIDevice: {error}"))?;
        let adapter: IDXGIAdapter = dxgi_device
            .GetAdapter()
            .map_err(|error| format!("GetAdapter: {error}"))?;

        let mut index = 0u32;
        loop {
            let output = match adapter.EnumOutputs(index) {
                Ok(output) => output,
                Err(_) => return Err(format!("no DXGI output for monitor {monitor:?}")),
            };
            index += 1;
            let description = output
                .GetDesc()
                .map_err(|error| format!("output description: {error}"))?;
            if description.Monitor != monitor {
                continue;
            }
            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|error| format!("IDXGIOutput1: {error}"))?;
            let duplication = output1
                .DuplicateOutput(&dxgi_device)
                .map_err(|error| format!("DuplicateOutput: {error}"))?;
            return Ok((device, context, duplication));
        }
    }
}
