//! USB host with the UVC class driver: one MJPEG stream from a USB camera.
//!
//! The stream runs only on demand: a snapshot request starts it, the frames
//! after a short warm-up are kept as "latest frame", and an idle timer stops
//! the stream when nobody has asked for a picture for a while. Attach and
//! detach are handled without restarting anything else.

use adapter_core::uvc::{self, VcInfo};
use anyhow::{anyhow, Result};
use esp_idf_svc::sys::{self, usb};
use log::{info, warn};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Frames dropped after the stream starts, while exposure settles.
const WARMUP_FRAMES: u32 = 2;
/// The stream is stopped this long after the last snapshot request.
const IDLE_STOP: Duration = Duration::from_secs(10);
/// Longest wait for one frame (covers stream start and the warm-up).
const FRAME_TIMEOUT: Duration = Duration::from_secs(8);
const FRAME_BUFFERS: i32 = 2;
/// Frame size the camera falls back to when none is configured.
const DEFAULT_RESOLUTION: (u16, u16) = (640, 480);

/// One entry of the camera's frame descriptor list.
#[derive(Clone, Debug)]
pub struct FrameInfo {
    pub mjpeg: bool,
    pub width: u16,
    pub height: u16,
    pub default_fps: f32,
    /// Discrete frame rates, or `min..max` for a continuous range.
    pub rates: String,
}

