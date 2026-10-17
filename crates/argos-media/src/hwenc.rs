//! Hardware H.264 encoding through a Media Foundation video encoder MFT.
//!
//! The software encoder spends 13.65 ms of CPU on a 1080p frame — measured on
//! synthetic content, which is the *cheap* case — and that single number is
//! behind most of the remaining work: it caps the frame rate, it is what makes
//! the quality ladder descend on a weak machine, and it competes with the
//! desktop itself for the same cores. A hardware encoder (Intel Quick Sync,
//! NVIDIA NVENC, AMD AMF) does the same job in a few milliseconds, behind one
//! API, and it is optional on exactly the machines that need it least.
//!
//! So this module is built around one rule: **every failure is a reason string,
//! never a broken stream.** A VM, an old GPU, a remote desktop session, a
//! driver with a broken MFT — all of those must still produce a working share,
//! just a slower one. The caller keeps `openh264` and reports which one it got.
//!
//! Two constraints are not negotiable, because the receiver depends on them:
//!
//! * **Base profile, no B-frames.** The SDP hardcodes `profile-level-id=42e01f`
//!   and both peers must be the same build, so anything above baseline would be
//!   a stream the far side cannot decode.
//! * **Annex B output.** The packetizer splits access units on start codes; a
//!   vendor that emits length-prefixed NAL units is normalised here rather than
//!   being made somebody else's problem.
//!
//! The input is the I420 planes the shared conversion already produced, so the
//! RGBA->YUV work is not done twice — the hardware path only adds a repack.

use std::collections::VecDeque;
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{IUnknown, Interface};
use windows::Win32::Foundation::TRUE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

/// Poll interval while waiting for the encoder to produce a frame.
///
/// One millisecond is a tenth of a frame at 60 fps, and the alternative — a
/// blocking `GetEvent` — has no upper bound, which would turn a wedged driver
/// into a share that has silently stopped sending.
const POLL: Duration = Duration::from_millis(1);

/// How long one frame may wait before the encoder is counted as stalled.
///
/// A healthy hardware encoder takes 3-6 ms. 200 ms is already six frames late;
/// anything past it is a broken encoder, not a slow one.
const STALL: Duration = Duration::from_millis(200);

/// Stalls tolerated before the hardware path gives up for good. A single late
/// frame is a driver hiccup; three in a row is a pattern, and the software
/// encoder is a known quantity.
const MAX_STALLS: u32 = 3;

/// The pixel formats an encoder MFT will accept, best first.
///
/// NV12 is what every vendor implements. The planar ones are worth taking when
/// they are offered, because the repack the hardware path needs is real work at
/// 1080p (~0.5 ms) and skipping it is free.
const INPUT_FORMATS: [(windows::core::GUID, &str); 4] = [
    (MFVideoFormat_NV12, "NV12"),
    (MFVideoFormat_I420, "I420"),
    (MFVideoFormat_IYUV, "IYUV"),
    (MFVideoFormat_YV12, "YV12"),
];

/// A hardware H.264 encoder, ready to take I420 frames.
pub struct HwEncoder {
    /// The vendor's own name, e.g. "Intel® Quick Sync Video H.264 Encoder".
    /// Goes on the pipeline readout so a report says which machine got what.
    name: String,
    transform: IMFTransform,
    /// Present only for asynchronous MFTs, which every hardware encoder is.
    events: Option<IMFMediaEventGenerator>,
    /// Kept so a resolution change can build a new transform: Media Foundation
    /// has no in-place size change for an encoder.
    activate: IMFActivate,
    input_format: windows::core::GUID,
    input_label: &'static str,
    dims: Option<(u32, u32)>,
    /// Presentation time for input samples, in 100 ns units. The transport
    /// takes its timestamps from the wall clock; these are only what the MFT
    /// needs to see monotonic values.
    clock: i64,
    frame_duration: i64,
    /// Encoded frames the MFT has taken but not yet returned. Non-empty only
    /// when the hardware pipelines one frame deeper than the caller does.
    pending: VecDeque<Vec<u8>>,
    stalls: u32,
    /// Set when a key frame was asked for and has not been seen on an output
    /// sample yet.
    want_keyframe: bool,
    fps: f32,
    bitrate: u32,
    /// The device the MFT reads its D3D manager from. Dropped last: the
    /// manager holds a handle into it.
    _device: ID3D11Device,
    manager: IMFDXGIDeviceManager,
}

