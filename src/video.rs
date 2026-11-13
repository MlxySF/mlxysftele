//! Embedded video playback, rendered by libmpv into memory surfaces
//! (`MPV_RENDER_API_TYPE_SW`) and uploaded as egui textures.
//!
//! Why SW and not OpenGL: the SW renderer needs no GPU context juggling with
//! eframe/wgpu/glutin, needs no BookAccess-level GL plumbing, and is more than
//! fast enough for a small overlay at capped resolution (we render at
//! <= 1280px wide). Audio is played by mpv itself through its own audio
//! output (pulse/pipewire/alsa — whatever libmpv finds), so no PCM plumbing
//! is required from the GUI.
//!
//! Threading: libmpv calls the update callback from its own threads. The only
//! thing that callback does is set an `AtomicBool`; all libmpv API calls happen
//! on the GUI thread (never from the callback), which satisfies mpv's
//! threading requirements.
//!
//! `#[link(name = "mpv")]` requires libmpv to be present at link time
//! (Debian/Ubuntu: `apt install libmpv-dev`).

use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// MPV_FORMAT_* values from <mpv/client.h>.
const MPV_FORMAT_FLAG: i32 = 3; // int 0/1
const MPV_FORMAT_INT64: i32 = 4; // int64_t
const MPV_FORMAT_DOUBLE: i32 = 5; // double

// MPV_RENDER_PARAM_* values from <mpv/render.h>.
const MPV_RENDER_PARAM_INVALID: i32 = 0;
const MPV_RENDER_PARAM_API_TYPE: i32 = 1;
const MPV_RENDER_PARAM_SW_SIZE: i32 = 17;
const MPV_RENDER_PARAM_SW_FORMAT: i32 = 18;
const MPV_RENDER_PARAM_SW_STRIDE: i32 = 19;
const MPV_RENDER_PARAM_SW_POINTER: i32 = 20;

const MPV_RENDER_UPDATE_FRAME: u64 = 1 << 0;

#[repr(C)]
struct MpvRenderParam {
    type_: i32,
    data: *mut c_void,
}

#[link(name = "mpv")]
extern "C" {
    fn mpv_create() -> *mut c_void;
    fn mpv_initialize(ctx: *mut c_void) -> i32;
    fn mpv_destroy(ctx: *mut c_void);
    fn mpv_set_option_string(ctx: *mut c_void, name: *const c_char, data: *const c_char) -> i32;
    fn mpv_set_property_string(ctx: *mut c_void, name: *const c_char, data: *const c_char) -> i32;
    fn mpv_command(ctx: *mut c_void, args: *const *const c_char) -> i32;
    fn mpv_set_property(ctx: *mut c_void, name: *const c_char, format: i32, data: *mut c_void) -> i32;
    fn mpv_get_property(ctx: *mut c_void, name: *const c_char, format: i32, data: *mut c_void) -> i32;
    fn mpv_render_context_create(res: *mut *mut c_void, mpv: *mut c_void, params: *mut MpvRenderParam) -> i32;
    fn mpv_render_context_set_update_callback(ctx: *mut c_void, callback: Option<extern "C" fn(*mut c_void)>, callback_ctx: *mut c_void);
    fn mpv_render_context_update(ctx: *mut c_void) -> u64;
    fn mpv_render_context_render(ctx: *mut c_void, params: *mut MpvRenderParam) -> i32;
    fn mpv_render_context_free(ctx: *mut c_void);
}

/// Called by libmpv (possibly from an mpv thread) when a new frame is ready.
/// It must not call any mpv function — it only pokes the flag.
extern "C" fn render_update_trampoline(callback_ctx: *mut c_void) {
    if callback_ctx.is_null() {
        return;
    }
    // SAFETY: `callback_ctx` is an `Arc::into_raw` pointer that is intentionally
    // leaked for the lifetime of the process, so it is always valid.
    let flag = unsafe { &*(callback_ctx as *const AtomicBool) };
    flag.store(true, Ordering::Relaxed);
}

/// Largest render width for the SW surface — plenty for a 90%-screen overlay
/// while keeping the CPU path comfortable.
const MAX_RENDER_WIDTH: i64 = 1280;

