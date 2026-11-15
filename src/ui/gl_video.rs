//! Linux, GPU renderer: received streams drawn straight from the decoder's buffers.
//!
//! Each window that can show a stream (main window, popout) has a `Presenter` living in its
//! rendering notifier, so everything here runs on the UI thread with that window's own GL
//! context current. Before a frame is drawn, the newest picture the decoder offered
//! (hw_decode::gl_present) is imported: DMABuf -> EGLImage -> GL texture, which the window's
//! ordinary `Image` then shows as a borrowed texture. Nothing is read back or copied.
//!
//! This needs an EGL context (Wayland; under X11 the backend uses GLX) with DMABuf import.
//! Whatever is missing, or fails later, turns the whole thing off for the run
//! (`gl_present::give_up`) and streams are shown through pixels as before.
//!
//! A texture belongs to the context that made it: the two windows never exchange images,
//! each imports the picture itself.

use std::cell::{Cell, RefCell};
use std::ffi::{c_void, CStr, CString};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use gstreamer as gst;
use gstreamer_allocators as gst_alloc;
use gstreamer_video as gst_video;
use slint::ComponentHandle;

use crate::screen_capture;
use crate::screen_capture::hw_decode::gl_present;
use crate::{AppWindow, PopoutStreamWindow};

type GlGenTextures = unsafe extern "system" fn(i32, *mut u32);
type GlDeleteTextures = unsafe extern "system" fn(i32, *const u32);
type GlBindTexture = unsafe extern "system" fn(u32, u32);
type GlTexParameteri = unsafe extern "system" fn(u32, u32, i32);
type GlGetIntegerv = unsafe extern "system" fn(u32, *mut i32);
type GlGetError = unsafe extern "system" fn() -> u32;
type GlGetString = unsafe extern "system" fn(u32) -> *const u8;
type GlEglImageTargetTexture2d = unsafe extern "system" fn(u32, *mut c_void);

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_BINDING_2D: u32 = 0x8069;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_RENDERER: u32 = 0x1F01;
const GL_EXTENSIONS: u32 = 0x1F03;

/// The few GL calls needed, looked up through Slint's `get_proc_address`.
struct Gl {
    gen_textures: GlGenTextures,
    delete_textures: GlDeleteTextures,
    bind_texture: GlBindTexture,
    tex_parameteri: GlTexParameteri,
    get_integerv: GlGetIntegerv,
    get_error: GlGetError,
    get_string: GlGetString,
    /// GL_OES_EGL_image.
    egl_image_target_texture_2d: GlEglImageTargetTexture2d,
}

impl Gl {
    unsafe fn load(get: &dyn Fn(&CStr) -> *const c_void) -> Option<Gl> {
        let sym = |name: &str| -> Option<*const c_void> {
            let p = get(&CString::new(name).ok()?);
            (!p.is_null()).then_some(p)
        };
        Some(Gl {
            gen_textures: std::mem::transmute::<*const c_void, GlGenTextures>(sym("glGenTextures")?),
            delete_textures: std::mem::transmute::<*const c_void, GlDeleteTextures>(sym("glDeleteTextures")?),
            bind_texture: std::mem::transmute::<*const c_void, GlBindTexture>(sym("glBindTexture")?),
            tex_parameteri: std::mem::transmute::<*const c_void, GlTexParameteri>(sym("glTexParameteri")?),
            get_integerv: std::mem::transmute::<*const c_void, GlGetIntegerv>(sym("glGetIntegerv")?),
            get_error: std::mem::transmute::<*const c_void, GlGetError>(sym("glGetError")?),
            get_string: std::mem::transmute::<*const c_void, GlGetString>(sym("glGetString")?),
            egl_image_target_texture_2d: std::mem::transmute::<*const c_void, GlEglImageTargetTexture2d>(sym("glEGLImageTargetTexture2DOES")?),
        })
    }

    unsafe fn string(&self, what: u32) -> String {
        let p = (self.get_string)(what);
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p as *const _).to_string_lossy().to_string()
    }
}

