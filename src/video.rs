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
//! libmpv is loaded at runtime; video playback requires a compatible shared library.

use eframe::egui;

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


// The library stays loaded until after the render context and player are destroyed.
struct MpvApi {
    _library: libloading::Library,
    mpv_create: unsafe extern "C" fn() -> *mut c_void,
    mpv_initialize: unsafe extern "C" fn(ctx: *mut c_void) -> i32,
    mpv_destroy: unsafe extern "C" fn(ctx: *mut c_void),
    mpv_set_option_string: unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char, data: *const c_char) -> i32,
    mpv_command: unsafe extern "C" fn(ctx: *mut c_void, args: *const *const c_char) -> i32,
    mpv_set_property: unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char, format: i32, data: *mut c_void) -> i32,
    mpv_get_property: unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char, format: i32, data: *mut c_void) -> i32,
    mpv_render_context_create: unsafe extern "C" fn(res: *mut *mut c_void, mpv: *mut c_void, params: *mut MpvRenderParam) -> i32,
    mpv_render_context_set_update_callback: unsafe extern "C" fn(ctx: *mut c_void, callback: Option<extern "C" fn(*mut c_void)>, callback_ctx: *mut c_void),
    mpv_render_context_update: unsafe extern "C" fn(ctx: *mut c_void) -> u64,
    mpv_render_context_render: unsafe extern "C" fn(ctx: *mut c_void, params: *mut MpvRenderParam) -> i32,
    mpv_render_context_free: unsafe extern "C" fn(ctx: *mut c_void),
}

impl MpvApi {
    fn load() -> Result<Self, String> {
        #[cfg(target_os = "windows")]
        let names = ["mpv-2.dll", "libmpv-2.dll", "mpv-1.dll", "libmpv.dll", "mpv.dll"].as_slice();
        #[cfg(target_os = "macos")]
        let names = ["libmpv.2.dylib", "libmpv.1.dylib", "libmpv.dylib"].as_slice();
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let names = ["libmpv.so.2", "libmpv.so.1", "libmpv.so"].as_slice();

        Self::load_from_names(names)
    }