// SAFETY: `IUnknown` inside `IMFTransform` is not `Send` because it is a raw
// pointer, but the COM apartment is MTA (`COINIT_MULTITHREADED`), which makes
// every interface on this thread safe to use from any other MTA thread. The
// refcount behind the pointer is interlocked (thread-safe). After
// `H264Encoder::attach_hardware` hands this to the encode worker, only that
// one thread touches it; nothing here is shared or cloned across threads.
unsafe impl Send for HwEncoder {}

impl HwEncoder {
    /// Finds a hardware H.264 encoder and prepares it.
    ///
    /// Called once, from the encode worker: COM and Media Foundation are
    /// initialized per thread, and the objects built here are used by that
    /// thread alone.
    pub fn new(fps: f32, bitrate: u32) -> Result<Self, String> {
        // `CoInitializeEx` answers S_FALSE when this thread's apartment is
        // already set up, and that still counts as a success that has to be
        // balanced — which is why the error arm is the only one that unwinds.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|error| {
                format!(
                    "COM is unavailable on this thread ({error}); that rules out hardware encoding"
                )
            })?;
        match Self::build(fps, bitrate) {
            Ok(encoder) => Ok(encoder),
            Err(reason) => {
                unsafe { CoUninitialize() };
                Err(reason)
            }
        }
    }

    fn build(fps: f32, bitrate: u32) -> Result<Self, String> {
        // Refcounted and process-wide. The matching shutdown in `Drop` keeps a
        // share session that starts and stops from leaking the platform.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(|error| format!("Media Foundation is unavailable ({error})"))?;
        let built = Self::build_inner(fps, bitrate);
        if built.is_err() {
            let _ = unsafe { MFShutdown() };
        }
        built
    }

    fn build_inner(fps: f32, bitrate: u32) -> Result<Self, String> {
        let activate = pick_encoder()?;
        let name = friendly_name(&activate);
        let transform: IMFTransform = unsafe { activate.ActivateObject() }
            .map_err(|error| format!("{name} would not start: {error}"))?;

        // Hardware MFTs are asynchronous: they post events instead of
        // returning output on the same call, and they refuse to run until the
        // client has promised to drain that queue.
        let events = transform
            .cast::<IMFMediaEventGenerator>()
            .map_err(|error| format!("{name} has no event queue: {error}"))?;
        let attributes = unsafe { transform.GetAttributes() }
            .map_err(|error| format!("{name} exposes no attributes: {error}"))?;
        if unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0 {
            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, TRUE.0 as u32) }
                .map_err(|error| format!("{name} could not be unlocked: {error}"))?;
        }

        // The D3D manager is what makes this a *hardware* transform rather
        // than a software one wearing the vendor's name, and it has to be set
        // before the media types are asked for — the list of input formats the
        // MFT offers depends on it.
        let (device, manager) = d3d_manager()?;
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
            .map_err(|error| format!("{name} rejected the D3D device: {error}"))?;

        let (input_format, input_label) = pick_input_format(&transform, &name)?;

        Ok(Self {
            name,
            transform,
            events: Some(events),
            activate,
            input_format,
            input_label,
            dims: None,
            clock: 0,
            frame_duration: frame_duration(fps),
            pending: VecDeque::new(),
            stalls: 0,
            want_keyframe: false,
            fps,
            bitrate,
            _device: device,
            manager,
        })
    }

    /// What the pipeline readout reports for this encoder.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Encodes one I420 frame.
    ///
    /// `Ok(None)` means the encoder has the frame but has not produced output
    /// yet — the caller sends nothing and the next frame carries the newer
    /// timestamp. `Err` means the hardware path has given up and the caller
    /// should fall back to software.
    pub fn encode(
        &mut self,
        yuv: &[u8],
        width: u32,
        height: u32,
        keyframe: bool,
    ) -> Result<Option<Vec<u8>>, String> {
        self.configure(width, height)?;
        self.push_input(yuv, width, height, keyframe)?;
        self.want_keyframe |= keyframe;
        let Some(sample) = self.wait_output()? else {
            return Ok(None);
        };
        let clean_point = sample_is_clean(&sample);
        let bytes = sample_bytes(&sample)?;
        if self.want_keyframe && !clean_point {
            // Asking is all this path can do. An encoder that ignores it would
            // leave a viewer waiting out the periodic keyframe interval after
            // every loss, which is the situation the software encoder does not
            // have — so the honest answer is to stop using it.
            return Err(format!(
                "{} ignored a keyframe request, so a lost frame would not be recoverable until the next \
                 periodic keyframe",
                self.name
            ));
        }
        self.want_keyframe = false;
        self.stalls = 0;
        Ok(Some(to_annex_b(&bytes)?))
    }

    /// Brings the transform up at the frame size it is about to be fed.
    fn configure(&mut self, width: u32, height: u32) -> Result<(), String> {
        if self.dims == Some((width, height)) {
            return Ok(());
        }
        if self.dims.is_some() {
            // The quality ladder moves the resolution mid-stream, and an
            // encoder MFT cannot be reconfigured in place: a new rung means a
            // new transform. Rare enough (once per ladder move) to be worth
            // the cost of doing properly.
            self.restart()?;
        }
        let media_types = self.media_types(width, height)?;
        // Output type before input type: an encoder decides what it can
        // produce from what it is being asked to produce, and several
        // hardware MFTs refuse an input type that has no output type yet.
        // The ones that want it the other way round get it the other way
        // round.
        if unsafe { self.transform.SetOutputType(0, &media_types.0, 0) }.is_err() {
            unsafe { self.transform.SetInputType(0, &media_types.1, 0) }.map_err(|error| {
                format!(
                    "{} rejected its {} input type: {error}",
                    self.name, self.input_label
                )
            })?;
            unsafe { self.transform.SetOutputType(0, &media_types.0, 0) }
                .map_err(|error| format!("{} rejected H.264 output: {error}", self.name))?;
        } else {
            unsafe { self.transform.SetInputType(0, &media_types.1, 0) }.map_err(|error| {
                format!(
                    "{} rejected its {} input type: {error}",
                    self.name, self.input_label
                )
            })?;
        }
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|error| format!("{} would not start streaming: {error}", self.name))?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|error| format!("{} would not start the stream: {error}", self.name))?;
        }
        self.dims = Some((width, height));
        self.pending.clear();
        self.clock = 0;
        Ok(())
    }

    /// Builds the output (H.264) and input (NV12 or planar) media types.
    fn media_types(&self, width: u32, height: u32) -> Result<(IMFMediaType, IMFMediaType), String> {
        let frames = self.fps.round().max(1.0) as u32;
        // Taken from the MFT's own list rather than built from scratch: a
        // hardware encoder publishes the output types it will actually
        // negotiate, and the first one is its preferred H.264 configuration.
        let output = unsafe { self.transform.GetOutputAvailableType(0, 0) }
            .map_err(|error| format!("{} offers no H.264 output type: {error}", self.name))?;
        unsafe {
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|_| output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
                .and_then(|_| set_pair(&output, &MF_MT_FRAME_SIZE, width, height))
                .and_then(|_| set_pair(&output, &MF_MT_FRAME_RATE, frames, 1))
                .and_then(|_| output.SetUINT32(&MF_MT_AVG_BITRATE, self.bitrate))
                .and_then(|_| {
                    output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                })
                // Baseline, and only baseline: the SDP advertises 42e01f and
                // the far side decodes with openh264.
                .and_then(|_| {
                    output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
                })
        }
        .map_err(|error| format!("{} rejected its H.264 output type: {error}", self.name))?;

        let input = unsafe { MFCreateMediaType() }
            .map_err(|error| format!("could not build an input media type: {error}"))?;
        unsafe {
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|_| input.SetGUID(&MF_MT_SUBTYPE, &self.input_format))
                .and_then(|_| set_pair(&input, &MF_MT_FRAME_SIZE, width, height))
                .and_then(|_| set_pair(&input, &MF_MT_FRAME_RATE, frames, 1))
                .and_then(|_| {
                    input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                })
                // Top-down, which is how the conversion above packs its rows.
                .and_then(|_| input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width as i32 as u32))
        }
        .map_err(|error| format!("{} rejected its input type: {error}", self.name))?;
        Ok((output, input))
    }

    /// Replaces the transform, keeping the platform and the D3D manager.
    fn restart(&mut self) -> Result<(), String> {
        let transform: IMFTransform = unsafe { self.activate.ActivateObject() }
            .map_err(|error| format!("{} would not restart: {error}", self.name))?;
        let events = transform
            .cast::<IMFMediaEventGenerator>()
            .map_err(|error| format!("{} has no event queue: {error}", self.name))?;
        if let Ok(attributes) = unsafe { transform.GetAttributes() } {
            if unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0 {
                let _ = unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, TRUE.0 as u32) };
            }
        }
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, self.manager.as_raw() as usize)
                .map_err(|error| format!("{} rejected the D3D device: {error}", self.name))?;
        }
        self.transform = transform;
        self.events = Some(events);
        self.dims = None;
        self.pending.clear();
        Ok(())
    }

    fn push_input(
        &mut self,
        yuv: &[u8],
        width: u32,
        height: u32,
        keyframe: bool,
    ) -> Result<(), String> {
        let length = (width * height) as usize * 3 / 2;
        let buffer = unsafe { MFCreateMemoryBuffer(length as u32) }
            .map_err(|error| format!("could not allocate an encoder buffer: {error}"))?;
        let mut data: *mut u8 = ptr::null_mut();
        let mut maximum = 0u32;
        unsafe { buffer.Lock(&mut data, Some(&mut maximum), None) }
            .map_err(|error| format!("could not lock the encoder buffer: {error}"))?;
        if data.is_null() || (maximum as usize) < length {
            let _ = unsafe { buffer.Unlock() };
            return Err("the encoder buffer was too small".to_string());
        }
        pack_input(
            yuv,
            width as usize,
            height as usize,
            self.input_format,
            unsafe { std::slice::from_raw_parts_mut(data, length) },
        )?;
        unsafe { buffer.Unlock() }
            .map_err(|error| format!("could not unlock the encoder buffer: {error}"))?;
        unsafe { buffer.SetCurrentLength(length as u32) }
            .map_err(|error| format!("could not size the encoder buffer: {error}"))?;

        let sample = unsafe { MFCreateSample() }
            .map_err(|error| format!("could not build an encoder sample: {error}"))?;
        unsafe { sample.AddBuffer(&buffer) }
            .map_err(|error| format!("could not hand the frame to the encoder: {error}"))?;
        unsafe { sample.SetSampleTime(self.clock) }
            .map_err(|error| format!("could not timestamp the frame: {error}"))?;
        self.clock += self.frame_duration;
        if keyframe {
            // The attribute every documented way of asking an encoder MFT for
            // a key frame goes through. It is verified on the output sample in
            // [`Self::encode`], because believing it without checking is how a
            // recovery mechanism turns into a four-second wait.
            unsafe { sample.SetUINT32(&MFSampleExtension_CleanPoint, TRUE.0 as u32) }
                .map_err(|error| format!("could not request a keyframe: {error}"))?;
        }
        unsafe { self.transform.ProcessInput(0, &sample, 0) }
            .map_err(|error| format!("{} would not take the frame: {error}", self.name))?;
        Ok(())
    }

    /// Waits for the next encoded frame, or gives up on the deadline.
    fn wait_output(&mut self) -> Result<Option<IMFSample>, String> {
        let deadline = Instant::now() + STALL;
        let mut last_error = String::new();
        while Instant::now() < deadline {
            // Draining is not optional. An asynchronous MFT posts one event
            // per frame for the life of the stream, and a queue nobody reads
            // eventually stops the encoder.
            let Some(events) = self.events.as_ref() else {
                return self.take_output();
            };
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    // A queued failure reaches the client here rather than as
                    // a silent stall, so it is worth propagating verbatim.
                    if let Err(error) = unsafe { event.GetStatus() } {
                        return Err(format!("{} reported {error}", self.name));
                    }
                    if unsafe { event.GetType() }
                        .is_ok_and(|kind| kind == METransformHaveOutput.0 as u32)
                    {
                        return self.take_output();
                    }
                    // NeedInput, DrainComplete and Marker carry nothing this
                    // one-frame-in, one-frame-out encoder can act on.
                }
                Err(error) => {
                    last_error = error.to_string();
                    thread::sleep(POLL);
                }
            }
        }
        self.stalls += 1;
        if self.stalls > MAX_STALLS {
            return Err(format!(
                "{} produced no frame within {} ms, three times running ({last_error})",
                self.name,
                STALL.as_millis()
            ));
        }
        Ok(None)
    }

    /// Pulls one frame out of the transform, if it has one ready.
    fn take_output(&mut self) -> Result<Option<IMFSample>, String> {
        let mut status = 0u32;
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default()];
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        // The sample is ours only on success; `ManuallyDrop` means it will not
        // be released for us either way.
        let sample = buffers[0].pSample.take();
        if let Err(error) = result {
            return Err(format!("{} could not produce a frame: {error}", self.name));
        }
        Ok(sample)
    }
}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        // The transform holds the D3D device, and Media Foundation is
        // refcounted per process, so both are wound down here rather than left
        // to whatever the process exit does with them.
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
        };
        let _ = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) };
        let _ = unsafe { self.activate.ShutdownObject() };
        let _ = unsafe { MFShutdown() };
        unsafe { CoUninitialize() };
    }
}