type EglGetCurrent = unsafe extern "C" fn() -> *mut c_void;
type EglQueryString = unsafe extern "C" fn(*mut c_void, i32) -> *const std::ffi::c_char;
type EglCreateImage = unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const isize) -> *mut c_void;
type EglDestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type EglGetError = unsafe extern "C" fn() -> i32;

const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_HEIGHT: isize = 0x3056;
const EGL_WIDTH: isize = 0x3057;
const EGL_NONE: isize = 0x3038;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: isize = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: isize = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: isize = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: isize = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: isize = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: isize = 0x3444;
/// DRM_FORMAT_MOD_INVALID: "no modifier given".
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// EGL from libEGL itself, for the display of the context that is current. Under GLX the
/// library still loads and then reports that there is no current EGL context.
struct Egl {
    display: *mut c_void,
    create_image: EglCreateImage,
    destroy_image: EglDestroyImage,
    get_error: EglGetError,
    extensions: String,
}

impl Egl {
    /// Must be called with the window's context current.
    unsafe fn current() -> Result<Egl, String> {
        let lib = libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
        if lib.is_null() {
            return Err("libEGL.so.1 not found".into());
        }
        let sym = |name: &CStr| -> Result<*mut c_void, String> {
            let p = libc::dlsym(lib, name.as_ptr());
            if p.is_null() {
                Err(format!("{} missing in libEGL", name.to_string_lossy()))
            } else {
                Ok(p)
            }
        };
        let get_display = std::mem::transmute::<*mut c_void, EglGetCurrent>(sym(c"eglGetCurrentDisplay")?);
        let get_context = std::mem::transmute::<*mut c_void, EglGetCurrent>(sym(c"eglGetCurrentContext")?);
        let query_string = std::mem::transmute::<*mut c_void, EglQueryString>(sym(c"eglQueryString")?);
        let display = get_display();
        if display.is_null() || get_context().is_null() {
            return Err("the window's GL context is not an EGL context (X11/GLX)".into());
        }
        let ext = query_string(display, EGL_EXTENSIONS);
        let extensions = if ext.is_null() { String::new() } else { CStr::from_ptr(ext).to_string_lossy().to_string() };
        Ok(Egl {
            display,
            // EGL 1.5 (Mesa has had it for years).
            create_image: std::mem::transmute::<*mut c_void, EglCreateImage>(sym(c"eglCreateImage")?),
            destroy_image: std::mem::transmute::<*mut c_void, EglDestroyImage>(sym(c"eglDestroyImage")?),
            get_error: std::mem::transmute::<*mut c_void, EglGetError>(sym(c"eglGetError")?),
            extensions,
        })
    }

    fn has(&self, ext: &str) -> bool {
        self.extensions.split(' ').any(|e| e == ext)
    }
}

/// What the caps and the buffer say about a single-plane RGB DMABuf.
struct DmabufPlane {
    fd: i32,
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    offset: usize,
    stride: i32,
    drm_format: String,
}

fn dmabuf_plane(sample: &gst::Sample) -> Result<DmabufPlane, String> {
    let caps = sample.caps().ok_or("picture without caps")?;
    let s = caps.structure(0).ok_or("empty caps")?;
    let width = s.get::<i32>("width").map_err(|e| e.to_string())? as u32;
    let height = s.get::<i32>("height").map_err(|e| e.to_string())? as u32;
    // "XB24:0x0200000028a01f04", or "XB24" alone for a linear buffer.
    let drm_format = s.get::<String>("drm-format").map_err(|e| format!("no drm-format in {caps}: {e}"))?;
    let (code, modifier) = match drm_format.split_once(':') {
        Some((c, m)) => (c, u64::from_str_radix(m.trim_start_matches("0x"), 16).map_err(|e| e.to_string())?),
        None => (drm_format.as_str(), DRM_FORMAT_MOD_INVALID),
    };
    let c = code.as_bytes();
    if c.len() != 4 {
        return Err(format!("odd drm-format {drm_format}"));
    }
    let fourcc = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    let buffer = sample.buffer().ok_or("picture without buffer")?;
    let meta = buffer.meta::<gst_video::VideoMeta>().ok_or("DMABuf without VideoMeta")?;
    if buffer.n_memory() != 1 || meta.n_planes() != 1 {
        return Err(format!("{} memories, {} planes (one of each expected)", buffer.n_memory(), meta.n_planes()));
    }
    let mem = buffer.peek_memory(0);
    let dma = mem.downcast_memory_ref::<gst_alloc::DmaBufMemory>().ok_or("not DMABuf memory")?;
    Ok(DmabufPlane { fd: dma.fd(), width, height, fourcc, modifier, offset: meta.offset()[0] + mem.offset(), stride: meta.stride()[0], drm_format })
}