    fn load_from_names(names: &[&str]) -> Result<Self, String> {
        let mut errors = Vec::new();
        for name in names {
            // SAFETY: only load libraries installed by the user or shipped next to
            // the executable. Symbols below use libmpv's documented C ABI.
            let library = match unsafe { libloading::Library::new(*name) } {
                Ok(library) => library,
                Err(e) => { errors.push(format!("{name}: {e}")); continue; }
            };
            // SAFETY: the requested functions are the libmpv C API. Copy the
            // function pointers while keeping the owning library in this struct.
            let result = unsafe { Self::from_library(library) };
            match result {
                Ok(api) => return Ok(api),
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        Err(format!("libmpv not found or incompatible. Install libmpv or place its DLL beside the executable. Details: {}", errors.join("; ")))
    }

    unsafe fn from_library(library: libloading::Library) -> Result<Self, String> {
        let mpv_create = *library.get(b"mpv_create\0").map_err(|e| e.to_string())?;
        let mpv_initialize = *library.get(b"mpv_initialize\0").map_err(|e| e.to_string())?;
        let mpv_destroy = *library.get(b"mpv_destroy\0").map_err(|e| e.to_string())?;
        let mpv_set_option_string = *library.get(b"mpv_set_option_string\0").map_err(|e| e.to_string())?;
        let mpv_command = *library.get(b"mpv_command\0").map_err(|e| e.to_string())?;
        let mpv_set_property = *library.get(b"mpv_set_property\0").map_err(|e| e.to_string())?;
        let mpv_get_property = *library.get(b"mpv_get_property\0").map_err(|e| e.to_string())?;
        let mpv_render_context_create = *library.get(b"mpv_render_context_create\0").map_err(|e| e.to_string())?;
        let mpv_render_context_set_update_callback = *library.get(b"mpv_render_context_set_update_callback\0").map_err(|e| e.to_string())?;
        let mpv_render_context_update = *library.get(b"mpv_render_context_update\0").map_err(|e| e.to_string())?;
        let mpv_render_context_render = *library.get(b"mpv_render_context_render\0").map_err(|e| e.to_string())?;
        let mpv_render_context_free = *library.get(b"mpv_render_context_free\0").map_err(|e| e.to_string())?;
        Ok(Self {
            mpv_create,
            mpv_initialize,
            mpv_destroy,
            mpv_set_option_string,
            mpv_command,
            mpv_set_property,
            mpv_get_property,
            mpv_render_context_create,
            mpv_render_context_set_update_callback,
            mpv_render_context_update,
            mpv_render_context_render,
            mpv_render_context_free,
            _library: library,
        })
    }
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
    api: MpvApi,
    /// Opaque `mpv_handle*`.
    mpv: *mut c_void,
    /// Opaque `mpv_render_context*`.
    render: *mut c_void,
    /// Set by the mpv update callback; consumed by `refresh_frame`.
    frame_ready: Arc<AtomicBool>,
    /// Reusable SW target surface (`stride == width * 4`).
    buf: Vec<u32>,
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

        let api = MpvApi::load()?;
        // SAFETY: mpv_create either returns a valid handle or null; checked below.
        let mpv = unsafe { (api.mpv_create)() };
        if mpv.is_null() {
            return Err("mpv_create failed (is libmpv installed?)".into());
        }

        // Options must be set BEFORE mpv_initialize.
        let pre_init_opts = [
            ("vo", "libmpv"),          // required for the render API
            ("hwdec", "no"),           // SW renderer can't use hw surfaces
            ("keep-open", "yes"),      // stay on last frame at EOF
            ("config", "no"),          // never read user mpv.conf
            ("msg-level", "all=warn"), // quiet
        ];
        for (name, value) in pre_init_opts {
            let c_name = CString::new(name).unwrap();
            let c_value = CString::new(value).unwrap();
            // SAFETY: mpv and the CString args are valid for the call duration.
            let rc = unsafe { (api.mpv_set_option_string)(mpv, c_name.as_ptr(), c_value.as_ptr()) };
            if rc < 0 {
                unsafe { (api.mpv_destroy)(mpv) };
                return Err(format!("mpv rejected option {name}={value} (error {rc})"));
            }
        }

        // SAFETY: mpv is a valid handle created above.
        let rc = unsafe { (api.mpv_initialize)(mpv) };
        if rc < 0 {
            unsafe { (api.mpv_destroy)(mpv) };
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
        let rc = unsafe { (api.mpv_render_context_create)(&mut render, mpv, params.as_mut_ptr()) };
        if rc < 0 || render.is_null() {
            unsafe { (api.mpv_destroy)(mpv) };
            return Err(format!("mpv_render_context_create failed (error {rc})"));
        }

        // Register the update callback. The Arc's allocation is deliberately
        // leaked so the callback can never observe a freed flag, even if mpv
        // invokes it from one of its threads after we dropped everything.
        let frame_ready = Arc::new(AtomicBool::new(false));
        let cb_ctx = Arc::into_raw(Arc::clone(&frame_ready)) as *mut c_void;
        // SAFETY: render is valid; cb_ctx outlives the context (leaked).
        unsafe { (api.mpv_render_context_set_update_callback)(render, Some(render_update_trampoline), cb_ctx) };

        // Start decoding/playing the file.
        let c_loadfile = CString::new("loadfile").unwrap();
        let argv: [*const c_char; 3] = [c_loadfile.as_ptr(), c_path.as_ptr(), std::ptr::null()];
        // SAFETY: argv is a valid NULL-terminated argument vector.
        let rc = unsafe { (api.mpv_command)(mpv, argv.as_ptr()) };
        if rc < 0 {
            unsafe { (api.mpv_render_context_free)(render); (api.mpv_destroy)(mpv) };
            return Err(format!("mpv failed to open {} (error {rc})", path.display()));
        }

        Ok(VideoPlayer {
            api,
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
        let update = unsafe { (self.api.mpv_render_context_update)(self.render) };
        let callback_fired = self.frame_ready.swap(false, Ordering::Relaxed);
        if update & MPV_RENDER_UPDATE_FRAME == 0 && !callback_fired {
            return;
        }

        self.learn_aspect();

        let aspect = self.aspect.unwrap_or(16.0 / 9.0);
        let video_w = self.video_width().unwrap_or(MAX_RENDER_WIDTH);
        let desired_w = video_w.clamp(1, MAX_RENDER_WIDTH) as f32;
        let desired_h = desired_w / aspect;
        let scale = (MAX_RENDER_WIDTH as f32 / desired_h).min(1.0);
        let target_w = (desired_w * scale).round().max(1.0) as usize;
        let target_h = (desired_h * scale).round().clamp(1.0, MAX_RENDER_WIDTH as f32) as usize;

        let stride = target_w * 4;
        let needed = target_w * target_h;
        if self.buf.len() != needed || self.buf_size != (target_w, target_h) {
            self.buf = vec![0u32; needed];
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
        let rc = unsafe { (self.api.mpv_render_context_render)(self.render, params.as_mut_ptr()) };
        if rc < 0 {
            self.last_error = format!("mpv render failed (error {rc})");
            return;
        }
        // "rgb0" leaves the 4th byte undefined — force opaque alpha for egui.
        // u32 storage guarantees the 4-byte alignment required by libmpv.
        let bytes = unsafe { std::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast::<u8>(), needed * 4) };
        for px in bytes.chunks_exact_mut(4) {
            px[3] = 255;
        }

        let image = egui::ColorImage::from_rgba_unmultiplied([target_w, target_h], bytes);
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
        unsafe { (self.api.mpv_command)(self.mpv, argv.as_ptr()) };
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
        let rc = unsafe { (self.api.mpv_get_property)(self.mpv, c_name.as_ptr(), MPV_FORMAT_FLAG, &mut value as *mut i32 as *mut c_void) };
        (rc >= 0).then_some(value != 0)
    }

    fn set_flag(&self, name: &str, value: i32) {
        let Some(c_name) = CString::new(name).ok() else { return };
        let value = value;
        // SAFETY: c_name is valid; value is a valid in-pointer.
        unsafe { (self.api.mpv_set_property)(self.mpv, c_name.as_ptr(), MPV_FORMAT_FLAG, &value as *const i32 as *mut c_void) };
    }

    fn get_double(&self, name: &str) -> Option<f64> {
        let c_name = CString::new(name).ok()?;
        let mut value: f64 = 0.0;
        // SAFETY: c_name is valid; value is a valid out-pointer.
        let rc = unsafe { (self.api.mpv_get_property)(self.mpv, c_name.as_ptr(), MPV_FORMAT_DOUBLE, &mut value as *mut f64 as *mut c_void) };
        (rc >= 0).then_some(value)
    }

    fn set_double(&self, name: &str, value: f64) {
        let Some(c_name) = CString::new(name).ok() else { return };
        // SAFETY: c_name is valid; value is a valid in-pointer.
        unsafe { (self.api.mpv_set_property)(self.mpv, c_name.as_ptr(), MPV_FORMAT_DOUBLE, &value as *const f64 as *mut c_void) };
    }

    fn get_int64(&self, name: &str) -> Option<i64> {
        let c_name = CString::new(name).ok()?;
        let mut value: i64 = 0;
        // SAFETY: c_name is valid; value is a valid out-pointer.
        let rc = unsafe { (self.api.mpv_get_property)(self.mpv, c_name.as_ptr(), MPV_FORMAT_INT64, &mut value as *mut i64 as *mut c_void) };
        (rc >= 0).then_some(value)
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        // Free the render context BEFORE the mpv handle — required by libmpv.
        // SAFETY: both pointers are valid until freed here; Drop runs once.
        unsafe {
            if !self.render.is_null() {
                (self.api.mpv_render_context_free)(self.render);
                self.render = std::ptr::null_mut();
            }
            if !self.mpv.is_null() {
                (self.api.mpv_destroy)(self.mpv);
                self.mpv = std::ptr::null_mut();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_library_returns_actionable_error() {
        let error = match MpvApi::load_from_names(&["mlxysftele-nonexistent-libmpv-12345.dll"]) {
            Ok(_) => panic!("unexpectedly loaded nonexistent library"),
            Err(error) => error,
        };
        assert!(error.contains("libmpv not found or incompatible"));
    }

    #[test]
    fn initializes_software_renderer() {
        // Windows build CI first tests without an mpv DLL, then sets this
        // variable after installing the DLL to exercise actual initialization.
        if cfg!(target_os = "windows") && std::env::var_os("MLXYSFTELE_TEST_MPV").is_none() {
            return;
        }
        // Opening a missing file is asynchronous; this exercises initialization
        // and verifies our mpv options and SW renderer without a display.
        let player = VideoPlayer::open(Path::new("/nonexistent-mlxysftele-video.mp4"), "test");
        assert!(player.is_ok(), "libmpv initialization failed: {}", player.err().unwrap_or_default());
    }
}
