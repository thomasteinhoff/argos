//! The share machine's pointer, captured with GDI.
//!
//! The cursor cannot travel inside the video stream: moving the mouse over a
//! static desktop produces no new DXGI frame, so a pointer composited into the
//! capture would freeze in place. Instead this module polls [`GetCursorInfo`]
//! at the capture thread's existing cadence (about 60 Hz even while idle) and
//! publishes a normalized position plus a rasterized shape for the app to ship
//! to viewers over the LAN channel.
//!
//! The shape is rasterized the classic way: `GetIconInfo` hands back the
//! icon's color and mask bitmaps as copies, `GetDIBits` reads them top-down (a
//! negative `biHeight` is honored for the return order), and the AND plane
//! decides which pixels are transparent. Monochrome cursors (the standard
//! arrow, I-beam, resize grips) have no color bitmap; their mask is twice as
//! tall, an XOR plane above an AND plane, spelled out in [`decode_mono`].

use argos_core::lan::CursorUpdate;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::GetIconInfo;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, CURSORINFO, CURSOR_SHOWING, HCURSOR, ICONINFO,
};

#[derive(Clone)]
struct RasterShape {
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    rgba: Vec<u8>,
}

#[derive(Default)]
pub struct CursorShadow {
    fingerprint: isize,
    generation: u32,
    shape: Option<RasterShape>,
    last: Option<CursorUpdate>,
}

impl CursorShadow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Polls the pointer once. Returns a complete update (position, visibility
    /// and the current shape) only when something a viewer could see changed.
    ///
    /// `desktop` is the shared monitor's rectangle in screen coordinates,
    /// which the cursor's own screen-space position is compared against; the
    /// normalized fractions that come out of it are scale-invariant, so they
    /// survive DPI differences between the two machines.
    pub fn poll(&mut self, desktop: &RECT) -> Option<CursorUpdate> {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            if GetCursorInfo(&mut info).is_err() {
                return None;
            }
        }
        let left = desktop.left as f32;
        let top = desktop.top as f32;
        let width = (desktop.right - desktop.left).max(1) as f32;
        let height = (desktop.bottom - desktop.top).max(1) as f32;
        let x = info.ptScreenPos.x as f32;
        let y = info.ptScreenPos.y as f32;
        let on_desktop = x >= left && x < left + width && y >= top && y < top + height;
        let showing = (info.flags.0) & (CURSOR_SHOWING.0) != 0;
        let visible = showing && on_desktop;

        let fingerprint = info.hCursor.0 as isize;
        if fingerprint != self.fingerprint {
            self.shape = raster_shape(info.hCursor);
            self.fingerprint = fingerprint;
            if self.shape.is_some() {
                self.generation = self.generation.wrapping_add(1);
            }
        }
        // A cursor that could not be rasterized (an exotic shape, or the flag
        // flapped) keeps whatever we had; a gap per frame beats a blank stream.
        let Some(shape) = &self.shape else {
            return None;
        };

        let candidate = CursorUpdate {
            x: (x - left) / width,
            y: (y - top) / height,
            desktop_w: (desktop.right - desktop.left).max(1) as u16,
            desktop_h: (desktop.bottom - desktop.top).max(1) as u16,
            width: shape.width,
            height: shape.height,
            hotspot_x: shape.hotspot_x,
            hotspot_y: shape.hotspot_y,
            generation: self.generation,
            visible,
            // Always carried in the slot: the app re-ships the shape on a
            // keepalive, and needs it complete even when this poll only moved.
            rgba: shape.rgba.clone(),
        };
        let changed = self.last.as_ref().is_none_or(|last| {
            last.x != candidate.x
                || last.y != candidate.y
                || last.visible != candidate.visible
                || last.generation != candidate.generation
        });
        if !changed {
            return None;
        }
        self.last = Some(candidate.clone());
        Some(candidate)
    }
}