/// A picture the window is showing: our texture, bound to an EGLImage of the decoder's
/// DMABuf. The sample keeps the buffer out of the decoder's pool while it is on screen.
struct Shown {
    tex: u32,
    egl_image: *mut c_void,
    image: slint::Image,
    _sample: gst::Sample,
}

/// One window's side of the hand-over. Lives in the window's rendering notifier.
struct Presenter {
    name: &'static str,
    main: bool,
    gl: Option<Gl>,
    egl: Option<Egl>,
    /// The stream this window shows now, if it shows one.
    wants: Box<dyn Fn() -> Option<u64>>,
    /// The window's picture property: read, write.
    shown: Box<dyn Fn() -> slint::Image>,
    show: Box<dyn Fn(slint::Image)>,
    current: Option<Shown>,
    /// Replaced on the last frame: deleted once a whole frame has been drawn without them.
    retired: Vec<Shown>,
    /// The picture imported last, and whether the wake-up emptied the picture property
    /// since the last frame (both shared with it, see `install`).
    last_seq: Rc<Cell<u64>>,
    emptied: Rc<Cell<bool>>,
    imported: u64,
    /// What the log last said this window shows (drm-format, width, height).
    announced: Option<(String, u32, u32)>,
}

impl Presenter {
    /// Looks at the window's context once it exists: the first frame, and the first frame
    /// after the context was recreated (the popout's is destroyed when it is hidden).
    fn setup(&mut self, api: &slint::GraphicsAPI<'_>) {
        let found = (|| -> Result<(Gl, Egl), String> {
            let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = api else {
                return Err("the renderer is not OpenGL".into());
            };
            let gl = unsafe { Gl::load(get_proc_address) }.ok_or("GL functions missing (GL_OES_EGL_image)")?;
            let egl = unsafe { Egl::current() }?;
            let gl_extensions = unsafe { gl.string(GL_EXTENSIONS) };
            if !gl_extensions.split(' ').any(|e| e == "GL_OES_EGL_image") {
                return Err("GL_OES_EGL_image missing".into());
            }
            for ext in ["EGL_EXT_image_dma_buf_import", "EGL_EXT_image_dma_buf_import_modifiers"] {
                if !egl.has(ext) {
                    return Err(format!("{ext} missing"));
                }
            }
            let renderer = unsafe { gl.string(GL_RENDERER) };
            if !gl_present::verified_on(&renderer) {
                return Err(format!("not verified on this driver yet ({renderer}); LITECORD_GL_PRESENT=1 tries it anyway"));
            }
            Ok((gl, egl))
        })();
        match found {
            Ok((gl, egl)) => {
                if self.main && !gl_present::ready() {
                    log::info!("🎞️ [GL VIDEO] {} window: EGL context on \"{}\" with DMABuf import", self.name, unsafe { gl.string(GL_RENDERER) });
                    gl_present::set_ready();
                }
                self.gl = Some(gl);
                self.egl = Some(egl);
            }
            Err(e) => gl_present::give_up(&format!("{} window: {}", self.name, e)),
        }
    }