pub struct VideoPlayer {
    /// Opaque `mpv_handle*`.
    mpv: *mut c_void,
    /// Opaque `mpv_render_context*`.
    render: *mut c_void,
    /// Set by the mpv update callback; consumed by `refresh_frame`.
    frame_ready: Arc<AtomicBool>,
    /// Reusable SW target surface (`stride == width * 4`).
    buf: Vec<u8>,
    buf_size: (usize, usize),
    /// Display aspect (w/h), learned once mpv reports video params.
    aspect: Option<f32>,
    /// Most recent decoded frame as an egui texture.
    texture: Option<egui::TextureHandle>,
    /// Last mpv error, if any (kept for the toast/diagnostic).
    last_error: String,
    pub path: PathBuf,
    pub title: String,
}

impl VideoPlayer {
    /// Open `path` and start playback (audio + video). Playback begins
    /// immediately; pause it with `set_paused`.
    pub fn open(path: &Path, title: &str) -> Result<VideoPlayer, String> {
        let path_str = path.to_string_lossy().into_owned();
        let c_path = CString::new(path_str).map_err(|_| "path contains NUL".to_string())?;

        // SAFETY: mpv_create either returns a valid handle or null; checked below.
        let mpv = unsafe { mpv_create() };
        if mpv.is_null() {
            return Err("mpv_create failed (is libmpv installed?)".into());
        }

        // Options must be set BEFORE mpv_initialize.
        let pre_init_opts = [
            ("vo", "libmpv"),          // required for the render API
            ("hwdec", "no"),           // SW renderer can't use hw surfaces
            ("keep-open", "yes"),      // stay on last frame at EOF
            ("config", "no"),          // never read user mpv.conf
            ("log-level", "warn"),     // quiet
        ];
        for (name, value) in pre_init_opts {
            let c_name = CString::new(name).unwrap();
            let c_value = CString::new(value).unwrap();
            // SAFETY: mpv and the CString args are valid for the call duration.
            let rc = unsafe { mpv_set_option_string(mpv, c_name.as_ptr(), c_value.as_ptr()) };
            if rc < 0 {
                unsafe { mpv_destroy(mpv) };
                return Err(format!("mpv rejected option {name}={value} (error {rc})"));
            }
        }

        // SAFETY: mpv is a valid handle created above.
        let rc = unsafe { mpv_initialize(mpv) };
        if rc < 0 {
            unsafe { mpv_destroy(mpv) };
            return Err(format!("mpv_initialize failed (error {rc})"));
        }

        // Create the SW render context.
        let api_type = b"sw\0".as_ptr() as *mut c_void;
        let mut params = [
            MpvRenderParam { type_: MPV_RENDER_PARAM_API_TYPE, data: api_type },
            MpvRenderParam { type_: MPV_RENDER_PARAM_INVALID, data: std::ptr::null_mut() },
        ];
        let mut render: *mut c_void = std::ptr::null_mut();
        // SAFETY: params is a valid, terminated array; mpv and render are valid.
        let rc = unsafe { mpv_render_context_create(&mut render, mpv, params.as_mut_ptr()) };
        if rc < 0 || render.is_null() {
            unsafe { mpv_destroy(mpv) };
            return Err(format!("mpv_render_context_create failed (error {rc})"));
        }

        // Register the update callback. The Arc's allocation is deliberately
        // leaked so the callback can never observe a freed flag, even if mpv
        // invokes it from one of its threads after we dropped everything.
        let frame_ready = Arc::new(AtomicBool::new(false));
        let cb_ctx = Arc::into_raw(Arc::clone(&frame_ready)) as *mut c_void;
        // SAFETY: render is valid; cb_ctx outlives the context (leaked).
        unsafe { mpv_render_context_set_update_callback(render, Some(render_update_trampoline), cb_ctx) };

        // Start decoding/playing the file.
        let c_loadfile = CString::new("loadfile").unwrap();
        let argv: [*const c_char; 3] = [c_loadfile.as_ptr(), c_path.as_ptr(), std::ptr::null()];
        // SAFETY: argv is a valid NULL-terminated argument vector.
        let rc = unsafe { mpv_command(mpv, argv.as_ptr()) };
        if rc < 0 {
            unsafe { mpv_render_context_free(render); mpv_destroy(mpv) };
            return Err(format!("mpv failed to open {} (error {rc})", path.display()));
        }

        Ok(VideoPlayer {
            mpv,
            render,
            frame_ready,
            buf: Vec::new(),
            buf_size: (0, 0),
            aspect: None,
            texture: None,
            last_error: String::new(),
            path: path.to_path_buf(),
            title: title.to_string(),
        })
    }