/// Asks the platform for a hardware H.264 encoder.
///
/// The output type filter is doing real work: `MFT_CATEGORY_VIDEO_ENCODER`
/// also holds HEVC and AV1 encoders, and a stream in either would be one the
/// far side's `openh264` decoder cannot open.
fn pick_encoder() -> Result<IMFActivate, String> {
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut raw: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output),
            &mut raw,
            &mut count,
        )
    }
    .map_err(|error| format!("could not list hardware encoders ({error})"))?;
    if raw.is_null() || count == 0 {
        unsafe { CoTaskMemFree(Some(raw.cast())) };
        return Err("this machine has no hardware H.264 encoder".to_string());
    }
    // The array and its references belong to the COM allocator; the chosen
    // element is cloned out and every reference is released before the block
    // is freed.
    let mut chosen: Option<IMFActivate> = None;
    for index in 0..count as usize {
        let slot = unsafe { &*raw.add(index) };
        if chosen.is_none() && slot.is_some() {
            chosen = slot.clone();
        }
        unsafe { *raw.add(index) = None };
    }
    unsafe { CoTaskMemFree(Some(raw.cast())) };
    chosen.ok_or_else(|| "this machine has no hardware H.264 encoder".to_string())
}

/// The vendor's name for its encoder, or a generic one.
fn friendly_name(activate: &IMFActivate) -> String {
    let mut text = windows::core::PWSTR::default();
    let mut length = 0u32;
    let fetched = unsafe {
        activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut length)
    };
    if fetched.is_err() {
        return "hardware H.264 encoder".to_string();
    }
    let name = unsafe { text.to_string() }.unwrap_or_else(|_| "hardware H.264 encoder".to_string());
    unsafe { CoTaskMemFree(Some(text.as_ptr().cast())) };
    name
}