impl FrameInfo {
    fn from_raw(f: &usb::uvc_host_frame_info_t) -> Self {
        let fps = |interval: u32| {
            if interval == 0 {
                0.0
            } else {
                10_000_000.0 / interval as f32
            }
        };
        let rates = unsafe {
            if f.interval_type == 0 {
                let r = &f.__bindgen_anon_1.__bindgen_anon_1;
                format!("{:.0}..{:.0}", fps(r.interval_max), fps(r.interval_min))
            } else {
                let n = (f.interval_type as usize).min(f.__bindgen_anon_1.interval.len());
                f.__bindgen_anon_1.interval[..n]
                    .iter()
                    .map(|&i| format!("{:.0}", fps(i)))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        FrameInfo {
            mjpeg: f.format == usb::uvc_host_stream_format_UVC_VS_FORMAT_MJPEG,
            width: f.h_res as u16,
            height: f.v_res as u16,
            default_fps: fps(f.default_interval),
            rates,
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} {}x{} @ {} fps (default {:.0})",
            if self.mjpeg { "MJPEG" } else { "other" },
            self.width,
            self.height,
            self.rates,
            self.default_fps
        )
    }
}

/// Picks the MJPEG format to open: the configured size if the camera offers
/// it, else the one closest in pixel count to it (or to 640x480).
pub fn choose_format(formats: &[FrameInfo], wanted: Option<(u16, u16)>) -> Option<FrameInfo> {
    let mjpeg: Vec<&FrameInfo> = formats.iter().filter(|f| f.mjpeg).collect();
    if let Some((w, h)) = wanted {
        if let Some(f) = mjpeg.iter().find(|f| f.width == w && f.height == h) {
            return Some((*f).clone());
        }
    }
    let (tw, th) = wanted.unwrap_or(DEFAULT_RESOLUTION);
    let target = tw as i64 * th as i64;
    mjpeg
        .into_iter()
        .min_by_key(|f| (f.width as i64 * f.height as i64 - target).abs())
        .cloned()
}

/// The camera announced by the driver, before its stream is opened.
struct Connected {
    dev_addr: u8,
    stream_index: u8,
    formats: Vec<FrameInfo>,
}

/// Raw stream handle; the driver is thread-safe for the calls we make.
#[derive(Clone, Copy)]
struct Handle(usb::uvc_host_stream_hdl_t);
unsafe impl Send for Handle {}

/// The open stream and who is using it. The disconnect callback runs on the
/// driver task and must not close a handle another thread is calling into,
/// so a close requested while `in_use` is left for that thread to perform.
struct DevState {
    handle: Option<Handle>,
    in_use: bool,
    pending_close: bool,
    running: bool,
    /// Frames received since the stream was last started.
    frames: u32,
    /// Negotiated format, for the status line.
    format: Option<(u16, u16, f32)>,
}

struct Frame {
    data: Vec<u8>,
    /// Incremented for every frame stored; 0 before the first one.
    seq: u64,
    /// Index of the stored frame within the current stream run.
    index: u32,
}

struct Inner {
    dev: Mutex<DevState>,
    frame: Mutex<Frame>,
    frame_cv: Condvar,
    connected: Mutex<Option<Connected>>,
    connected_cv: Condvar,
    attached: AtomicBool,
    last_request: Mutex<Instant>,
    overflows: AtomicU32,
    wanted: Option<(u16, u16)>,
    wanted_fps: Option<u16>,
    /// VideoControl interface and units of the attached camera.
    vc: Mutex<Option<VcInfo>>,
    /// Query parameters last applied successfully; re-applied when the
    /// camera is plugged in again (UVC settings do not survive that).
    last_settings: Mutex<Vec<(String, String)>>,
}

#[derive(Clone)]
pub struct Camera {
    inner: Arc<Inner>,
}

// Shared with the C callbacks (there is exactly one USB host instance).
static INSTANCE: Mutex<Option<Arc<Inner>>> = Mutex::new(None);

fn instance() -> Option<Arc<Inner>> {
    INSTANCE.lock().unwrap().clone()
}

fn esp_check(rc: sys::esp_err_t, what: &str) -> Result<()> {
    if rc == sys::ESP_OK {
        Ok(())
    } else {
        Err(anyhow!("{what} failed: esp_err {rc}"))
    }
}

/// Driver event: a UVC function was enumerated. Fetch its frame list here so
/// the open thread and the shell can show it.
unsafe extern "C" fn driver_event_cb(
    event: *const usb::uvc_host_driver_event_data_t,
    _user: *mut c_void,
) {
    let ev = &*event;
    if ev.type_ != usb::uvc_host_driver_event_UVC_HOST_DRIVER_EVENT_DEVICE_CONNECTED {
        return;
    }
    let c = ev.__bindgen_anon_1.device_connected;
    info!(
        "camera: device connected, address {} stream {} ({} formats)",
        c.dev_addr, c.uvc_stream_index, c.frame_info_num
    );
    let mut raw: Vec<usb::uvc_host_frame_info_t> = Vec::new();
    let mut n = c.frame_info_num;
    if n > 0 {
        raw.resize_with(n, || core::mem::zeroed());
        let rc = usb::uvc_host_get_frame_list(
            c.dev_addr,
            c.uvc_stream_index,
            raw.as_mut_ptr() as *mut [usb::uvc_host_frame_info_t; 0],
            &mut n,
        );
        if rc != sys::ESP_OK {
            warn!("camera: frame list unavailable ({rc})");
            n = 0;
        }
    }
    let formats: Vec<FrameInfo> = raw[..n.min(raw.len())]
        .iter()
        .map(FrameInfo::from_raw)
        .collect();
    for f in &formats {
        info!("camera: format {}", f.describe());
    }
    if let Some(inner) = instance() {
        // Only the first UVC function of the first camera is used.
        let mut conn = inner.connected.lock().unwrap();
        if conn.is_none() {
            *conn = Some(Connected {
                dev_addr: c.dev_addr,
                stream_index: c.uvc_stream_index,
                formats,
            });
            inner.connected_cv.notify_all();
        } else {
            warn!("camera: ignoring an additional UVC function");
        }
    }
}

/// Stream events, on the driver task.
unsafe extern "C" fn stream_event_cb(
    event: *const usb::uvc_host_stream_event_data_t,
    _user: *mut c_void,
) {
    let ev = &*event;
    let Some(inner) = instance() else { return };
    match ev.type_ {
        usb::uvc_host_dev_event_UVC_HOST_DEVICE_DISCONNECTED => {
            info!("camera: disconnected");
            inner.attached.store(false, Ordering::SeqCst);
            *inner.connected.lock().unwrap() = None;
            *inner.vc.lock().unwrap() = None;
            let mut d = inner.dev.lock().unwrap();
            d.running = false;
            d.format = None;
            if d.in_use {
                // A driver call is in flight on this handle: its thread closes it.
                d.pending_close = true;
            } else if let Some(h) = d.handle.take() {
                drop(d);
                usb::uvc_host_stream_close(h.0);
            }
            inner.frame_cv.notify_all();
        }
        usb::uvc_host_dev_event_UVC_HOST_TRANSFER_ERROR => {
            warn!("camera: transfer error {}", ev.__bindgen_anon_1.transfer_error.error)
        }
        usb::uvc_host_dev_event_UVC_HOST_FRAME_BUFFER_OVERFLOW => {
            let n = inner.overflows.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 50 == 0 {
                warn!("camera: frame larger than its buffer, dropped ({n} so far); lower the resolution");
            }
        }
        usb::uvc_host_dev_event_UVC_HOST_FRAME_BUFFER_UNDERFLOW => {
            log::debug!("camera: no free frame buffer, frame dropped")
        }
        _ => {}
    }
}

/// A complete frame, on the driver task. Copied out and returned at once.
unsafe extern "C" fn frame_cb(frame: *const usb::uvc_host_frame_t, _user: *mut c_void) -> bool {
    let f = &*frame;
    let Some(inner) = instance() else { return true };
    let index = {
        let mut d = inner.dev.lock().unwrap();
        d.frames = d.frames.saturating_add(1);
        d.frames - 1
    };
    if index < WARMUP_FRAMES || f.data_len < 4 {
        return true;
    }
    let data = std::slice::from_raw_parts(f.data, f.data_len);
    let mut fr = inner.frame.lock().unwrap();
    fr.data.clear();
    fr.data.extend_from_slice(data);
    fr.seq += 1;
    fr.index = index;
    inner.frame_cv.notify_all();
    true
}

impl Camera {
    /// Installs the USB host and the UVC driver and starts the thread that
    /// opens the camera's stream whenever one is attached.
    pub fn start(resolution: Option<(u16, u16)>, fps: Option<u16>) -> Result<Self> {
        let inner = Arc::new(Inner {
            dev: Mutex::new(DevState {
                handle: None,
                in_use: false,
                pending_close: false,
                running: false,
                frames: 0,
                format: None,
            }),
            frame: Mutex::new(Frame {
                data: Vec::new(),
                seq: 0,
                index: 0,
            }),
            frame_cv: Condvar::new(),
            connected: Mutex::new(None),
            connected_cv: Condvar::new(),
            attached: AtomicBool::new(false),
            last_request: Mutex::new(Instant::now()),
            overflows: AtomicU32::new(0),
            wanted: resolution,
            wanted_fps: fps,
            vc: Mutex::new(None),
            last_settings: Mutex::new(Vec::new()),
        });
        *INSTANCE.lock().unwrap() = Some(inner.clone());

        unsafe {
            let mut host_cfg: usb::usb_host_config_t = core::mem::zeroed();
            host_cfg.intr_flags = sys::ESP_INTR_FLAG_LEVEL1 as i32;
            esp_check(usb::usb_host_install(&host_cfg), "usb_host_install")?;
        }
        // USB host library event pump.
        std::thread::Builder::new()
            .name("usb-events".into())
            .stack_size(4 * 1024)
            .spawn(|| unsafe {
                loop {
                    let mut flags: u32 = 0;
                    usb::usb_host_lib_handle_events(u32::MAX, &mut flags);
                    if flags & usb::USB_HOST_LIB_EVENT_FLAGS_NO_CLIENTS != 0 {
                        usb::usb_host_device_free_all();
                    }
                }
            })?;
        unsafe {
            let mut drv: usb::uvc_host_driver_config_t = core::mem::zeroed();
            // The frame callback (copy into the latest-frame buffer) and the
            // frame list fetch run on this task.
            drv.driver_task_stack_size = 8192;
            drv.driver_task_priority = 10;
            drv.xCoreID = 0;
            drv.create_background_task = true;
            drv.event_cb = Some(driver_event_cb);
            esp_check(usb::uvc_host_install(&drv), "uvc_host_install")?;
        }
        info!("camera: USB host installed, waiting for a UVC camera");
        let cam = Camera { inner };
        let c = cam.clone();
        std::thread::Builder::new()
            .name("camera-open".into())
            .stack_size(6 * 1024)
            .spawn(move || c.open_loop())?;
        let c = cam.clone();
        std::thread::Builder::new()
            .name("camera-idle".into())
            .stack_size(4 * 1024)
            .spawn(move || c.idle_loop())?;
        Ok(cam)
    }

