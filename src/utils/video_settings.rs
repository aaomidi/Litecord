use std::sync::{Mutex, OnceLock};
use serde::{Deserialize, Serialize};
use log::info;

pub const VIDEO_SETTINGS_FILE: &str = ".litecord_video_settings.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoSettings {
    pub video_encoder: String,             // "auto", "gpu:VVVV:DDDD" (Windows), "openh264", or legacy encoder names
    pub video_capture_backend: String,     // "auto", "printwindow", "bitblt" (Win) / "auto", "portal", "x11" (Linux)
    pub audio_loopback_backend: String,    // "auto", "wasapi_isolated", "cpal" (Win) / "auto", "pulsesrc", "cpal" (Linux)
    pub enable_self_preview_notice: bool, // default true
    #[serde(default = "default_true")]
    pub show_hardware_monitor: bool,      // default true
    #[serde(default)]
    pub use_light_theme: bool,            // default false
    /// Draw the UI with the GPU (OpenGL) instead of the CPU software renderer. Off by
    /// default: idle in the tray the software renderer costs almost nothing and does not
    /// keep a GPU context (and its driver threads) next to the game.
    #[serde(default)]
    pub use_gpu_renderer: bool,           // default false
    /// Width of the channel list, in logical pixels, as the user dragged it.
    #[serde(default = "default_channel_sidebar_width")]
    pub channel_sidebar_width: f32,
    #[serde(default = "default_noise_suppression")]
    pub noise_suppression: String,        // "advanced", "standard", "none" (default "none")
    #[serde(default)]
    pub echo_cancellation: bool,         // default false
    #[serde(default)]
    pub automatic_gain_control: bool,    // default false
    /// Where the main window was when it last moved or resized. None until it does.
    #[serde(default)]
    pub window: Option<WindowGeometry>,
    /// Stream quality (advanced), read when a share starts. `stream_min_qp`: the VA-API
    /// encoder's QP floor. `stream_max_bitrate_mbps`: where a stream starts and climbs
    /// back to. `stream_profile`: "auto", "compatible" or "efficient" (see `StreamProfile`).
    /// None (never set, or not a value of that kind in the file) is the default.
    #[serde(default, deserialize_with = "lenient")]
    pub stream_min_qp: Option<u32>,
    #[serde(default, deserialize_with = "lenient")]
    pub stream_max_bitrate_mbps: Option<u32>,
    #[serde(default, deserialize_with = "lenient")]
    pub stream_profile: Option<String>,
}

/// Position in physical pixels (a logical position is ambiguous across monitors with
/// different scaling), size in logical pixels. While maximized the size kept is the one
/// the window returns to. Wayland gives no position, so it can be missing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub maximized: bool,
}

/// Smaller saved sizes are treated as damaged and ignored.
pub const WINDOW_MIN_WIDTH: u32 = 400;
pub const WINDOW_MIN_HEIGHT: u32 = 300;

pub const CHANNEL_SIDEBAR_MIN_WIDTH: f32 = 180.0;
pub const CHANNEL_SIDEBAR_MAX_WIDTH: f32 = 420.0;

fn default_channel_sidebar_width() -> f32 {
    230.0
}

fn default_true() -> bool {
    true
}

fn default_noise_suppression() -> String {
    "none".to_string()
}

pub const STREAM_MIN_QP_RANGE: (u32, u32) = (18, 36);
pub const STREAM_MIN_QP_DEFAULT: u32 = 30;
pub const STREAM_MAX_BITRATE_MBPS_RANGE: (u32, u32) = (3, 50);
pub const STREAM_MAX_BITRATE_MBPS_DEFAULT: u32 = 8;

/// A hand-edited value of the wrong kind costs that setting, not the whole file.
fn lenient<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    Ok(serde_json::Value::deserialize(d).ok().and_then(|v| serde_json::from_value(v).ok()))
}