/// Rasterizes an icon handle into its RGBA pixels and hotspot.
fn raster_shape(cursor: HCURSOR) -> Option<RasterShape> {
    unsafe {
        let mut icon = ICONINFO::default();
        if GetIconInfo(cursor.into(), &mut icon).is_err() {
            return None;
        }
        let result = (|| {
            let mask = bitmap_info(icon.hbmMask)?;
            if mask.bmWidth <= 0 || mask.bmHeight <= 0 {
                return None;
            }
            let width = mask.bmWidth as u32;
            let hotspot = (icon.xHotspot as u16, icon.yHotspot as u16);
            let color = bitmap_info(icon.hbmColor);
            let (rgba, height) = match color {
                Some(color) if color.bmWidth > 0 && color.bmHeight > 0 => (
                    read_color(icon.hbmColor, &color, icon.hbmMask, &mask, width)?,
                    color.bmHeight as u32,
                ),
                _ => mono_from_mask(icon.hbmMask, &mask, width)?,
            };
            Some(RasterShape {
                width: width as u16,
                height: height as u16,
                hotspot_x: hotspot.0,
                hotspot_y: hotspot.1,
                rgba,
            })
        })();
        let _ = DeleteObject(icon.hbmMask.into());
        let _ = DeleteObject(icon.hbmColor.into());
        result
    }
}

fn bitmap_info(bitmap: HBITMAP) -> Option<BITMAP> {
    let mut info = std::mem::MaybeUninit::<BITMAP>::zeroed();
    let copied = unsafe {
        GetObjectW(
            bitmap.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(info.as_mut_ptr().cast()),
        )
    };
    if copied == 0 {
        return None;
    }
    Some(unsafe { info.assume_init() })
}

/// Reads a color cursor's pixels (BGRA from the color bitmap, transparency
/// from the AND plane) as RGBA.
fn read_color(
    hbm_color: HBITMAP,
    color: &BITMAP,
    hbm_mask: HBITMAP,
    _mask: &BITMAP,
    width: u32,
) -> Option<Vec<u8>> {
    let height = color.bmHeight as u32;
    let pixels = (width * height) as usize;
    let mut bgra = vec![0u8; pixels * 4];
    read_bits(hbm_color, 32, width, height, &mut bgra)?;
    let mut and = vec![0u8; plane_bytes(width, height)];
    read_bits(hbm_mask, 1, width, height, &mut and)?;
    let mut out = vec![0u8; pixels * 4];
    blend_color(&bgra, &and, width, height, &mut out);
    Some(out)
}

/// Reads a monochrome cursor's mask (top half XOR, bottom half AND) as RGBA.
fn mono_from_mask(hbm_mask: HBITMAP, mask: &BITMAP, width: u32) -> Option<(Vec<u8>, u32)> {
    let height = (mask.bmHeight as u32) / 2;
    if height == 0 {
        return None;
    }
    let rows = plane_bytes(width, height);
    let mut plane = vec![0u8; rows * 2];
    read_bits(hbm_mask, 1, width, height * 2, &mut plane)?;
    let mut out = vec![0u8; (width * height * 4) as usize];
    decode_mono(&plane[..rows], &plane[rows..], width, height, &mut out);
    Some((out, height))
}

/// Reads `height` top-down rows of a `bit_count` bits-per-pixel bitmap into
/// `out` (DWORD-padded rows, exactly as [`GetDIBits`] lays them out).
fn read_bits(hbm: HBITMAP, bit_count: u16, width: u32, height: u32, out: &mut [u8]) -> Option<()> {
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: bit_count,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let copied = GetDIBits(
            hdc,
            hbm,
            0,
            height,
            Some(out.as_mut_ptr().cast()),
            &mut bmi as *mut BITMAPINFO,
            DIB_RGB_COLORS,
        );
        let _ = ReleaseDC(None, hdc);
        if copied == 0 {
            return None;
        }
        Some(())
    }
}

/// Bytes in one DWORD-aligned row of a 1-bit-per-pixel plane.
fn plane_bytes(width: u32, rows: u32) -> usize {
    (width.div_ceil(32) * 4) as usize * rows as usize
}

/// Whether the 1bpp bit at `(x, y)` is set in a top-down, DWORD-padded plane.
fn plane_bit(plane: &[u8], width: u32, x: u32, y: u32) -> bool {
    let stride = width.div_ceil(32) * 4;
    let byte = plane[(y * stride + x / 8) as usize];
    byte & (1 << (7 - (x % 8))) != 0
}