/// The input format the encoder accepts, from the ones Argos can produce.
fn pick_input_format(
    transform: &IMFTransform,
    name: &str,
) -> Result<(windows::core::GUID, &'static str), String> {
    let mut offered: Vec<windows::core::GUID> = Vec::new();
    // A hardware encoder publishes a handful of input types and then answers
    // MF_E_NO_MORE_TYPES, which is an error code rather than an end marker.
    for index in 0..32 {
        let Ok(media_type) = (unsafe { transform.GetInputAvailableType(0, index) }) else {
            break;
        };
        if let Ok(subtype) = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) } {
            offered.push(subtype);
        }
    }
    INPUT_FORMATS
        .iter()
        .find(|(format, _)| offered.contains(format))
        .map(|(format, label)| (*format, *label))
        .ok_or_else(|| {
            format!(
                "{name} takes none of the formats Argos can produce (offered: {})",
                offered.len()
            )
        })
}

/// A D3D11 device and the Media Foundation manager that hands it to the MFT.
///
/// Its own device rather than the capture's: the capture device is used by
/// another thread, and an MFT that outlives a capture session must not take
/// that device down with it.
fn d3d_manager() -> Result<(ID3D11Device, IMFDXGIDeviceManager), String> {
    let mut device: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(
            None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(|error| format!("D3D11CreateDevice: {error}"))?;
    let device = device.ok_or_else(|| "D3D11CreateDevice returned no device".to_string())?;
    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
        .map_err(|error| format!("MFCreateDXGIDeviceManager: {error}"))?;
    let manager =
        manager.ok_or_else(|| "MFCreateDXGIDeviceManager returned no manager".to_string())?;
    let unknown: IUnknown = device
        .cast()
        .map_err(|error| format!("ID3D11Device is not an IUnknown: {error}"))?;
    unsafe { manager.ResetDevice(&unknown, token) }
        .map_err(|error| format!("the D3D device was rejected: {error}"))?;
    Ok((device, manager))
}

/// Writes an attribute holding two UINT32s, which is how Media Foundation
/// stores a frame size or a frame rate. The typed setters for these are not
/// part of the generated bindings, and a blob is the same thing on the wire.
fn set_pair(
    attributes: &IMFMediaType,
    key: *const windows::core::GUID,
    first: u32,
    second: u32,
) -> windows::core::Result<()> {
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&first.to_le_bytes());
    bytes[4..].copy_from_slice(&second.to_le_bytes());
    unsafe { attributes.SetBlob(key, &bytes) }
}