/// Which H.264 profile a stream uses where the encoder has a choice (VA-API).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamProfile {
    /// Main while every viewer says it decodes it, Constrained Baseline otherwise.
    Auto,
    /// Constrained Baseline, which every receiver decodes.
    Compatible,
    /// Main, whoever watches.
    Efficient,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            video_encoder: "auto".to_string(),
            video_capture_backend: "auto".to_string(),
            audio_loopback_backend: "auto".to_string(),
            enable_self_preview_notice: true,
            show_hardware_monitor: true,
            use_light_theme: false,
            use_gpu_renderer: false,
            channel_sidebar_width: default_channel_sidebar_width(),
            noise_suppression: "none".to_string(),
            echo_cancellation: false,
            automatic_gain_control: false,
            window: None,
            stream_min_qp: None,
            stream_max_bitrate_mbps: None,
            stream_profile: None,
        }
    }
}

static VIDEO_SETTINGS: OnceLock<Mutex<VideoSettings>> = OnceLock::new();

pub fn get_video_settings_store() -> &'static Mutex<VideoSettings> {
    VIDEO_SETTINGS.get_or_init(|| {
        let settings = if let Ok(data) = std::fs::read_to_string(VIDEO_SETTINGS_FILE) {
            // Tolerate a BOM (editors on Windows add one) instead of silently resetting.
            serde_json::from_str::<VideoSettings>(data.trim_start_matches('\u{feff}')).unwrap_or_default()
        } else {
            VideoSettings::default()
        };
        info!("⚙️ Configurações de Vídeo e Interface carregadas: {:?}", settings);
        Mutex::new(settings)
    })
}

pub fn save_video_settings(settings: &VideoSettings) {
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(VIDEO_SETTINGS_FILE, json);
    }
}

pub fn get_video_encoder() -> String {
    get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).video_encoder.clone()
}

pub fn set_video_encoder(val: String) {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.video_encoder = val;
    save_video_settings(&store);
}

pub fn get_video_capture_backend() -> String {
    get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).video_capture_backend.clone()
}

pub fn set_video_capture_backend(val: String) {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.video_capture_backend = val;
    save_video_settings(&store);
}

pub fn get_audio_loopback_backend() -> String {
    get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).audio_loopback_backend.clone()
}

pub fn set_audio_loopback_backend(val: String) {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.audio_loopback_backend = val;
    save_video_settings(&store);
}

pub fn get_enable_self_preview_notice() -> bool {
    get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).enable_self_preview_notice
}

pub fn set_enable_self_preview_notice(val: bool) {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.enable_self_preview_notice = val;
    save_video_settings(&store);
}

pub fn toggle_enable_self_preview_notice() -> bool {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.enable_self_preview_notice = !store.enable_self_preview_notice;
    let new_val = store.enable_self_preview_notice;
    save_video_settings(&store);
    new_val
}


pub fn get_show_hardware_monitor() -> bool {
    get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).show_hardware_monitor
}

pub fn set_show_hardware_monitor(val: bool) {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.show_hardware_monitor = val;
    save_video_settings(&store);
}

pub fn toggle_show_hardware_monitor() -> bool {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.show_hardware_monitor = !store.show_hardware_monitor;
    let new_val = store.show_hardware_monitor;
    save_video_settings(&store);
    new_val
}

pub fn get_channel_sidebar_width() -> f32 {
    let width = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).channel_sidebar_width;
    width.clamp(CHANNEL_SIDEBAR_MIN_WIDTH, CHANNEL_SIDEBAR_MAX_WIDTH)
}

pub fn set_channel_sidebar_width(width: f32) {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.channel_sidebar_width = width.clamp(CHANNEL_SIDEBAR_MIN_WIDTH, CHANNEL_SIDEBAR_MAX_WIDTH);
    save_video_settings(&store);
}