    fn open_loop(&self) {
        loop {
            // Wait for an enumerated camera without an open stream.
            let (dev_addr, stream_index, formats) = {
                let mut conn = self.inner.connected.lock().unwrap();
                loop {
                    let has_stream = self.inner.dev.lock().unwrap().handle.is_some();
                    if let Some(c) = conn.as_ref().filter(|_| !has_stream) {
                        break (c.dev_addr, c.stream_index, c.formats.clone());
                    }
                    conn = self
                        .inner
                        .connected_cv
                        .wait_timeout(conn, Duration::from_millis(500))
                        .unwrap()
                        .0;
                }
            };
            let Some(format) = choose_format(&formats, self.inner.wanted) else {
                warn!("camera: no MJPEG format offered; this camera is not supported");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            };
            if let Some((w, h)) = self.inner.wanted {
                if (format.width, format.height) != (w, h) {
                    warn!(
                        "camera: {w}x{h} not offered, using {}x{}",
                        format.width, format.height
                    );
                }
            }
            match self.try_open(dev_addr, stream_index, &format) {
                Ok(()) => {
                    info!("camera: attached ({})", self.describe());
                    let vc = read_config_descriptor(dev_addr)
                        .map_err(|e| warn!("camera: configuration descriptor: {e}"))
                        .ok()
                        .and_then(|d| uvc::parse_vc(&d));
                    match &vc {
                        Some(v) => info!(
                            "camera: VideoControl interface {}, camera terminal {:?}, processing unit {:?}",
                            v.interface, v.camera_terminal, v.processing_unit
                        ),
                        None => warn!("camera: no VideoControl interface found; settings unavailable"),
                    }
                    *self.inner.vc.lock().unwrap() = vc;
                    let saved = self.inner.last_settings.lock().unwrap().clone();
                    if !saved.is_empty() {
                        match self.apply_settings(&saved) {
                            Ok(lines) => info!("camera: re-applied settings: {}", lines.join(" ")),
                            Err((_, e)) => warn!("camera: could not re-apply settings: {e}"),
                        }
                    }
                }
                Err(e) => {
                    warn!("camera: open failed: {e}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    }

    fn try_open(&self, dev_addr: u8, stream_index: u8, format: &FrameInfo) -> Result<()> {
        let fps_candidates: Vec<f32> = match self.inner.wanted_fps {
            Some(f) => vec![f as f32, 0.0],
            None => vec![0.0],
        };
        let mut last = anyhow!("no format tried");
        for fps in fps_candidates {
            match self.open_with(dev_addr, stream_index, format, fps) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if fps != 0.0 {
                        warn!("camera: {fps} fps not accepted ({e}); trying the default rate");
                    }
                    last = e;
                }
            }
        }
        Err(last)
    }

    fn open_with(&self, dev_addr: u8, stream_index: u8, format: &FrameInfo, fps: f32) -> Result<()> {
        let mut cfg: usb::uvc_host_stream_config_t = unsafe { core::mem::zeroed() };
        cfg.event_cb = Some(stream_event_cb);
        cfg.frame_cb = Some(frame_cb);
        cfg.user_ctx = core::ptr::null_mut();
        cfg.usb.dev_addr = dev_addr;
        cfg.usb.vid = usb::UVC_HOST_ANY_VID as u16;
        cfg.usb.pid = usb::UVC_HOST_ANY_PID as u16;
        cfg.usb.uvc_stream_index = stream_index;
        cfg.vs_format.h_res = format.width as u32;
        cfg.vs_format.v_res = format.height as u32;
        cfg.vs_format.fps = fps;
        cfg.vs_format.format = usb::uvc_host_stream_format_UVC_VS_FORMAT_MJPEG;
        cfg.advanced.number_of_frame_buffers = FRAME_BUFFERS;
        // JPEG frames are far smaller than dwMaxVideoFrameSize (often the raw
        // size); half the 16 bpp raw size leaves ample margin and keeps two
        // 1280x720 buffers under 1 MB of PSRAM.
        cfg.advanced.frame_size =
            (format.width as usize * format.height as usize / 2).max(48 * 1024);
        cfg.advanced.frame_heap_caps = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
        cfg.advanced.number_of_urbs = 4;
        cfg.advanced.urb_size = 0;
        cfg.advanced.user_frame_buffers = core::ptr::null_mut();

        {
            let mut d = self.inner.dev.lock().unwrap();
            d.in_use = true;
            d.pending_close = false;
        }
        let mut hdl: usb::uvc_host_stream_hdl_t = core::ptr::null_mut();
        let rc = unsafe { usb::uvc_host_stream_open(&cfg, 5000, &mut hdl) };
        let mut d = self.inner.dev.lock().unwrap();
        d.in_use = false;
        if rc != sys::ESP_OK {
            d.pending_close = false;
            return Err(anyhow!(
                "uvc_host_stream_open {}x{} @ {fps} fps: esp_err {rc}",
                format.width,
                format.height
            ));
        }
        if d.pending_close {
            // Disconnected while we were opening.
            d.pending_close = false;
            drop(d);
            unsafe { usb::uvc_host_stream_close(hdl) };
            return Err(anyhow!("device left during open"));
        }
        let mut got: usb::uvc_host_stream_format_t = unsafe { core::mem::zeroed() };
        let negotiated = if unsafe { usb::uvc_host_stream_format_get(hdl, &mut got) } == sys::ESP_OK
        {
            (got.h_res as u16, got.v_res as u16, got.fps)
        } else {
            (format.width, format.height, fps)
        };
        d.handle = Some(Handle(hdl));
        d.running = false;
        d.frames = 0;
        d.format = Some(negotiated);
        drop(d);
        self.inner.frame.lock().unwrap().seq = 0;
        self.inner.overflows.store(0, Ordering::Relaxed);
        self.inner.attached.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Runs a driver call on the open handle with the in-use protocol.
    pub(crate) fn with_handle(
        &self,
        f: impl FnOnce(usb::uvc_host_stream_hdl_t) -> Result<()>,
    ) -> Result<()> {
        let hdl = {
            let mut d = self.inner.dev.lock().unwrap();
            if d.in_use {
                return Err(anyhow!("camera busy"));
            }
            match d.handle {
                Some(h) => {
                    d.in_use = true;
                    h
                }
                None => return Err(anyhow!("no camera attached")),
            }
        };
        let result = f(hdl.0);
        let mut d = self.inner.dev.lock().unwrap();
        d.in_use = false;
        if d.pending_close {
            // The camera left while we were calling into the driver.
            d.pending_close = false;
            d.handle = None;
            d.running = false;
            drop(d);
            unsafe { usb::uvc_host_stream_close(hdl.0) };
            return Err(anyhow!("camera detached"));
        }
        result
    }

    fn ensure_streaming(&self) -> Result<()> {
        if self.inner.dev.lock().unwrap().running {
            return Ok(());
        }
        self.with_handle(|h| {
            let rc = unsafe { usb::uvc_host_stream_start(h) };
            if rc == sys::ESP_OK || rc == sys::ESP_ERR_INVALID_STATE {
                Ok(())
            } else {
                Err(anyhow!("uvc_host_stream_start: esp_err {rc}"))
            }
        })?;
        let mut d = self.inner.dev.lock().unwrap();
        if d.handle.is_some() {
            d.running = true;
            d.frames = 0;
            info!("camera: stream started");
        }
        Ok(())
    }

    /// Stops the stream; a call that finds the handle busy is retried by the
    /// idle loop on its next tick.
    fn stop_streaming(&self) {
        match self.with_handle(|h| {
            let rc = unsafe { usb::uvc_host_stream_stop(h) };
            if rc != sys::ESP_OK {
                warn!("camera: uvc_host_stream_stop: esp_err {rc}");
            }
            Ok(())
        }) {
            Ok(()) => {
                self.inner.dev.lock().unwrap().running = false;
                info!("camera: stream stopped (idle)");
            }
            Err(e) => log::debug!("camera: stop deferred: {e}"),
        }
    }

    fn idle_loop(&self) {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let running = self.inner.dev.lock().unwrap().running;
            if running && self.inner.last_request.lock().unwrap().elapsed() > IDLE_STOP {
                self.stop_streaming();
            }
        }
    }

    pub fn is_attached(&self) -> bool {
        self.inner.attached.load(Ordering::Relaxed)
    }

    /// Returns one JPEG frame captured after this call. `skip` additional
    /// frames are let pass first (after a settings change, the camera needs a
    /// frame or two to apply it).
    pub fn snapshot(&self, skip: u64) -> Result<Vec<u8>> {
        if !self.is_attached() {
            return Err(anyhow!("no camera attached"));
        }
        *self.inner.last_request.lock().unwrap() = Instant::now();
        let start_seq = self.inner.frame.lock().unwrap().seq;
        self.ensure_streaming()?;
        let deadline = Instant::now() + FRAME_TIMEOUT;
        let mut fr = self.inner.frame.lock().unwrap();
        loop {
            if fr.seq > start_seq + skip {
                return Ok(fr.data.clone());
            }
            if !self.is_attached() {
                return Err(anyhow!("camera detached"));
            }
            let now = Instant::now();
            if now >= deadline {
                let overflows = self.inner.overflows.load(Ordering::Relaxed);
                return Err(if overflows > 0 {
                    anyhow!("no frame within {}s: frames exceed the buffer ({overflows} dropped); lower the resolution", FRAME_TIMEOUT.as_secs())
                } else {
                    anyhow!("no frame within {}s", FRAME_TIMEOUT.as_secs())
                });
            }
            fr = self
                .inner
                .frame_cv
                .wait_timeout(fr, deadline - now)
                .unwrap()
                .0;
        }
    }

    /// One line for the status display.
    pub fn describe(&self) -> String {
        let d = self.inner.dev.lock().unwrap();
        match (self.is_attached(), d.format) {
            (true, Some((w, h, fps))) => format!(
                "MJPEG {w}x{h} @ {fps:.0} fps, {}",
                if d.running { "streaming" } else { "idle" }
            ),
            (true, None) => "attached".into(),
            (false, _) => {
                if self.inner.connected.lock().unwrap().is_some() {
                    "enumerated, opening".into()
                } else {
                    "detached".into()
                }
            }
        }
    }

    pub(crate) fn vc_info(&self) -> Option<VcInfo> {
        self.inner.vc.lock().unwrap().clone()
    }

    pub(crate) fn remember_settings(&self, params: &[(String, String)]) {
        let mut saved = self.inner.last_settings.lock().unwrap();
        for (k, v) in params {
            if uvc::find(k).is_none() {
                continue;
            }
            saved.retain(|(n, _)| n != k);
            saved.push((k.clone(), v.clone()));
        }
    }

    /// The formats the attached camera offers (empty when detached).
    pub fn formats(&self) -> Vec<FrameInfo> {
        self.inner
            .connected
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.formats.clone())
            .unwrap_or_default()
    }
}

unsafe extern "C" fn client_event_cb(_msg: *const usb::usb_host_client_event_msg_t, _arg: *mut c_void) {}

/// Reads the active configuration descriptor of the device at `dev_addr`
/// through a short-lived USB host client of our own (the UVC driver keeps
/// its device handle private).
fn read_config_descriptor(dev_addr: u8) -> Result<Vec<u8>> {
    unsafe {
        let mut cfg: usb::usb_host_client_config_t = core::mem::zeroed();
        cfg.is_synchronous = false;
        cfg.max_num_event_msg = 3;
        cfg.__bindgen_anon_1.async_.client_event_callback = Some(client_event_cb);
        let mut client: usb::usb_host_client_handle_t = core::ptr::null_mut();
        esp_check(
            usb::usb_host_client_register(&cfg, &mut client),
            "usb_host_client_register",
        )?;
        let mut dev: usb::usb_device_handle_t = core::ptr::null_mut();
        let result = (|| {
            esp_check(
                usb::usb_host_device_open(client, dev_addr, &mut dev),
                "usb_host_device_open",
            )?;
            let mut desc: *const usb::usb_config_desc_t = core::ptr::null();
            let r = esp_check(
                usb::usb_host_get_active_config_descriptor(dev, &mut desc),
                "usb_host_get_active_config_descriptor",
            )
            .and_then(|()| {
                if desc.is_null() {
                    return Err(anyhow!("null configuration descriptor"));
                }
                let p = desc as *const u8;
                let total = u16::from_le_bytes([*p.add(2), *p.add(3)]) as usize;
                Ok(std::slice::from_raw_parts(p, total).to_vec())
            });
            usb::usb_host_device_close(client, dev);
            r
        })();
        usb::usb_host_client_deregister(client);
        result
    }
}