fn frame_duration(fps: f32) -> i64 {
    let frames = fps.round().max(1.0) as i64;
    10_000_000 / frames
}

/// Copies the shared conversion's I420 planes into the layout the encoder
/// asked for.
///
/// NV12 is the usual answer and is a repack rather than a conversion: the Y
/// plane is copied whole and the two chroma planes are interleaved. YV12 is
/// I420 with U and V exchanged, which is a swap of two same-sized halves.
pub fn pack_input(
    yuv: &[u8],
    width: usize,
    height: usize,
    format: windows::core::GUID,
    out: &mut [u8],
) -> Result<(), String> {
    let y_len = width * height;
    let uv_len = y_len / 4;
    if yuv.len() != y_len + uv_len * 2 || out.len() < y_len + uv_len * 2 {
        return Err(format!(
            "frame planes are {} bytes for {}x{}, which does not fit",
            yuv.len(),
            width,
            height
        ));
    }
    out[..y_len].copy_from_slice(&yuv[..y_len]);
    let (u, v) = yuv[y_len..].split_at(uv_len);
    if format == MFVideoFormat_NV12 {
        for index in 0..uv_len {
            out[y_len + index * 2] = u[index];
            out[y_len + index * 2 + 1] = v[index];
        }
    } else if format == MFVideoFormat_YV12 {
        for index in 0..uv_len {
            out[y_len + index * 2] = v[index];
            out[y_len + index * 2 + 1] = u[index];
        }
    } else {
        out[y_len..y_len + uv_len].copy_from_slice(u);
        out[y_len + uv_len..y_len + uv_len * 2].copy_from_slice(v);
    }
    Ok(())
}