    /// DMABuf -> EGLImage -> texture, in the window's own context.
    unsafe fn import(&mut self, sample: gst::Sample) -> Result<Shown, String> {
        let p = dmabuf_plane(&sample)?;
        self.imported += 1;
        if gl_present::test_fail_after().is_some_and(|n| self.imported > n) {
            return Err("simulated failure (LITECORD_GL_PRESENT_TEST_FAIL=after)".into());
        }
        let (Some(gl), Some(egl)) = (&self.gl, &self.egl) else { return Err("no EGL context".into()) };
        let mut attribs: Vec<isize> = vec![
            EGL_WIDTH,
            p.width as isize,
            EGL_HEIGHT,
            p.height as isize,
            EGL_LINUX_DRM_FOURCC_EXT,
            p.fourcc as isize,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            p.fd as isize,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            p.offset as isize,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            p.stride as isize,
        ];
        if p.modifier != DRM_FORMAT_MOD_INVALID {
            attribs.extend_from_slice(&[EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (p.modifier & 0xffff_ffff) as isize, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (p.modifier >> 32) as isize]);
        }
        attribs.push(EGL_NONE);
        // EGL_NO_CONTEXT and a null client buffer: that is what EGL_LINUX_DMA_BUF_EXT wants.
        let egl_image = (egl.create_image)(egl.display, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attribs.as_ptr());
        if egl_image.is_null() {
            return Err(format!("eglCreateImage failed for {} {}x{}: 0x{:x}", p.drm_format, p.width, p.height, (egl.get_error)()));
        }
        // The renderer does not expect its texture binding to change under it.
        let mut previous = 0i32;
        (gl.get_integerv)(GL_TEXTURE_BINDING_2D, &mut previous);
        let mut tex = 0u32;
        (gl.gen_textures)(1, &mut tex);
        (gl.bind_texture)(GL_TEXTURE_2D, tex);
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        while (gl.get_error)() != 0 {}
        (gl.egl_image_target_texture_2d)(GL_TEXTURE_2D, egl_image);
        let err = (gl.get_error)();
        (gl.bind_texture)(GL_TEXTURE_2D, previous as u32);
        let Some(id) = std::num::NonZeroU32::new(tex).filter(|_| err == 0) else {
            (gl.delete_textures)(1, &tex);
            (egl.destroy_image)(egl.display, egl_image);
            return Err(format!("glEGLImageTargetTexture2DOES failed for {} {}x{}: 0x{err:x}", p.drm_format, p.width, p.height));
        };
        let image = slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(id, (p.width, p.height).into()).build();
        let now = (p.drm_format, p.width, p.height);
        if self.announced.as_ref() != Some(&now) {
            log::info!("🎞️ [GL VIDEO] {} window shows the stream from the GPU: {} {}x{} DMABuf as a texture", self.name, now.0, now.1, now.2);
            self.announced = Some(now);
        }
        gl_present::IMPORTED.fetch_add(1, Ordering::Relaxed);
        Ok(Shown { tex, egl_image, image, _sample: sample })
    }

    /// Deletes textures nothing draws any more. Only called from the notifier (the
    /// window's context is current).
    fn release(&self, shown: impl IntoIterator<Item = Shown>) {
        let (Some(gl), Some(egl)) = (&self.gl, &self.egl) else { return };
        for shown in shown {
            unsafe {
                (gl.delete_textures)(1, &shown.tex);
                (egl.destroy_image)(egl.display, shown.egl_image);
            }
        }
    }