    /// If mpv has a new frame ready, decode it into the egui texture cache.
    /// Cheap no-op otherwise (one atomic load).
    pub fn refresh_frame(&mut self, ctx: &egui::Context) {
        // Ask mpv whether anything is pending; even without the callback this
        // keeps us correct (it also nudges ADVANCED_CONTROL-style internals).
        // SAFETY: render is valid until Drop.
        let update = unsafe { mpv_render_context_update(self.render) };
        let callback_fired = self.frame_ready.swap(false, Ordering::Relaxed);
        if update & MPV_RENDER_UPDATE_FRAME == 0 && !callback_fired {
            return;
        }

        self.learn_aspect();

        let aspect = self.aspect.unwrap_or(16.0 / 9.0);
        let video_w = self.video_width().unwrap_or(MAX_RENDER_WIDTH);
        let target_w = video_w.clamp(160, MAX_RENDER_WIDTH) as usize;
        let target_h = ((target_w as f32 / aspect).round().max(90.0)) as usize;

        let stride = target_w * 4;
        let needed = stride * target_h;
        if self.buf.len() != needed || self.buf_size != (target_w, target_h) {
            self.buf = vec![0u8; needed];
            self.buf_size = (target_w, target_h);
        }

        let mut sw_size = [target_w as i32, target_h as i32];
        let sw_format = b"rgb0\0".as_ptr() as *mut c_void;
        let sw_stride: usize = stride;
        let sw_pointer = self.buf.as_mut_ptr() as *mut c_void;
        let mut params = [
            MpvRenderParam { type_: MPV_RENDER_PARAM_SW_SIZE, data: sw_size.as_mut_ptr() as *mut c_void },
            MpvRenderParam { type_: MPV_RENDER_PARAM_SW_FORMAT, data: sw_format },
            MpvRenderParam { type_: MPV_RENDER_PARAM_SW_STRIDE, data: &sw_stride as *const usize as *mut c_void },
            MpvRenderParam { type_: MPV_RENDER_PARAM_SW_POINTER, data: sw_pointer },
            MpvRenderParam { type_: MPV_RENDER_PARAM_INVALID, data: std::ptr::null_mut() },
        ];
        // SAFETY: params is a valid, terminated array; the buffer lives for the
        // call; render is valid until Drop.
        let rc = unsafe { mpv_render_context_render(self.render, params.as_mut_ptr()) };
        if rc < 0 {
            self.last_error = format!("mpv render failed (error {rc})");
            return;
        }
        // "rgb0" leaves the 4th byte undefined — force opaque alpha for egui.
        for px in self.buf.chunks_exact_mut(4) {
            px[3] = 255;
        }

        let image = egui::ColorImage::from_rgba_unmultiplied([target_w, target_h], &self.buf);
        match &mut self.texture {
            Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.texture =
                    Some(ctx.load_texture("mlxysftele-video", image, egui::TextureOptions::LINEAR))
            }
        }
    }

    pub fn texture_id(&self) -> Option<egui::TextureId> {
        self.texture.as_ref().map(|t| t.id())
    }

    /// Width/height of the current video in display pixels, aspect ratio.
    pub fn size_vec2(&self) -> Option<egui::Vec2> {
        self.texture.as_ref().map(|t| t.size_vec2())
    }

    pub fn is_paused(&self) -> bool {
        self.get_flag("pause").unwrap_or(false)
    }

    pub fn set_paused(&self, paused: bool) {
        let value = if paused { 1i32 } else { 0i32 };
        self.set_flag("pause", value);
    }

    pub fn time_pos(&self) -> Option<f64> {
        self.get_double("time-pos")
    }

    pub fn duration(&self) -> Option<f64> {
        self.get_double("duration")
    }

    /// Jump to an absolute position (seconds), clamped to >= 0.
    pub fn seek_to(&self, seconds: f64) {
        self.set_double("time-pos", seconds.max(0.0));
    }

    pub fn skip_relative(&self, delta_seconds: f64) {
        let arg = format!("{delta_seconds:+.3}");
        let Ok(c_arg) = CString::new(arg) else { return };
        let c_seek = CString::new("seek").unwrap();
        let c_relative = CString::new("relative").unwrap();
        let argv: [*const c_char; 4] = [
            c_seek.as_ptr(),
            c_arg.as_ptr(),
            c_relative.as_ptr(),
            std::ptr::null(),
        ];
        // SAFETY: argv is a valid NULL-terminated argument vector.
        unsafe { mpv_command(self.mpv, argv.as_ptr()) };
    }

    pub fn volume(&self) -> Option<f64> {
        self.get_double("volume")
    }

    pub fn set_volume(&self, volume: f64) {
        self.set_double("volume", volume.clamp(0.0, 100.0));
    }

    pub fn eof_reached(&self) -> bool {
        self.get_flag("eof-reached").unwrap_or(false)
    }

    /// Restart from the beginning (used by the "replay" button at EOF).
    pub fn replay(&self) {
        self.set_double("time-pos", 0.0);
        self.set_flag("pause", 0);
    }

    pub fn last_error(&self) -> &str {
        &self.last_error
    }

    fn video_width(&self) -> Option<i64> {
        self.get_int64("width")
    }

    fn video_height(&self) -> Option<i64> {
        self.get_int64("height")
    }

    fn learn_aspect(&mut self) {
        if self.aspect.is_some() {
            return;
        }
        match (self.video_width(), self.video_height()) {
            (Some(w), Some(h)) if w > 0 && h > 0 => {
                self.aspect = Some(w as f32 / h as f32);
            }
            _ => {}
        }
    }

    fn get_flag(&self, name: &str) -> Option<bool> {
        let c_name = CString::new(name).ok()?;
        let mut value: i32 = 0;
        // SAFETY: c_name is valid; value is a valid out-pointer.
        let rc = unsafe { mpv_get_property(self.mpv, c_name.as_ptr(), MPV_FORMAT_FLAG, &mut value as *mut i32 as *mut c_void) };
        (rc >= 0).then_some(value != 0)
    }

    fn set_flag(&self, name: &str, value: i32) {
        let Some(c_name) = CString::new(name).ok() else { return };
        let value = value;
        // SAFETY: c_name is valid; value is a valid in-pointer.
        unsafe { mpv_set_property(self.mpv, c_name.as_ptr(), MPV_FORMAT_FLAG, &value as *const i32 as *mut c_void) };
    }

    fn get_double(&self, name: &str) -> Option<f64> {
        let c_name = CString::new(name).ok()?;
        let mut value: f64 = 0.0;
        // SAFETY: c_name is valid; value is a valid out-pointer.
        let rc = unsafe { mpv_get_property(self.mpv, c_name.as_ptr(), MPV_FORMAT_DOUBLE, &mut value as *mut f64 as *mut c_void) };
        (rc >= 0).then_some(value)
    }

    fn set_double(&self, name: &str, value: f64) {
        let Some(c_name) = CString::new(name).ok() else { return };
        // SAFETY: c_name is valid; value is a valid in-pointer.
        unsafe { mpv_set_property(self.mpv, c_name.as_ptr(), MPV_FORMAT_DOUBLE, &value as *const f64 as *mut c_void) };
    }

    fn get_int64(&self, name: &str) -> Option<i64> {
        let c_name = CString::new(name).ok()?;
        let mut value: i64 = 0;
        // SAFETY: c_name is valid; value is a valid out-pointer.
        let rc = unsafe { mpv_get_property(self.mpv, c_name.as_ptr(), MPV_FORMAT_INT64, &mut value as *mut i64 as *mut c_void) };
        (rc >= 0).then_some(value)
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        // Free the render context BEFORE the mpv handle — required by libmpv.
        // SAFETY: both pointers are valid until freed here; Drop runs once.
        unsafe {
            if !self.render.is_null() {
                mpv_render_context_free(self.render);
                self.render = std::ptr::null_mut();
            }
            if !self.mpv.is_null() {
                mpv_destroy(self.mpv);
                self.mpv = std::ptr::null_mut();
            }
        }
    }
}