/// Rewrites an encoder's output into the start-code form the rest of Argos
/// speaks.
///
/// Media Foundation does not pin the output layout down: the same encoder can
/// emit Annex B on one driver and length-prefixed NAL units on another. The
/// packetizer only understands the former, and this is a vendor quirk rather
/// than anything RTP should know about, so it is normalised here.
pub fn to_annex_b(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]) {
        return Ok(data.to_vec());
    }
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut position = 0usize;
    while position + 4 <= data.len() {
        // Big-endian, which is the length-prefix convention every container
        // that uses this layout follows (MP4/AVCC).
        let length = u32::from_be_bytes([
            data[position],
            data[position + 1],
            data[position + 2],
            data[position + 3],
        ]) as usize;
        position += 4;
        if length == 0 || position + length > data.len() {
            return Err(
                "the hardware encoder produced an output that is neither Annex B nor length-prefixed"
                    .to_string(),
            );
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[position..position + length]);
        position += length;
    }
    if position != data.len() {
        return Err("the hardware encoder produced a truncated access unit".to_string());
    }
    Ok(out)
}

/// Whether an output sample is a sync point, which is what a key frame request
/// was asking for.
fn sample_is_clean(sample: &IMFSample) -> bool {
    unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.is_ok_and(|value| value != 0)
}