pub fn get_window_geometry() -> Option<WindowGeometry> {
    get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).window
        .filter(|g| g.width >= WINDOW_MIN_WIDTH && g.height >= WINDOW_MIN_HEIGHT)
}

/// Writes the file only when the geometry actually changed.
pub fn set_window_geometry(geometry: WindowGeometry) {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    if store.window != Some(geometry) {
        store.window = Some(geometry);
        save_video_settings(&store);
    }
}

pub fn get_use_gpu_renderer() -> bool {
    get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).use_gpu_renderer
}

/// Takes effect on the next start: the renderer is chosen before the window exists.
pub fn toggle_use_gpu_renderer() -> bool {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.use_gpu_renderer = !store.use_gpu_renderer;
    let new_val = store.use_gpu_renderer;
    save_video_settings(&store);
    new_val
}

pub fn get_use_light_theme() -> bool {
    get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).use_light_theme
}

pub fn toggle_use_light_theme() -> bool {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.use_light_theme = !store.use_light_theme;
    let new_val = store.use_light_theme;
    save_video_settings(&store);
    new_val
}

pub fn get_noise_suppression() -> String {
    let mode = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).noise_suppression.clone();
    if mode == "krisp" {
        "advanced".to_string()
    } else {
        mode
    }
}

pub fn set_noise_suppression(val: String) {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.noise_suppression = val;
    save_video_settings(&store);
}

pub fn get_echo_cancellation() -> bool {
    get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).echo_cancellation
}

pub fn toggle_echo_cancellation() -> bool {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.echo_cancellation = !store.echo_cancellation;
    let new_val = store.echo_cancellation;
    save_video_settings(&store);
    new_val
}

pub fn get_automatic_gain_control() -> bool {
    get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).automatic_gain_control
}

pub fn toggle_automatic_gain_control() -> bool {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.automatic_gain_control = !store.automatic_gain_control;
    let new_val = store.automatic_gain_control;
    save_video_settings(&store);
    new_val
}

pub fn get_stream_min_qp() -> u32 {
    let qp = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).stream_min_qp;
    qp.unwrap_or(STREAM_MIN_QP_DEFAULT).clamp(STREAM_MIN_QP_RANGE.0, STREAM_MIN_QP_RANGE.1)
}

/// The file is written only when the value changes (a slider calls this while it moves).
pub fn set_stream_min_qp(qp: u32) {
    let qp = qp.clamp(STREAM_MIN_QP_RANGE.0, STREAM_MIN_QP_RANGE.1);
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    if store.stream_min_qp != Some(qp) {
        store.stream_min_qp = Some(qp);
        save_video_settings(&store);
    }
}

pub fn get_stream_max_bitrate_mbps() -> u32 {
    let mbps = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).stream_max_bitrate_mbps;
    mbps.unwrap_or(STREAM_MAX_BITRATE_MBPS_DEFAULT).clamp(STREAM_MAX_BITRATE_MBPS_RANGE.0, STREAM_MAX_BITRATE_MBPS_RANGE.1)
}

pub fn get_stream_max_bitrate_bps() -> u32 {
    get_stream_max_bitrate_mbps() * 1_000_000
}

pub fn set_stream_max_bitrate_mbps(mbps: u32) {
    let mbps = mbps.clamp(STREAM_MAX_BITRATE_MBPS_RANGE.0, STREAM_MAX_BITRATE_MBPS_RANGE.1);
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    if store.stream_max_bitrate_mbps != Some(mbps) {
        store.stream_max_bitrate_mbps = Some(mbps);
        save_video_settings(&store);
    }
}

pub fn get_stream_profile() -> StreamProfile {
    match get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).stream_profile.as_deref() {
        Some("compatible") => StreamProfile::Compatible,
        Some("efficient") => StreamProfile::Efficient,
        _ => StreamProfile::Auto,
    }
}

pub fn set_stream_profile(val: String) {
    let mut store = get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
    store.stream_profile = Some(val);
    save_video_settings(&store);
}