    /// Takes the newest picture, if this window shows the stream. The window's picture
    /// property is written here and never read: this runs while Slint records what the
    /// frame depends on, and a read would make every later change of the property redraw
    /// the window, shown or not.
    fn before_rendering(&mut self) {
        // What the previous frame replaced: a whole frame has been drawn without it since.
        let retired = std::mem::take(&mut self.retired);
        self.release(retired);
        let emptied = self.emptied.replace(false);
        if !gl_present::ready() {
            return;
        }
        let Some(peer) = (self.wants)() else { return };
        let frame = gl_present::latest(peer).filter(|f| f.seq != self.last_seq.get() || self.current.is_none());
        let Some(frame) = frame.filter(|_| !screen_capture::is_stream_recently_stopped(peer)) else {
            // The picture the wake-up announced is gone again (its decoder was released):
            // the last one stays on screen.
            if let Some(cur) = self.current.as_ref().filter(|_| emptied) {
                (self.show)(cur.image.clone());
            }
            return;
        };
        self.last_seq.set(frame.seq);
        match unsafe { self.import(frame.sample) } {
            Ok(shown) => {
                (self.show)(shown.image.clone());
                self.retired.extend(self.current.replace(shown));
            }
            Err(e) => {
                // Nothing is shown from a picture that could not be imported: the window
                // goes blank until the stream is back as pixels, from its next keyframe.
                (self.show)(slint::Image::default());
                self.retired.extend(self.current.take());
                gl_present::give_up(&format!("{} window: {}", self.name, e));
            }
        }
        if cfg!(debug_assertions) {
            if let Some(gl) = &self.gl {
                let err = unsafe { (gl.get_error)() };
                if err != 0 {
                    log::warn!("🎞️ [GL VIDEO] {} window: GL error 0x{:x} after importing a picture", self.name, err);
                }
            }
        }
    }

    /// Lets go of the texture once this window should not show it any more: the stream
    /// ended or moved to the other window, pixels took over, or the picture property
    /// holds something else by now. The frame has been drawn, so the property can be read.
    fn after_rendering(&mut self) {
        let Some(cur) = &self.current else { return };
        let ours = (self.shown)() == cur.image;
        if ours && gl_present::ready() && (self.wants)().is_some() {
            return;
        }
        if ours {
            (self.show)(slint::Image::default());
        }
        // Nothing of ours is on screen after this frame, which is complete.
        let gone: Vec<Shown> = self.current.take().into_iter().chain(std::mem::take(&mut self.retired)).collect();
        self.release(gone);
    }

    /// The context is going away (the popout was hidden): everything made in it goes too.
    fn teardown(&mut self) {
        if self.current.as_ref().is_some_and(|cur| (self.shown)() == cur.image) {
            (self.show)(slint::Image::default());
        }
        let gone: Vec<Shown> = self.current.take().into_iter().chain(std::mem::take(&mut self.retired)).collect();
        self.release(gone);
        self.last_seq.set(0);
        self.gl = None;
        self.egl = None;
    }

    fn notify(&mut self, state: slint::RenderingState, api: &slint::GraphicsAPI<'_>) {
        match state {
            slint::RenderingState::RenderingSetup => self.setup(api),
            slint::RenderingState::BeforeRendering => {
                if self.gl.is_none() && !gl_present::is_off() {
                    // The setup notification comes once per window: it is missed when the
                    // hook is installed late, and after the context was recreated.
                    self.setup(api);
                }
                self.before_rendering();
            }
            slint::RenderingState::AfterRendering => self.after_rendering(),
            slint::RenderingState::RenderingTeardown => self.teardown(),
            _ => {}
        }
    }
}