/// Copies an encoded sample out of the transform.
fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, String> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|error| format!("could not read the encoded frame: {error}"))?;
    let mut data: *mut u8 = ptr::null_mut();
    let mut maximum = 0u32;
    let mut length = 0u32;
    unsafe { buffer.Lock(&mut data, Some(&mut maximum), Some(&mut length)) }
        .map_err(|error| format!("could not lock the encoded frame: {error}"))?;
    if data.is_null() {
        let _ = unsafe { buffer.Unlock() };
        return Err("the encoded frame was empty".to_string());
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec();
    unsafe { buffer.Unlock() }
        .map_err(|error| format!("could not unlock the encoded frame: {error}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{pack_input, to_annex_b, HwEncoder};
    use windows::Win32::Media::MediaFoundation::{MFVideoFormat_I420, MFVideoFormat_NV12};

    /// The NV12 repack is a layout change and nothing else: Y untouched,
    /// chroma interleaved. Any arithmetic in here would be a bug.
    #[test]
    fn nv12_repack_interleaves_chroma_and_keeps_luma() {
        let width = 4;
        let height = 2;
        let mut yuv = vec![0u8; width * height + width * height / 2];
        for (index, byte) in yuv.iter_mut().enumerate() {
            *byte = (index * 17 % 251) as u8;
        }
        let mut nv12 = vec![0u8; yuv.len()];
        pack_input(&yuv, width, height, MFVideoFormat_NV12, &mut nv12).expect("pack failed");
        let y_len = width * height;
        let uv_len = y_len / 4;
        assert_eq!(&nv12[..y_len], &yuv[..y_len], "luma must be copied whole");
        for index in 0..uv_len {
            assert_eq!(nv12[y_len + index * 2], yuv[y_len + index], "U");
            assert_eq!(
                nv12[y_len + index * 2 + 1],
                yuv[y_len + uv_len + index],
                "V"
            );
        }
    }

    /// I420 is already the layout the conversion produces, so it is a copy.
    #[test]
    fn planar_input_is_copied_unchanged() {
        let yuv: Vec<u8> = (0..96u8).collect();
        let mut out = vec![0u8; yuv.len()];
        pack_input(&yuv, 8, 8, MFVideoFormat_I420, &mut out).expect("pack failed");
        assert_eq!(out, yuv);
    }

    /// A frame that does not match its planes must fail loudly rather than
    /// read past the end.
    #[test]
    fn planar_input_rejects_a_buffer_of_the_wrong_size() {
        let yuv = vec![0u8; 10];
        let mut out = vec![0u8; 10];
        assert!(pack_input(&yuv, 4, 4, MFVideoFormat_NV12, &mut out).is_err());
    }

    /// The normaliser exists because the far side parses access units on start
    /// codes; both vendor layouts have to arrive as one of those.
    #[test]
    fn annex_b_is_passed_through_and_length_prefixed_is_rewritten() {
        let annex_b = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce];
        assert_eq!(to_annex_b(&annex_b).expect("annex b"), annex_b);

        let prefixed = [
            0, 0, 0, 2, 0x67, 0x42, // one NAL of two bytes
            0, 0, 0, 2, 0x68, 0xce, // and another
        ];
        assert_eq!(
            to_annex_b(&prefixed).expect("length prefixed"),
            annex_b,
            "start codes must be identical to what openh264 emits"
        );
    }

    #[test]
    fn annex_b_rejects_an_output_it_cannot_explain() {
        assert!(
            to_annex_b(&[0x67, 0x42, 0, 1]).is_err(),
            "no start code, no length"
        );
        assert!(
            to_annex_b(&[0, 0, 0, 9, 0x67, 0x42]).is_err(),
            "length overruns"
        );
    }

    /// The end-to-end check, and the only test that touches the FFI: whatever
    /// this machine's hardware encoder produces has to be something our own
    /// decoder can decode, at the size it was asked for.
    ///
    /// Ignored because it needs a GPU, a driver and an encoder MFT. On a
    /// machine without one it reports that and passes, so it is also the
    /// question "what does this box get?" — which is what the encoder row on
    /// the readout answers at run time.
    ///
    ///     cargo test -p argos-media --release -- --ignored --nocapture hardware_encoder
    #[test]
    #[ignore = "needs a machine with a hardware H.264 encoder"]
    fn hardware_encoder_round_trips_a_frame() {
        use crate::decode::H264Decoder;
        use argos_core::metrics::ReceiverMetrics;

        let mut encoder = match HwEncoder::new(30.0, 3_000_000) {
            Ok(encoder) => encoder,
            Err(reason) => {
                println!("no hardware encoder on this machine: {reason}");
                return;
            }
        };
        println!("hardware encoder: {}", encoder.name());

        let (width, height) = (320u32, 240u32);
        let grey = (width * height) as usize;
        let mut yuv = vec![0u8; grey + grey / 2];
        for (index, byte) in yuv.iter_mut().enumerate() {
            *byte = (index % 200) as u8;
        }

        let mut decoder = H264Decoder::new().expect("decoder");
        let metrics = ReceiverMetrics::default();
        let mut decoded = None;
        for frame in 0..3 {
            let Some(bitstream) = encoder
                .encode(&yuv, width, height, frame == 0)
                .expect("hardware encode")
            else {
                continue;
            };
            decoded = decoder
                .decode(&bitstream, &metrics)
                .expect("the far side's decoder rejected the hardware bitstream");
            if decoded.is_some() {
                break;
            }
        }
        let frame = decoded.expect("three frames of hardware output that decoded to nothing");
        assert_eq!((frame.width, frame.height), (width, height));
    }
}