/// Monochrome cursor pixels: where the AND plane is set the pixel shows the
/// desktop (transparent); elsewhere the XOR plane gives white or black.
///
/// This is the two-tone decode most screen-sharing tools ship. True XOR
/// semantics would invert against whatever the viewer draws underneath,
/// which a fixed stream cannot know.
fn decode_mono(xor: &[u8], and: &[u8], width: u32, height: u32, out: &mut [u8]) {
    for y in 0..height {
        for x in 0..width {
            let p = ((y * width + x) * 4) as usize;
            if plane_bit(and, width, x, y) {
                out[p..p + 4].copy_from_slice(&[0, 0, 0, 0]);
            } else if plane_bit(xor, width, x, y) {
                out[p..p + 4].copy_from_slice(&[255, 255, 255, 255]);
            } else {
                out[p..p + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
}

/// Color cursor pixels: BGRA source with the AND plane as the alpha channel.
fn blend_color(bgra: &[u8], and: &[u8], width: u32, height: u32, out: &mut [u8]) {
    for y in 0..height {
        for x in 0..width {
            let p = ((y * width + x) * 4) as usize;
            if plane_bit(and, width, x, y) {
                out[p..p + 4].copy_from_slice(&[0, 0, 0, 0]);
            } else {
                out[p] = bgra[p + 2];
                out[p + 1] = bgra[p + 1];
                out[p + 2] = bgra[p];
                out[p + 3] = 255;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1bpp plane where every bit is 0 except a single set bit. Row padding
    /// is DWORD-aligned, as GetDIBits produces it.
    fn plane(touched_x: u32, touched_y: u32, width: u32, height: u32) -> Vec<u8> {
        let mut plane = vec![0u8; plane_bytes(width, height)];
        let stride = width.div_ceil(32) * 4;
        let index = (touched_y * stride + touched_x / 8) as usize;
        plane[index] |= 1 << (7 - (touched_x % 8));
        plane
    }

    #[test]
    fn mono_decode_white_black_and_transparent() {
        let width = 8;
        let height = 2;
        // XOR plane: bit (3,0) set (white). AND plane: bit (1,1) set (transparent).
        let xor = plane(3, 0, width, height);
        let and = plane(1, 1, width, height);
        let mut out = vec![0u8; (width * height * 4) as usize];
        decode_mono(&xor, &and, width, height, &mut out);
        assert_eq!(&out[12..16], &[255, 255, 255, 255]); // white at (3,0)
        assert_eq!(&out[0..4], &[0, 0, 0, 255]); // black elsewhere in row 0
                                                 // pixel (1,1) is index 1*width+1 = 9
        assert_eq!(&out[9 * 4..9 * 4 + 4], &[0, 0, 0, 0]); // transparent at (1,1)
        assert_eq!(&out[10 * 4..10 * 4 + 4], &[0, 0, 0, 255]); // full plane, row 1
    }

    #[test]
    fn mono_decode_honours_row_padding() {
        // A 5-wide plane pads each row to 8 bytes; a set bit in the second row
        // must land in the second row's block, not the first row's padding.
        let width = 5;
        let height = 2;
        let xor = plane(0, 0, width, height);
        let and = plane(4, 1, width, height);
        let mut out = vec![0u8; (width * height * 4) as usize];
        decode_mono(&xor, &and, width, height, &mut out);
        assert_eq!(&out[0..4], &[255, 255, 255, 255]);
        // (4,1) transparent, so row 1 pixel 4 has alpha 0; (0,1) is black.
        let idx = (width as usize + 4) * 4;
        assert_eq!(&out[idx..idx + 4], &[0, 0, 0, 0]);
        let idx0 = width as usize * 4;
        assert_eq!(&out[idx0..idx0 + 4], &[0, 0, 0, 255]);
    }

    #[test]
    fn color_blend_swaps_channels_and_uses_the_plane_for_alpha() {
        let width = 2;
        let height = 1;
        // BGRA source: blue pixel 0, green pixel 1.
        let bgra = [255, 0, 0, 0, 0, 255, 0, 0];
        let and = plane(1, 0, width, height);
        let mut out = vec![0u8; (width * height * 4) as usize];
        blend_color(&bgra, &and, width, height, &mut out);
        assert_eq!(&out[0..4], &[0, 0, 255, 255]); // RGBA blue, opaque
        assert_eq!(&out[4..8], &[0, 0, 0, 0]); // (1,0) masked transparent
    }
}