thread_local! {
    /// What the UI thread does when the decoder says a new picture waits (see `install`).
    static POKE: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Puts a presenter into each window. Must run before the windows draw their first frame.
pub fn install(app: &AppWindow, popout: &PopoutStreamWindow) {
    let (main_seq, main_emptied) = (Rc::new(Cell::new(0u64)), Rc::new(Cell::new(false)));
    let (pop_seq, pop_emptied) = (Rc::new(Cell::new(0u64)), Rc::new(Cell::new(false)));

    let parse = |s: slint::SharedString| s.parse::<u64>().ok().filter(|&id| id != 0);
    let main_wants = {
        let weak = app.as_weak();
        move || {
            let app = weak.upgrade()?;
            let peer = app.get_active_stream_user_id();
            let shown = app.get_has_active_stream() && screen_capture::main_window_watching() && app.get_popped_out_stream_uid() != peer;
            parse(peer).filter(|_| shown)
        }
    };
    let pop_wants = {
        let weak = popout.as_weak();
        move || {
            let pop = weak.upgrade()?;
            parse(pop.get_user_id()).filter(|_| !pop.get_is_self() && screen_capture::popout_watching())
        }
    };

    let mut main = Presenter {
        name: "main",
        main: true,
        gl: None,
        egl: None,
        wants: Box::new(main_wants.clone()),
        shown: Box::new({
            let weak = app.as_weak();
            move || weak.upgrade().map(|a| a.get_remote_stream_frame()).unwrap_or_default()
        }),
        show: Box::new({
            let weak = app.as_weak();
            move |image| {
                if let Some(a) = weak.upgrade() {
                    a.set_remote_stream_frame(image);
                }
            }
        }),
        current: None,
        retired: Vec::new(),
        last_seq: main_seq.clone(),
        emptied: main_emptied.clone(),
        imported: 0,
        announced: None,
    };
    if let Err(e) = app.window().set_rendering_notifier(move |state, api| main.notify(state, api)) {
        // The software renderer has no such hook (and nothing to import a DMABuf into).
        gl_present::give_up(&format!("the window is not drawn with OpenGL ({e:?})"));
        return;
    }
    let mut pop = Presenter {
        name: "popout",
        main: false,
        gl: None,
        egl: None,
        wants: Box::new(pop_wants.clone()),
        shown: Box::new({
            let weak = popout.as_weak();
            move || weak.upgrade().map(|p| p.get_stream_frame()).unwrap_or_default()
        }),
        show: Box::new({
            let weak = popout.as_weak();
            move |image| {
                if let Some(p) = weak.upgrade() {
                    p.set_stream_frame(image);
                }
            }
        }),
        current: None,
        retired: Vec::new(),
        last_seq: pop_seq.clone(),
        emptied: pop_emptied.clone(),
        imported: 0,
        announced: None,
    };
    if let Err(e) = popout.window().set_rendering_notifier(move |state, api| pop.notify(state, api)) {
        gl_present::give_up(&format!("the popout is not drawn with OpenGL ({e:?})"));
        return;
    }

    // A new picture waits (called on the decode thread). A window draws a frame for it
    // only if something in it shows the picture: emptying the picture property makes
    // Slint redraw exactly then, and the presenter fills it in again before that frame
    // is drawn. A window whose stream card is scrolled away or covered by another view
    // has nothing bound to the property and is left alone.
    let (app_weak, pop_weak) = (app.as_weak(), popout.as_weak());
    let poke = move || {
        let offered = |peer: Option<u64>, seq: &Cell<u64>| peer.and_then(gl_present::latest).is_some_and(|f| f.seq != seq.get());
        if offered(main_wants(), &main_seq) {
            if let Some(a) = app_weak.upgrade() {
                a.set_remote_stream_frame(slint::Image::default());
                main_emptied.set(true);
            }
        }
        if offered(pop_wants(), &pop_seq) {
            if let Some(p) = pop_weak.upgrade() {
                p.set_stream_frame(slint::Image::default());
                pop_emptied.set(true);
            }
        }
    };
    POKE.with(|p| *p.borrow_mut() = Some(Box::new(poke)));
    // At most one event is on its way to the UI thread at any time.
    static POSTED: AtomicBool = AtomicBool::new(false);
    gl_present::set_wake(Box::new(|| {
        if POSTED.swap(true, Ordering::AcqRel) {
            return;
        }
        let posted = slint::invoke_from_event_loop(|| {
            POSTED.store(false, Ordering::Release);
            POKE.with(|p| {
                if let Some(poke) = p.borrow().as_ref() {
                    poke();
                }
            });
        });
        if posted.is_err() {
            POSTED.store(false, Ordering::Release);
        }
    }));
}
