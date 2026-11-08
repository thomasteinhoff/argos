//! Screen capture on Linux: Wayland (XDG Desktop Portal `zbus` + PipeWire) and
//! X11 (`x11rb` RandR + XProto + XFixes).

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argos_core::lan::CursorUpdate;
use x11rb::connection::Connection as X11Connection;
use x11rb::protocol::randr::ConnectionExt as RandrExt;
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
use x11rb::protocol::xproto::{ConnectionExt as XProtoExt, ImageFormat};
use zbus::blocking::{Connection as DbusConnection, MessageIterator};
use zbus::zvariant::{ObjectPath, OwnedFd, OwnedValue, Value};
use zbus::MatchRule;

use crate::capture::{due_at, Control, Frame, MonitorInfo};

const INACTIVE_SLEEP: Duration = Duration::from_millis(20);
const IDLE_POLL: Duration = Duration::from_millis(2);

pub struct Recorder {
    join: Option<JoinHandle<bool>>,
}

#[derive(Clone, Debug)]
struct LinuxMonitor {
    info: MonitorInfo,
    x: i16,
    y: i16,
}

impl Recorder {
    pub fn start(name: &str, tx: SyncSender<Frame>, control: Control) -> Result<Self, String> {
        let monitors = enumerate_monitors();
        let target = monitors
            .iter()
            .find(|m| m.info.name == name)
            .or_else(|| monitors.first())
            .cloned()
            .unwrap_or(LinuxMonitor {
                info: MonitorInfo {
                    name: name.to_string(),
                    width: 1920,
                    height: 1080,
                    is_primary: true,
                },
                x: 0,
                y: 0,
            });

        let is_wayland = std::env::var("XDG_SESSION_TYPE")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or_else(|_| std::env::var_os("WAYLAND_DISPLAY").is_some());

        if is_wayland {
            if let Ok(portal) = open_portal_stream(target.info.width, target.info.height) {
                let join = thread::Builder::new()
                    .name("argos-wayland-capture".to_string())
                    .spawn(move || run_portal(portal, tx, control))
                    .map_err(|e| e.to_string())?;
                return Ok(Self { join: Some(join) });
            }
        }

        // Fallback or primary X11 path via in-process x11rb
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| format!("X11/Wayland capture unavailable: {e}"))?;
        let _ = conn.xfixes_query_version(5, 0);
        let join = thread::Builder::new()
            .name("argos-x11-capture".to_string())
            .spawn(move || run_x11(conn, screen_num, target, tx, control))
            .map_err(|e| e.to_string())?;
        Ok(Self { join: Some(join) })
    }

    pub fn wait(&mut self) -> bool {
        self.join
            .take()
            .map(|join| join.join().unwrap_or(false))
            .unwrap_or(false)
    }
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    enumerate_monitors().into_iter().map(|m| m.info).collect()
}

fn enumerate_monitors() -> Vec<LinuxMonitor> {
    if let Ok((conn, screen_num)) = x11rb::connect(None) {
        if let Some(screen) = conn.setup().roots.get(screen_num) {
            let root = screen.root;
            if let Ok(cookie) = conn.randr_get_monitors(root, true) {
                if let Ok(reply) = cookie.reply() {
                    let mut out = Vec::new();
                    for info in reply.monitors {
                        if info.width == 0 || info.height == 0 {
                            continue;
                        }
                        let name = conn
                            .get_atom_name(info.name)
                            .ok()
                            .and_then(|c| c.reply().ok())
                            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                            .unwrap_or_else(|| format!("Display {}", out.len() + 1));
                        out.push(LinuxMonitor {
                            info: MonitorInfo {
                                name,
                                width: info.width as u32,
                                height: info.height as u32,
                                is_primary: info.primary,
                            },
                            x: info.x,
                            y: info.y,
                        });
                    }
                    if !out.is_empty() {
                        if !out.iter().any(|m| m.info.is_primary) {
                            out[0].info.is_primary = true;
                        }
                        return out;
                    }
                }
            }
        }
    }

    let drm = enumerate_drm_monitors();
    if !drm.is_empty() {
        return drm;
    }

    vec![LinuxMonitor {
        info: MonitorInfo {
            name: "Primary Display".to_string(),
            width: 1920,
            height: 1080,
            is_primary: true,
        },
        x: 0,
        y: 0,
    }]
}

fn enumerate_drm_monitors() -> Vec<LinuxMonitor> {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !dir_name.starts_with("card") || !dir_name.contains('-') {
            continue;
        }
        let status = fs::read_to_string(path.join("status")).unwrap_or_default();
        if status.trim() != "connected" {
            continue;
        }
        let modes = fs::read_to_string(path.join("modes")).unwrap_or_default();
        let (width, height) = modes
            .lines()
            .next()
            .and_then(parse_mode_dimensions)
            .unwrap_or((1920, 1080));
        let connector = dir_name
            .split_once('-')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_else(|| dir_name.to_string());
        out.push(LinuxMonitor {
            info: MonitorInfo {
                name: connector,
                width,
                height,
                is_primary: out.is_empty(),
            },
            x: 0,
            y: 0,
        });
    }
    out
}

fn parse_mode_dimensions(mode: &str) -> Option<(u32, u32)> {
    let trimmed = mode.trim();
    let (w, h_part) = trimmed.split_once('x')?;
    let h_digits: String = h_part.chars().take_while(|c| c.is_ascii_digit()).collect();
    let width: u32 = w.parse().ok()?;
    let height: u32 = h_digits.parse().ok()?;
    if width > 0 && height > 0 {
        Some((width, height))
    } else {
        None
    }
}

struct PortalStream {
    _dbus: DbusConnection,
    _pw_fd: OwnedFd,
    child: Child,
    width: u32,
    height: u32,
}

impl Drop for PortalStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn unique_token(prefix: &str, seq: u32) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{prefix}_{}_{nanos}_{seq}", std::process::id())
}

fn open_portal_stream(fallback_w: u32, fallback_h: u32) -> Result<PortalStream, String> {
    let conn = DbusConnection::session().map_err(|e| format!("D-Bus session error: {e}"))?;
    let unique = conn
        .unique_name()
        .ok_or_else(|| "D-Bus connection has no unique name".to_string())?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");

    let dest = "org.freedesktop.portal.Desktop";
    let path = "/org/freedesktop/portal/desktop";
    let iface = "org.freedesktop.portal.ScreenCast";

    // 1. CreateSession
    let req1 = unique_token("argos_req", 1);
    let sess_tok = unique_token("argos_sess", 1);
    let req1_path = format!("/org/freedesktop/portal/desktop/request/{unique}/{req1}");
    let iter1 = subscribe_portal_response(&conn, &req1_path)?;
    let mut opts1: HashMap<&str, Value<'_>> = HashMap::new();
    opts1.insert("handle_token", Value::from(req1.as_str()));
    opts1.insert("session_handle_token", Value::from(sess_tok.as_str()));
    conn.call_method(Some(dest), path, Some(iface), "CreateSession", &(opts1,))
        .map_err(|e| format!("ScreenCast.CreateSession failed: {e}"))?;
    let results1 = wait_portal_response(iter1)?;
    let session_path_str = results1
        .get("session_handle")
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_else(|| format!("/org/freedesktop/portal/desktop/session/{unique}/{sess_tok}"));
    let session_path =
        ObjectPath::try_from(session_path_str).map_err(|e| format!("invalid session path: {e}"))?;

    // 2. SelectSources (types=1 Monitor, cursor_mode=2 Embedded, persist_mode=2)
    let req2 = unique_token("argos_req", 2);
    let req2_path = format!("/org/freedesktop/portal/desktop/request/{unique}/{req2}");
    let iter2 = subscribe_portal_response(&conn, &req2_path)?;
    let mut opts2: HashMap<&str, Value<'_>> = HashMap::new();
    opts2.insert("handle_token", Value::from(req2.as_str()));
    opts2.insert("types", Value::from(1u32));
    opts2.insert("multiple", Value::from(false));
    opts2.insert("cursor_mode", Value::from(2u32));
    opts2.insert("persist_mode", Value::from(2u32));
    conn.call_method(
        Some(dest),
        path,
        Some(iface),
        "SelectSources",
        &(&session_path, opts2),
    )
    .map_err(|e| format!("ScreenCast.SelectSources failed: {e}"))?;
    let _ = wait_portal_response(iter2)?;

    // 3. Start
    let req3 = unique_token("argos_req", 3);
    let req3_path = format!("/org/freedesktop/portal/desktop/request/{unique}/{req3}");
    let iter3 = subscribe_portal_response(&conn, &req3_path)?;
    let mut opts3: HashMap<&str, Value<'_>> = HashMap::new();
    opts3.insert("handle_token", Value::from(req3.as_str()));
    conn.call_method(
        Some(dest),
        path,
        Some(iface),
        "Start",
        &(&session_path, "", opts3),
    )
    .map_err(|e| format!("ScreenCast.Start failed: {e}"))?;
    let results3 = wait_portal_response(iter3)?;
    let (node_id, stream_dims) = parse_portal_streams(&results3)?;
    let (width, height) = stream_dims.unwrap_or((fallback_w.max(64), fallback_h.max(64)));
    let width = (width & !1).max(64);
    let height = (height & !1).max(64);

    // 4. OpenPipeWireRemote
    let empty_opts: HashMap<&str, Value<'_>> = HashMap::new();
    let reply = conn
        .call_method(
            Some(dest),
            path,
            Some(iface),
            "OpenPipeWireRemote",
            &(&session_path, empty_opts),
        )
        .map_err(|e| format!("ScreenCast.OpenPipeWireRemote failed: {e}"))?;
    let pw_fd: OwnedFd = reply
        .body()
        .deserialize()
        .map_err(|e| format!("failed to read PipeWire fd: {e}"))?;
    let raw_fd = pw_fd.as_raw_fd();

    let mut cmd = Command::new("gst-launch-1.0");
    cmd.args([
        "-q",
        "pipewiresrc",
        &format!("fd={raw_fd}"),
        &format!("path={node_id}"),
        "do-timestamp=true",
        "!",
        "videoconvert",
        "!",
        "videoscale",
        "!",
        &format!("video/x-raw,format=RGBA,width={width},height={height}"),
        "!",
        "fdsink",
        "fd=1",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());

    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(raw_fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(raw_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn gst-launch-1.0 pipewiresrc: {e}"))?;

    Ok(PortalStream {
        _dbus: conn,
        _pw_fd: pw_fd,
        child,
        width,
        height,
    })
}

fn subscribe_portal_response(
    conn: &DbusConnection,
    request_path: &str,
) -> Result<MessageIterator, String> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")
        .map_err(|e| e.to_string())?
        .member("Response")
        .map_err(|e| e.to_string())?
        .path(request_path)
        .map_err(|e| e.to_string())?
        .build();
    MessageIterator::for_match_rule(rule, conn, Some(16)).map_err(|e| e.to_string())
}

fn wait_portal_response(mut iter: MessageIterator) -> Result<HashMap<String, OwnedValue>, String> {
    for msg in &mut iter {
        let msg = msg.map_err(|e| format!("portal signal error: {e}"))?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = msg
            .body()
            .deserialize()
            .map_err(|e| format!("portal response decode error: {e}"))?;
        if code != 0 {
            return Err(format!(
                "screen share permission denied or cancelled (code {code})"
            ));
        }
        return Ok(results);
    }
    Err("portal closed without a response".to_string())
}

fn parse_portal_streams(
    results: &HashMap<String, OwnedValue>,
) -> Result<(u32, Option<(u32, u32)>), String> {
    let streams_val = results
        .get("streams")
        .ok_or_else(|| "portal returned no streams".to_string())?;
    let streams: Vec<(u32, HashMap<String, OwnedValue>)> = streams_val
        .clone()
        .try_into()
        .map_err(|e| format!("invalid portal streams format: {e}"))?;
    let (node_id, props) = streams
        .into_iter()
        .next()
        .ok_or_else(|| "portal stream list was empty".to_string())?;
    let size = props
        .get("size")
        .and_then(|v| <(i32, i32)>::try_from(v.clone()).ok())
        .and_then(|(w, h)| (w > 0 && h > 0).then_some((w as u32, h as u32)));
    Ok((node_id, size))
}

fn run_portal(mut stream: PortalStream, tx: SyncSender<Frame>, control: Control) -> bool {
    let Control {
        stop,
        active,
        interval,
        metrics,
        cursor: _,
    } = control;
    let Some(mut stdout) = stream.child.stdout.take() else {
        return true;
    };
    let frame_bytes = (stream.width as usize) * (stream.height as usize) * 4;
    let mut next_due = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        if !active.load(Ordering::Relaxed) {
            thread::sleep(INACTIVE_SLEEP);
            continue;
        }
        let interval_micros = interval.load(Ordering::Relaxed).max(1);
        metrics.cadence_target.set(interval_micros);
        let target = Duration::from_micros(interval_micros);

        let waiting = Instant::now();
        let mut rgba = vec![0u8; frame_bytes];
        if stdout.read_exact(&mut rgba).is_err() {
            return !stop.load(Ordering::Relaxed);
        }
        metrics.acquire.record(waiting.elapsed());
        let presented = Instant::now();
        metrics.fresh.incr();

        if due_at(&mut next_due, presented, target) {
            metrics.readback.record(Duration::ZERO);
            let frame = Frame {
                width: stream.width,
                height: stream.height,
                rgba,
            };
            match tx.try_send(frame) {
                Ok(()) => metrics.captured.record(),
                Err(TrySendError::Full(_)) => metrics.captured.drop_frame(),
                Err(TrySendError::Disconnected(_)) => return false,
            }
        } else {
            metrics.paced.incr();
        }
    }
}

fn run_x11<C: X11Connection>(
    conn: C,
    screen_num: usize,
    monitor: LinuxMonitor,
    tx: SyncSender<Frame>,
    control: Control,
) -> bool {
    let Control {
        stop,
        active,
        interval,
        metrics,
        cursor,
    } = control;
    let Some(screen) = conn.setup().roots.get(screen_num) else {
        return true;
    };
    let root = screen.root;
    let width = (monitor.info.width & !1).max(2);
    let height = (monitor.info.height & !1).max(2);
    let mut next_due = Instant::now();
    let mut last_cursor_serial = 0u32;
    let mut generation = 0u32;
    let mut cached_cursor_rgba: Vec<u8> = Vec::new();
    let mut last_cursor_pos: Option<(i16, i16, bool)> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        if !active.load(Ordering::Relaxed) {
            thread::sleep(INACTIVE_SLEEP);
            continue;
        }

        poll_x11_cursor(
            &conn,
            &monitor,
            &mut last_cursor_serial,
            &mut generation,
            &mut cached_cursor_rgba,
            &mut last_cursor_pos,
            &cursor,
        );

        let interval_micros = interval.load(Ordering::Relaxed).max(1);
        metrics.cadence_target.set(interval_micros);
        let target = Duration::from_micros(interval_micros);
        let now = Instant::now();
        if !due_at(&mut next_due, now, target) {
            thread::sleep(IDLE_POLL);
            continue;
        }

        let started = Instant::now();
        let reply = conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                root,
                monitor.x,
                monitor.y,
                width as u16,
                height as u16,
                !0,
            )
            .ok()
            .and_then(|cookie| cookie.reply().ok());
        metrics.acquire.record(started.elapsed());

        let Some(image) = reply else {
            return !stop.load(Ordering::Relaxed);
        };
        metrics.fresh.incr();

        let copying = Instant::now();
        let pixel_count = (width as usize) * (height as usize);
        if image.data.len() < pixel_count * 4 {
            return true;
        }
        let mut rgba = Vec::with_capacity(pixel_count * 4);
        for px in image.data[..pixel_count * 4].as_chunks::<4>().0 {
            rgba.push(px[2]);
            rgba.push(px[1]);
            rgba.push(px[0]);
            rgba.push(255);
        }
        metrics.readback.record(copying.elapsed());

        let frame = Frame {
            width,
            height,
            rgba,
        };
        match tx.try_send(frame) {
            Ok(()) => metrics.captured.record(),
            Err(TrySendError::Full(_)) => metrics.captured.drop_frame(),
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn poll_x11_cursor<C: X11Connection>(
    conn: &C,
    monitor: &LinuxMonitor,
    last_serial: &mut u32,
    generation: &mut u32,
    cached_rgba: &mut Vec<u8>,
    last_pos: &mut Option<(i16, i16, bool)>,
    cursor_slot: &std::sync::Arc<std::sync::Mutex<Option<CursorUpdate>>>,
) {
    let Ok(cookie) = conn.xfixes_get_cursor_image() else {
        return;
    };
    let Ok(img) = cookie.reply() else {
        return;
    };
    if img.width == 0 || img.height == 0 {
        return;
    }
    let mon_w = monitor.info.width.max(1) as f32;
    let mon_h = monitor.info.height.max(1) as f32;
    let rel_x = (img.x - monitor.x) as f32;
    let rel_y = (img.y - monitor.y) as f32;
    let visible = rel_x >= 0.0 && rel_x < mon_w && rel_y >= 0.0 && rel_y < mon_h;

    let shape_changed = img.cursor_serial != *last_serial || cached_rgba.is_empty();
    if shape_changed {
        *last_serial = img.cursor_serial;
        *generation = generation.wrapping_add(1);
        cached_rgba.clear();
        cached_rgba.reserve(img.cursor_image.len() * 4);
        for argb in &img.cursor_image {
            let a = ((*argb >> 24) & 0xff) as u8;
            let r = ((*argb >> 16) & 0xff) as u8;
            let g = ((*argb >> 8) & 0xff) as u8;
            let b = (*argb & 0xff) as u8;
            cached_rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    let pos_tuple = (img.x, img.y, visible);
    if !shape_changed && *last_pos == Some(pos_tuple) {
        return;
    }
    *last_pos = Some(pos_tuple);

    let update = CursorUpdate {
        x: (rel_x / mon_w).clamp(0.0, 1.0),
        y: (rel_y / mon_h).clamp(0.0, 1.0),
        desktop_w: monitor.info.width.min(u16::MAX as u32) as u16,
        desktop_h: monitor.info.height.min(u16::MAX as u32) as u16,
        width: img.width,
        height: img.height,
        hotspot_x: img.xhot,
        hotspot_y: img.yhot,
        generation: *generation,
        visible,
        rgba: cached_rgba.clone(),
    };
    if let Ok(mut slot) = cursor_slot.lock() {
        *slot = Some(update);
    }
}

#[cfg(test)]
mod tests {
    use super::parse_mode_dimensions;

    #[test]
    fn parses_sysfs_drm_mode_strings() {
        assert_eq!(parse_mode_dimensions("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_mode_dimensions("2560x1440p\n"), Some((2560, 1440)));
        assert_eq!(parse_mode_dimensions("invalid"), None);
    }
}
