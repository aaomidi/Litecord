//! Test-only remote control for the UI.
//!
//! Compiled only with `--features automation`, and even then it stays off unless
//! `LITECORD_AUTOMATION_PORT` is set. It listens on 127.0.0.1 only: this process
//! holds a Discord token, so the control port must never be reachable from the
//! network (use an SSH tunnel to drive a remote machine).
//!
//! Protocol: one JSON object per line in, one JSON object per line out.
//!
//! ```text
//! {"cmd":"state"}
//! {"cmd":"screenshot","path":"C:/tmp/shot.png"}
//! {"cmd":"click","x":120,"y":300}            logical pixels, see "size"
//! {"cmd":"move","x":120,"y":300}
//! {"cmd":"scroll","x":400,"y":300,"dy":-120}
//! {"cmd":"drag","x":30,"y":100,"to_x":30,"to_y":220}  press, move in steps, release
//! {"cmd":"type","text":"hello"}
//! {"cmd":"key","key":"Return"}               Return, Escape, Tab, Backspace, arrows...
//! {"cmd":"key","key":"c","ctrl":true}        a shortcut: Ctrl+C
//! {"cmd":"set","prop":"show_settings_modal","value":true}
//! {"cmd":"set","prop":"login_alert_message","value":"text"}  also login_alert_is_error,
//!                                            web_share_active, web_share_url
//! {"cmd":"set_language","code":"de"}
//! {"cmd":"video_encoder_options"}           choices and the current capture path
//! {"cmd":"select_video_encoder","id":"auto"} same callback as the settings radio button
//! {"cmd":"size"}
//! {"cmd":"minimize_to_tray"}                 same as the title bar button
//! {"cmd":"restore"}                          same as clicking the tray icon
//! {"cmd":"open_guild","name":"Server"}       select a server by exact name
//! {"cmd":"open_channel","name":"general"}    select a channel of the open server by exact
//!                                            name (a voice channel joins it); errors if absent
//! {"cmd":"leave_voice"}                      same as the leave button
//! {"cmd":"set_voice_flags","muted":true,"deafened":true}  the mute and deafen states a voice
//!                                            channel is joined with; only while in no channel
//!                                            (in one, the buttons tell Discord as well)
//! {"cmd":"stream_quality","min_qp":30,"max_bitrate":8}  the settings sliders' callbacks (either
//!                                            or both); replies with the values now in force
//! {"cmd":"quit"}                             same as the title bar's close button
//! {"cmd":"start_screen_share","res":"1080p","fps":60,"screen":0,"audio":false}
//! {"cmd":"stop_screen_share"}
//! {"cmd":"watch_stream","user_id":"123"}     Watch a peer's stream and open it in the main view
//! {"cmd":"unwatch_stream","user_id":"123"}   hide it (stop receiving it)
//! {"cmd":"preview_watch_note","name":"Alice"} voice stage with a made-up stream that does
//!                                            not answer Watch ("shown":false hides the note,
//!                                            "both":true: neither stream answers, "active":true:
//!                                            the other stream is on screen meanwhile)
//! {"cmd":"stream_fullscreen"}                toggle the stream full screen button
//! {"cmd":"resize","width":1920,"height":1080} set the window size in logical pixels
//! {"cmd":"web_share_start"}                  start sharing to browsers; replies with the link
//! {"cmd":"web_share_stop"}
//! {"cmd":"open_in_browser","user_id":"123"}  same as the "Open in browser" button of a stream
//! {"cmd":"demo_voice_stage","a":"Name"}      a made-up call (nothing is connected) with one
//!                                            stream not watched (named "a" if given) and one
//!                                            watched, to look at the voice stage without an
//!                                            account
//!                                            "filler":true also fills the server list, the
//!                                            channel list and the call like a real server
//! {"cmd":"demo_stream","path":"/x/a.h264","fps":60,"user_id":"3"}
//!                                            play an H.264 Annex B file, looping, as that
//!                                            peer's watched stream: its access units enter
//!                                            the decode queue as received frames do. Sent
//!                                            again with another file, the stream goes on
//!                                            with that one (a size change at a keyframe)
//! {"cmd":"demo_stream","drop":2}             leave the next 2 access units out (lost frames)
//! {"cmd":"demo_stream","pause":true}         feed nothing until "pause":false
//! {"cmd":"demo_stream","stop":true}          the stream ends, as when its sender stops
//! {"cmd":"demo_chat","n":120}                a made-up chat of n messages in the main view
//!                                            (after demo_voice_stage with "filler":true for
//!                                            the lists around it), to measure scrolling
//! {"cmd":"demo_message"}                    one more message appended to that chat as a
//!                                            received one is ("own":true: the user's own;
//!                                            "text" optional), to see whether the view follows
//! {"cmd":"perf"}                             counters for measurements: access units fed,
//!                                            pictures decoded, pictures given to the UI,
//!                                            and on Linux pictures offered as GPU buffers
//!                                            and imported by a window
//! {"cmd":"popout_fullscreen"}                the popout's maximize button
//! {"cmd":"popout_close"}                     the popout's close button
//! {"cmd":"popout_resize","width":800,"height":450} popout size in logical pixels
//! {"cmd":"popout_screenshot","path":"/tmp/p.png"}
//! {"cmd":"spawn_threads","n":100,"alloc":true} start and join n threads (that allocate):
//!                                            what a thread that exits costs in memory
//! ```

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use log::{info, warn};
use serde_json::{json, Value};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model, SharedString};

use crate::AppWindow;

thread_local! {
    /// The popout window, for the commands that act on it (UI thread).
    static POPOUT: std::cell::RefCell<Option<slint::Weak<crate::PopoutStreamWindow>>> = const { std::cell::RefCell::new(None) };
}

/// Tells the control server about the popout window.
pub fn set_popout(popout: &crate::PopoutStreamWindow) {
    POPOUT.with(|p| *p.borrow_mut() = Some(popout.as_weak()));
}

fn popout() -> Result<crate::PopoutStreamWindow, String> {
    POPOUT.with(|p| p.borrow().as_ref().and_then(|w| w.upgrade())).ok_or_else(|| "no popout window".to_string())
}

/// Pictures of received streams the UI thread took from the decoder (measurements).
pub static UI_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The demo stream: a file played into the decode queue by its own thread.
struct DemoStream {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    drop: std::sync::Arc<std::sync::atomic::AtomicU32>,
    peer: u64,
}

static DEMO_STREAM: std::sync::Mutex<Option<DemoStream>> = std::sync::Mutex::new(None);
static DEMO_FED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Sequence number of the last access unit played (fed, dropped or paused over): it goes
/// on when another file takes over, as a sender's does.
static DEMO_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The access units of an Annex B stream, start codes included. A new one starts at a
/// delimiter, parameter set or SEI that follows a slice, or at a picture's first slice.
fn split_access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut units = Vec::new();
    let (mut i, mut au_start, mut have_slice) = (0, None, false);
    while i + 3 < data.len() {
        if data[i] != 0 || data[i + 1] != 0 || data[i + 2] != 1 {
            i += 1;
            continue;
        }
        let code = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
        let kind = data[i + 3] & 0x1f;
        let slice = matches!(kind, 1 | 5);
        // first_mb_in_slice is 0 when the slice data starts with a 1 bit.
        let first_slice = slice && data.get(i + 4).is_some_and(|b| b & 0x80 != 0);
        if have_slice && (matches!(kind, 6..=9) || first_slice) {
            units.push(data[au_start.unwrap_or(0)..code].to_vec());
            au_start = Some(code);
            have_slice = false;
        }
        au_start.get_or_insert(code);
        have_slice |= slice;
        i += 3;
    }
    if have_slice {
        units.push(data[au_start.unwrap_or(0)..].to_vec());
    }
    units
}

fn demo_stream(cmd: &Value) -> Result<Value, String> {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    let mut demo = DEMO_STREAM.lock().unwrap_or_else(|e| e.into_inner());
    if cmd["stop"].as_bool().unwrap_or(false) {
        let Some(d) = demo.take() else { return Err("no demo stream".into()) };
        d.stop.store(true, Ordering::Relaxed);
        crate::screen_capture::trigger_stream_stopped(d.peer);
        return Ok(json!({ "ok": true }));
    }
    if let Some(n) = cmd["drop"].as_u64() {
        let d = demo.as_ref().ok_or("no demo stream")?;
        d.drop.store(n as u32, Ordering::Relaxed);
        return Ok(json!({ "ok": true }));
    }
    if let Some(on) = cmd["pause"].as_bool() {
        let d = demo.as_ref().ok_or("no demo stream")?;
        d.paused.store(on, Ordering::Relaxed);
        return Ok(json!({ "ok": true }));
    }
    let path = cmd["path"].as_str().ok_or("missing \"path\"")?;
    let fps = cmd["fps"].as_f64().unwrap_or(60.0).clamp(1.0, 240.0);
    let peer: u64 = cmd["user_id"].as_str().unwrap_or("3").parse().map_err(|_| "user_id is not a number")?;
    let name = cmd["name"].as_str().unwrap_or("Streaming, watched here").to_string();
    let data = std::fs::read(path).map_err(|e| format!("could not read {path}: {e}"))?;
    let units = split_access_units(&data);
    if units.is_empty() {
        return Err(format!("no access units in {path}"));
    }
    if let Some(old) = demo.take() {
        old.stop.store(true, Ordering::Relaxed);
    }
    let d = DemoStream { stop: Arc::new(AtomicBool::new(false)), paused: Arc::new(AtomicBool::new(false)), drop: Arc::new(AtomicU32::new(0)), peer };
    let (stop, paused, drop) = (d.stop.clone(), d.paused.clone(), d.drop.clone());
    let count = units.len();
    std::thread::Builder::new()
        .name("demo-stream".into())
        .spawn(move || {
            let period = Duration::from_secs_f64(1.0 / fps);
            let (mut start, mut n) = (std::time::Instant::now(), 0u64);
            let mut at = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let due = start + period.mul_f64(n as f64);
                let now = std::time::Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                    // Another file may have taken over meanwhile: nothing more from this one.
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                } else if now - due > Duration::from_millis(500) {
                    // Far behind (the machine slept): start pacing again from here.
                    (start, n) = (now, 0);
                }
                let unit = &units[at % units.len()];
                let seq = DEMO_SEQ.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                at += 1;
                n += 1;
                if paused.load(Ordering::Relaxed) {
                    continue;
                }
                let dropping = drop.load(Ordering::Relaxed);
                if dropping > 0 {
                    drop.store(dropping - 1, Ordering::Relaxed);
                    continue;
                }
                if let Some(feed) = crate::screen_capture::DEMO_FEED.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    feed(peer, name.clone(), fps.round() as u8, seq, (at as f64 * 1000.0 / fps) as u32, unit.clone());
                    DEMO_FED.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    *demo = Some(d);
    Ok(json!({ "ok": true, "access_units": count, "fps": fps, "user_id": peer.to_string() }))
}

/// Starts the control server if `LITECORD_AUTOMATION_PORT` holds a port number.
pub fn start(ui: &AppWindow) {
    let port: u16 = match std::env::var("LITECORD_AUTOMATION_PORT").ok().and_then(|p| p.trim().parse().ok()) {
        Some(p) => p,
        None => return,
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            warn!("🤖 [AUTOMATION] Não foi possível escutar em 127.0.0.1:{}: {:?}", port, e);
            return;
        }
    };
    warn!("🤖 [AUTOMATION] Controle remoto de TESTE ativo em 127.0.0.1:{} (build com --features automation)", port);

    // A few long-lived threads serve the connections: a thread per connection would
    // leave some heap behind on every command and skew the memory the tests measure.
    let (tx, rx) = mpsc::channel::<TcpStream>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    for _ in 0..4 {
        let (rx, weak) = (rx.clone(), ui.as_weak());
        let _ = std::thread::Builder::new().name("automation-conn".into()).spawn(move || loop {
            let next = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
            match next {
                Ok(stream) => serve(stream, weak.clone()),
                Err(_) => return,
            }
        });
    }
    let _ = std::thread::Builder::new().name("automation".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = tx.send(stream);
        }
    });
}

fn serve(stream: TcpStream, weak: slint::Weak<AppWindow>) {
    let mut out = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    for line in BufReader::new(stream).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => return,
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(cmd) if cmd["cmd"] == "web_share_answer" => {
                match serde_json::from_value(cmd["offer"].clone()) {
                    Ok(offer) => match crate::screen_capture::web_share::answer_manual(offer) {
                        Ok(answer) => json!({"ok":true,"answer":answer}),
                        Err(error) => json!({"ok":false,"error":error}),
                    },
                    Err(e) => json!({"ok":false,"error":e.to_string()}),
                }
            },
            Ok(cmd) => run_on_ui_thread(&weak, cmd),
            Err(e) => json!({ "ok": false, "error": format!("invalid json: {e}") }),
        };
        if writeln!(out, "{reply}").is_err() {
            return;
        }
    }
}

fn run_on_ui_thread(weak: &slint::Weak<AppWindow>, cmd: Value) -> Value {
    let (tx, rx) = mpsc::channel();
    let weak = weak.clone();
    let posted = slint::invoke_from_event_loop(move || {
        let reply = match weak.upgrade() {
            Some(ui) => execute(&ui, &cmd).unwrap_or_else(|e| json!({ "ok": false, "error": e })),
            None => json!({ "ok": false, "error": "window is gone" }),
        };
        let _ = tx.send(reply);
    });
    if posted.is_err() {
        return json!({ "ok": false, "error": "event loop is not running" });
    }
    rx.recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|_| json!({ "ok": false, "error": "timed out waiting for the UI thread" }))
}

fn execute(ui: &AppWindow, cmd: &Value) -> Result<Value, String> {
    let name = cmd["cmd"].as_str().ok_or("missing \"cmd\"")?;
    let window = ui.window();
    match name {
        "size" => {
            let size = window.size();
            let scale = window.scale_factor();
            use i_slint_backend_winit::WinitWindowAccessor;
            Ok(json!({
                "ok": true,
                "focused": window.with_winit_window(|w| w.has_focus()),
                "system_scale": window.with_winit_window(|w| w.scale_factor()),
                "physical": [size.width, size.height],
                "logical": [size.width as f32 / scale, size.height as f32 / scale],
                "scale": scale,
                "popout_physical": popout().ok().filter(|p| p.window().is_visible()).map(|p| { let s = p.window().size(); [s.width, s.height] }),
            }))
        }
        "screenshot" => {
            let path = cmd["path"].as_str().ok_or("missing \"path\"")?;
            let shot = window.take_snapshot().map_err(|e| format!("snapshot failed: {e}"))?;
            image::save_buffer(path, shot.as_bytes(), shot.width(), shot.height(), image::ExtendedColorType::Rgba8)
                .map_err(|e| format!("could not write {path}: {e}"))?;
            Ok(json!({ "ok": true, "path": path, "width": shot.width(), "height": shot.height() }))
        }
        "move" | "click" => {
            let position = position_of(cmd)?;
            window.dispatch_event(WindowEvent::PointerMoved { position });
            if name == "click" {
                let button = PointerEventButton::Left;
                window.dispatch_event(WindowEvent::PointerPressed { position, button });
                window.dispatch_event(WindowEvent::PointerReleased { position, button });
            }
            Ok(json!({ "ok": true }))
        }
        "drag" => {
            // Press at (x, y), move to (to_x, to_y) in steps, release there.
            let start = position_of(cmd)?;
            let end = LogicalPosition::new(
                cmd["to_x"].as_f64().ok_or("missing \"to_x\"")? as f32,
                cmd["to_y"].as_f64().ok_or("missing \"to_y\"")? as f32,
            );
            let button = PointerEventButton::Left;
            window.dispatch_event(WindowEvent::PointerMoved { position: start });
            window.dispatch_event(WindowEvent::PointerPressed { position: start, button });
            for i in 1..=10 {
                let k = i as f32 / 10.0;
                let position = LogicalPosition::new(start.x + (end.x - start.x) * k, start.y + (end.y - start.y) * k);
                window.dispatch_event(WindowEvent::PointerMoved { position });
            }
            window.dispatch_event(WindowEvent::PointerReleased { position: end, button });
            Ok(json!({ "ok": true }))
        }
        "scroll" => {
            let position = position_of(cmd)?;
            window.dispatch_event(WindowEvent::PointerMoved { position });
            window.dispatch_event(WindowEvent::PointerScrolled {
                position,
                delta_x: cmd["dx"].as_f64().unwrap_or(0.0) as f32,
                delta_y: cmd["dy"].as_f64().unwrap_or(0.0) as f32,
            });
            Ok(json!({ "ok": true }))
        }
        "type" => {
            let text = cmd["text"].as_str().ok_or("missing \"text\"")?;
            for c in text.chars() {
                press(ui, SharedString::from(c.to_string()));
            }
            Ok(json!({ "ok": true }))
        }
        "key" => {
            let key = cmd["key"].as_str().ok_or("missing \"key\"")?;
            if cmd["ctrl"].as_bool().unwrap_or(false) {
                // A shortcut such as Ctrl+C: the key is the letter.
                let control = SharedString::from(char::from(Key::Control).to_string());
                window.dispatch_event(WindowEvent::KeyPressed { text: control.clone() });
                press(ui, key.into());
                window.dispatch_event(WindowEvent::KeyReleased { text: control });
                return Ok(json!({ "ok": true }));
            }
            let key = match key {
                "Return" | "Enter" => Key::Return,
                "Escape" => Key::Escape,
                "Tab" => Key::Tab,
                "Backspace" => Key::Backspace,
                "Delete" => Key::Delete,
                "Up" => Key::UpArrow,
                "Down" => Key::DownArrow,
                "Left" => Key::LeftArrow,
                "Right" => Key::RightArrow,
                "PageUp" => Key::PageUp,
                "PageDown" => Key::PageDown,
                "Home" => Key::Home,
                "End" => Key::End,
                other => return Err(format!("unknown key {other}")),
            };
            press(ui, SharedString::from(char::from(key).to_string()));
            Ok(json!({ "ok": true }))
        }
        "set" => {
            let prop = cmd["prop"].as_str().ok_or("missing \"prop\"")?;
            let value = &cmd["value"];
            let as_bool = || value.as_bool().ok_or(format!("{prop} needs a boolean"));
            let as_int = || value.as_i64().map(|v| v as i32).ok_or(format!("{prop} needs an integer"));
            let as_text = || value.as_str().map(SharedString::from).ok_or(format!("{prop} needs a string"));
            match prop {
                "show_settings_modal" => ui.set_show_settings_modal(as_bool()?),
                "settings_active_tab" => ui.set_settings_active_tab(as_int()?),
                "show_stream_modal" => ui.set_show_stream_modal(as_bool()?),
                "show_web_share_modal" => ui.set_show_web_share_modal(as_bool()?),
                "is_voice_focused" => ui.set_is_voice_focused(as_bool()?),
                "show_user_menu" => ui.set_show_user_menu(as_bool()?),
                // Dialogs that otherwise need an account or a running share.
                "login_alert_message" => ui.set_login_alert_message(as_text()?),
                "login_alert_is_error" => ui.set_login_alert_is_error(as_bool()?),
                "web_share_active" => ui.set_web_share_active(as_bool()?),
                "web_share_url" => ui.set_web_share_url(as_text()?),
                other => return Err(format!("property {other} is not settable from automation")),
            }
            Ok(json!({ "ok": true }))
        }
        "set_language" => {
            let code = cmd["code"].as_str().ok_or("missing \"code\"")?;
            let lang = crate::utils::i18n::Language::from_code(code);
            crate::ui::helpers::apply_i18n_translations(ui, lang);
            Ok(json!({ "ok": true, "language": lang.code() }))
        }
        "video_encoder_options" => {
            let options: Vec<Value> = ui.get_video_encoder_options().iter().map(|o|
                json!({"id":o.id.to_string(),"name":o.name.to_string(),"description":o.desc.to_string(),"selected":o.is_selected})
            ).collect();
            let reply = json!({"ok":true,"options":options,"selected":ui.get_selected_video_encoder().to_string(),"capture":ui.get_stream_capture_status().to_string()});
            #[cfg(windows)]
            let reply = {
                let mut reply = reply;
                reply["gpu_path"] = json!(crate::screen_capture::gpu_tx::active());
                reply["stream_choice"] = json!(crate::screen_capture::gpu_tx::encoder_choice());
                reply
            };
            Ok(reply)
        }
        "select_video_encoder" => {
            let id = cmd["id"].as_str().ok_or("missing \"id\"")?;
            if !ui.get_video_encoder_options().iter().any(|o| o.id == id) {
                return Err(format!("no video encoder option {id}"));
            }
            ui.invoke_select_video_encoder(id.into());
            Ok(json!({"ok":true,"selected":ui.get_selected_video_encoder().to_string()}))
        }
        "spawn_threads" => {
            let n = cmd["n"].as_u64().unwrap_or(10);
            let alloc = cmd["alloc"].as_bool().unwrap_or(false);
            for _ in 0..n {
                let _ = std::thread::spawn(move || {
                    if alloc {
                        let v: Vec<String> = (0..2000).map(|i| format!("line {i}")).collect();
                        std::hint::black_box(&v);
                    }
                })
                .join();
            }
            Ok(json!({ "ok": true }))
        }
        "minimize_to_tray" => {
            ui.invoke_minimize_to_tray();
            Ok(json!({ "ok": true }))
        }
        "restore" => {
            crate::ui::helpers::APP_IS_VISIBLE.store(true, std::sync::atomic::Ordering::Relaxed);
            crate::ui::helpers::NEED_UI_REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
            crate::utils::wake_hardware_monitor();
            crate::ui::helpers::wake_ui_loops();
            use i_slint_backend_winit::WinitWindowAccessor;
            let _ = ui.window().with_winit_window(|w| {
                w.set_visible(true);
                w.set_minimized(false);
            });
            i_slint_backend_winit::redraw_whole_window(ui.window());
            Ok(json!({ "ok": true }))
        }
        "set_full_conversion" => {
            let on = cmd["on"].as_bool().ok_or("missing \"on\"")?;
            crate::utils::test_hooks::FULL_SIZE_CONVERSION.store(on, std::sync::atomic::Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        }
        "set_cpu_decode" => {
            let on = cmd["on"].as_bool().ok_or("missing \"on\"")?;
            crate::utils::test_hooks::CPU_DECODE.store(on, std::sync::atomic::Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        }
        "drop_gateway" => {
            crate::utils::test_hooks::DROP_GATEWAY.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        }
        "break_voice_output" => {
            crate::utils::test_hooks::BREAK_VOICE_OUTPUT.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "ok": true }))
        }
        "minimize" => {
            // Like the taskbar button: the window stays "visible" but has no area.
            use i_slint_backend_winit::WinitWindowAccessor;
            let _ = ui.window().with_winit_window(|w| w.set_minimized(true));
            Ok(json!({ "ok": true }))
        }
        "popout_stream" => {
            let uid = cmd["user_id"].as_str().ok_or("missing \"user_id\"")?;
            ui.invoke_popout_stream_window(uid.into(), "".into());
            Ok(json!({ "ok": true }))
        }
        "open_guild" => {
            let name = cmd["name"].as_str().ok_or("missing \"name\"")?;
            let guild = ui.get_guilds().iter().find(|g| g.name == name).ok_or(format!("no server named {name}"))?;
            ui.invoke_select_guild(guild.id.clone());
            Ok(json!({ "ok": true, "id": guild.id.to_string() }))
        }
        "open_channel" => {
            let name = cmd["name"].as_str().ok_or("missing \"name\"")?;
            let channel = ui
                .get_channels()
                .iter()
                .find(|c| c.name == name && !c.is_category)
                .ok_or(format!("no channel named {name} in the open server"))?;
            ui.invoke_select_channel(channel.id.clone());
            Ok(json!({ "ok": true, "id": channel.id.to_string(), "is_voice": channel.is_voice }))
        }
        "leave_voice" => {
            ui.invoke_leave_voice();
            Ok(json!({ "ok": true }))
        }
        "set_voice_flags" => {
            if ui.get_is_in_voice() {
                return Err("already in a voice channel".into());
            }
            if let Some(muted) = cmd["muted"].as_bool() {
                ui.set_is_muted(muted);
            }
            if let Some(deafened) = cmd["deafened"].as_bool() {
                ui.set_is_deafened(deafened);
                crate::gateway::set_self_deaf(deafened);
            }
            Ok(json!({ "ok": true, "is_muted": ui.get_is_muted(), "is_deafened": ui.get_is_deafened() }))
        }
        "stream_quality" => {
            let quality = ui.global::<crate::StreamQuality>();
            if let Some(qp) = cmd["min_qp"].as_i64() {
                quality.set_min_qp(qp as f32);
                quality.invoke_set_min_qp(qp as i32);
            }
            if let Some(mbps) = cmd["max_bitrate"].as_i64() {
                quality.set_max_bitrate(mbps as f32);
                quality.invoke_set_max_bitrate(mbps as i32);
            }
            Ok(json!({
                "ok": true,
                "min_qp": crate::video_settings::get_stream_min_qp(),
                "max_bitrate": crate::video_settings::get_stream_max_bitrate_mbps(),
                "profile": format!("{:?}", crate::video_settings::get_stream_profile()),
                "ui_min_qp": quality.get_min_qp(),
                "ui_max_bitrate": quality.get_max_bitrate(),
            }))
        }
        "quit" => {
            // The reply is not sent: the close handler exits the process.
            ui.invoke_close_window();
            Ok(json!({ "ok": true }))
        }
        "start_screen_share" => {
            if ui.get_is_screen_sharing() {
                return Err("already sharing".into());
            }
            if let Some(title) = cmd["window_title"].as_str() {
                let matches: Vec<_> = crate::screen_capture::list_capturable_windows().into_iter()
                    .filter(|w| w.title.contains(title)).collect();
                if matches.len() != 1 { return Err(format!("expected one window matching {title}, found {}", matches.len())); }
                ui.set_stream_source_tab("windows".into());
                ui.set_stream_selected_window_id(matches[0].id.clone().into());
            } else {
                ui.set_stream_source_tab("screens".into());
            }
            ui.set_stream_selected_res(cmd["res"].as_str().unwrap_or("1080p").into());
            ui.set_stream_selected_fps(cmd["fps"].as_i64().unwrap_or(60) as i32);
            ui.set_stream_selected_source_index(cmd["screen"].as_i64().unwrap_or(0) as i32);
            ui.set_stream_include_audio(cmd["audio"].as_bool().unwrap_or(false));
            ui.invoke_toggle_screen_share();
            Ok(json!({ "ok": true }))
        }
        "stop_screen_share" => {
            if ui.get_is_screen_sharing() {
                ui.invoke_toggle_screen_share();
            }
            Ok(json!({ "ok": true }))
        }
        "watch_stream" => {
            // Like the Watch button, then the stream in the main view.
            let uid = cmd["user_id"].as_str().ok_or("missing \"user_id\"")?;
            ui.invoke_unhide_stream(uid.into());
            ui.set_focused_stream_uid(uid.into());
            Ok(json!({ "ok": true }))
        }
        "unwatch_stream" => {
            // Like hiding the stream: stop receiving it.
            let uid = cmd["user_id"].as_str().ok_or("missing \"user_id\"")?;
            ui.invoke_hide_stream(uid.into());
            Ok(json!({ "ok": true }))
        }
        "preview_watch_note" => {
            // The voice stage with a made-up participant whose stream does not answer Watch
            // (layout check; works on the login screen, nothing is sent anywhere).
            let name = cmd["name"].as_str().unwrap_or("Alice");
            let flag = |key: &str, default: bool| cmd[key].as_bool().unwrap_or(default);
            let (shown, both, active) = (flag("shown", true), flag("both", false), flag("active", false));
            let row = |id: &str, name: &str, is_self: bool, note: bool| crate::VoiceParticipant {
                user_id: id.into(),
                username: name.into(),
                avatar_text: name.chars().next().unwrap_or('?').to_string().into(),
                volume: 1.0,
                is_self,
                is_streaming: !is_self,
                is_stream_hidden: !is_self && !active,
                watch_note: if note { crate::utils::i18n_ui::ui_text().watch_no_answer.replace("{}", name).into() } else { Default::default() },
                ..Default::default()
            };
            ui.set_voice_participants(std::rc::Rc::new(slint::VecModel::from(vec![row("1", "Me", true, false), row("2", name, false, shown), row("3", "Bob", false, both)])).into());
            ui.set_has_active_stream(active);
            ui.set_active_stream_user_id(if active { "3" } else { "" }.into());
            ui.set_is_logged_in(true);
            ui.set_is_in_voice(true);
            ui.set_is_voice_focused(true);
            Ok(json!({ "ok": true }))
        }
        "stream_fullscreen" => {
            ui.invoke_toggle_stream_fullscreen();
            Ok(json!({ "ok": true }))
        }
        "resize" => {
            let w = cmd["width"].as_f64().ok_or("missing \"width\"")? as f32;
            let h = cmd["height"].as_f64().ok_or("missing \"height\"")? as f32;
            ui.window().set_size(slint::LogicalSize::new(w, h));
            Ok(json!({ "ok": true }))
        }
        "web_share_start" => {
            if !ui.get_is_screen_sharing() { return Err("start a screen share first".into()); }
            if !ui.get_web_share_active() && !ui.get_web_share_pending() { ui.invoke_toggle_web_share(); }
            Ok(json!({ "ok": true, "pending": ui.get_web_share_pending(), "url": ui.get_web_share_url().to_string() }))
        }
        "open_in_browser" => {
            let uid = cmd["user_id"].as_str().ok_or("missing \"user_id\"")?;
            ui.invoke_open_stream_in_browser(uid.into());
            Ok(json!({ "ok": true, "pending": ui.get_web_link_pending_uid().to_string() }))
        }
        "demo_voice_stage" => {
            let person = |id: &str, name: &str, is_self: bool, is_streaming: bool, is_stream_hidden: bool| crate::VoiceParticipant {
                user_id: id.into(),
                username: name.into(),
                avatar_text: name.chars().next().unwrap_or('?').to_string().into(),
                avatar_url: "".into(),
                avatar_img: Default::default(),
                is_speaking: false,
                audio_level: 0.0,
                is_muted: false,
                volume: 1.0,
                priority: 0,
                is_self,
                is_streaming,
                is_stream_hidden,
                watch_note: Default::default(),
            };
            let people = vec![
                person("1", "Me", true, false, false),
                person("2", cmd["a"].as_str().unwrap_or("Streaming, not watched"), false, true, true),
                person("3", "Streaming, watched here", false, true, false),
            ];
            let mut people = people;
            if cmd["filler"].as_bool().unwrap_or(false) {
                // A server as a member sees it: servers on the left, categories with
                // text and voice channels, and a few more people in the call.
                for (i, name) in ["Bruna", "Caio", "Débora", "Estêvão"].iter().enumerate() {
                    let mut p = person(&(10 + i).to_string(), name, false, false, false);
                    p.is_muted = i == 1;
                    people.push(p);
                }
                let guilds: Vec<crate::GuildItem> = ["Amigos", "Rust BR", "Jogatina", "Trabalho", "Música", "Filmes", "Estudos", "Família", "Linux", "Testes"]
                    .iter()
                    .enumerate()
                    .map(|(i, name)| crate::GuildItem { id: (100 + i).to_string().into(), name: (*name).into(), icon: name.chars().next().unwrap_or('?').to_string().into(), has_image: false, icon_image: Default::default() })
                    .collect();
                ui.set_guilds(std::rc::Rc::new(slint::VecModel::from(guilds)).into());
                ui.set_active_guild_name("Amigos".into());
                let mut channels = Vec::new();
                let channel = |id: usize, name: &str, is_voice: bool, is_category: bool, parent: usize, unread: i32| crate::ChannelItem {
                    id: id.to_string().into(),
                    name: name.into(),
                    is_voice,
                    voice_count: if id == 1005 { 7 } else { 0 },
                    is_voice_member: false,
                    parent_channel_id: if parent == 0 { "".into() } else { parent.to_string().into() },
                    avatar_text: "".into(),
                    avatar_url: "".into(),
                    avatar_slot: -1,
                    is_category,
                    has_parent: parent != 0,
                    is_collapsed: false,
                    has_separator: false,
                    unread_count: unread,
                    emoji_id: "".into(),
                    emoji_slot: -1,
                    label: name.into(),
                };
                let text = ["geral", "avisos", "memes", "links", "clipes", "música", "off-topic", "ajuda", "projetos", "eventos"];
                for (c, category) in ["Informações", "Conversa", "Jogos", "Voz"].iter().enumerate() {
                    let parent = 1000 * (c + 1);
                    channels.push(channel(parent, &category.to_uppercase(), false, true, 0, 0));
                    for (i, name) in text.iter().enumerate() {
                        let voice = c == 3;
                        let name = if voice { format!("Sala {}", i + 1) } else { format!("{name}-{}", c + 1) };
                        channels.push(channel(parent + i + 1, &name, voice, false, parent, if i % 4 == 1 && !voice { 3 } else { 0 }));
                    }
                }
                ui.set_channels(std::rc::Rc::new(slint::VecModel::from(channels)).into());
                ui.set_current_voice_channel("Sala 5".into());
                ui.set_active_channel_name("geral-2".into());
                ui.set_user_tag("demo".into());
            }
            ui.set_voice_participants(std::rc::Rc::new(slint::VecModel::from(people)).into());
            ui.set_has_active_stream(true);
            ui.set_active_stream_user_id("3".into());
            ui.set_is_logged_in(true);
            ui.set_is_in_voice(true);
            ui.set_is_voice_focused(true);
            Ok(json!({ "ok": true }))
        }
        "web_share_stop" => {
            crate::screen_capture::web_share::stop();
            crate::screen_capture::web_link::stopped_by_hand();
            ui.set_web_share_active(false);
            ui.set_web_share_url("".into());
            Ok(json!({ "ok": true }))
        }
        "demo_stream" => demo_stream(cmd),
        "perf" => {
            let reply = json!({
                "ok": true,
                "fed": DEMO_FED.load(std::sync::atomic::Ordering::Relaxed),
                "decoded": crate::screen_capture::RX_FRAMES_SHOWN.load(std::sync::atomic::Ordering::Relaxed),
                "on_gpu": crate::screen_capture::RX_GPU_FRAMES.load(std::sync::atomic::Ordering::Relaxed),
                "ui_frames": UI_FRAMES.load(std::sync::atomic::Ordering::Relaxed),
            });
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            let reply = {
                use crate::screen_capture::hw_decode::gl_present;
                let mut reply = reply;
                reply["gl_ready"] = json!(gl_present::ready());
                reply["offered"] = json!(gl_present::OFFERED.load(std::sync::atomic::Ordering::Relaxed));
                reply["presented"] = json!(gl_present::IMPORTED.load(std::sync::atomic::Ordering::Relaxed));
                reply
            };
            Ok(reply)
        }
        "popout_fullscreen" => {
            popout()?.invoke_toggle_fullscreen();
            Ok(json!({ "ok": true }))
        }
        "popout_close" => {
            popout()?.invoke_close_popout();
            Ok(json!({ "ok": true }))
        }
        "popout_resize" => {
            let w = cmd["width"].as_f64().ok_or("missing \"width\"")? as f32;
            let h = cmd["height"].as_f64().ok_or("missing \"height\"")? as f32;
            popout()?.window().set_size(slint::LogicalSize::new(w, h));
            Ok(json!({ "ok": true }))
        }
        "popout_screenshot" => {
            let path = cmd["path"].as_str().ok_or("missing \"path\"")?;
            let shot = popout()?.window().take_snapshot().map_err(|e| format!("snapshot failed: {e}"))?;
            image::save_buffer(path, shot.as_bytes(), shot.width(), shot.height(), image::ExtendedColorType::Rgba8)
                .map_err(|e| format!("could not write {path}: {e}"))?;
            Ok(json!({ "ok": true, "path": path, "width": shot.width(), "height": shot.height() }))
        }
        "demo_chat" => {
            // A made-up chat of n messages built the way received ones are (parsed into
            // lines and blocks), shown instead of the voice stage: something long to scroll.
            let n = cmd["n"].as_u64().unwrap_or(30) as usize;
            let authors = ["Bruna", "Caio", "Débora", "Estêvão", "Me"];
            let texts = [
                "bom dia pessoal",
                "alguém viu o jogo ontem? foi **muito** bom, o segundo tempo principalmente. Tem o replay em https://example.com/replay se alguém quiser ver de novo, vale a pena.",
                "kkkkk",
                "vou testar isso mais tarde\ne depois conto como foi\nse der certo eu mando o link",
                "A longer paragraph to wrap over several lines: the quick brown fox jumps over the lazy dog, and then does it again, and again, until the line is long enough that it must wrap at least twice in a chat column of ordinary width, which is the usual case for people who write in paragraphs.",
                "ok",
                "`cargo build --release` demorou 4 minutos aqui",
            ];
            let weak = ui.as_weak();
            let messages: Vec<crate::ChatMessage> = (0..n)
                .map(|i| {
                    let author = authors[(i / 3) % authors.len()];
                    let mut links = Vec::new();
                    let lines = crate::gateway::parse_text_into_lines(&format!("{} #{i}", texts[i % texts.len()]), &mut links);
                    crate::ChatMessage {
                        id: (5000 + i).to_string().into(),
                        author: author.into(),
                        author_id: ((i / 3) % authors.len() + 1).to_string().into(),
                        show_header: i % 3 == 0,
                        day_label: if i % 40 == 0 { "15 de novembro de 2026".into() } else { "".into() },
                        avatar_initial: author.chars().next().unwrap_or('?').to_string().into(),
                        timestamp: format!("{:02}:{:02}", 8 + i / 60, i % 60).into(),
                        hover_timestamp: format!("{:02}:{:02}", 8 + i / 60, i % 60).into(),
                        content: texts[i % texts.len()].into(),
                        content_lines: crate::ui::helpers::map_message_lines(&lines, "demo", &weak),
                        embed_color: slint::Color::from_rgb_u8(88, 101, 242),
                        ..Default::default()
                    }
                })
                .collect();
            ui.set_messages(std::rc::Rc::new(slint::VecModel::from(messages)).into());
            ui.set_has_more_older_messages(cmd["older"].as_bool().unwrap_or(true));
            ui.set_is_logged_in(true);
            ui.set_is_voice_focused(false);
            ui.set_scroll_to_bottom_trigger(ui.get_scroll_to_bottom_trigger() + 1);
            Ok(json!({ "ok": true, "messages": n }))
        }
        "demo_message" => {
            // One more message appended to the open chat the way a received one is (pushed
            // onto the existing model, then the same follow-or-stay request), to measure what
            // an incoming message does to the scroll position. "own":true is the user's own.
            let own = cmd["own"].as_bool().unwrap_or(false);
            let text = cmd["text"].as_str().unwrap_or("mensagem nova que acabou de chegar\ncom duas linhas");
            let mut links = Vec::new();
            let lines = crate::gateway::parse_text_into_lines(text, &mut links);
            let messages = ui.get_messages();
            let n = slint::Model::row_count(&messages);
            let pinned_before = ui.get_chat_pinned_to_bottom();
            let message = crate::ChatMessage {
                id: (900000 + n).to_string().into(),
                author: if own { "Me".into() } else { "Bruna".into() },
                author_id: if own { "5".into() } else { "1".into() },
                show_header: true,
                avatar_initial: if own { "M".into() } else { "B".into() },
                timestamp: "agora".into(),
                hover_timestamp: "agora".into(),
                content: text.into(),
                content_lines: crate::ui::helpers::map_message_lines(&lines, "demo", &ui.as_weak()),
                embed_color: slint::Color::from_rgb_u8(88, 101, 242),
                ..Default::default()
            };
            match slint::Model::as_any(&messages).downcast_ref::<slint::VecModel<crate::ChatMessage>>() {
                Some(model) => model.push(message),
                None => return Err("the chat model is not a VecModel; run demo_chat first".into()),
            }
            crate::ui::helpers::request_chat_follow_new_message(ui.as_weak(), own);
            Ok(json!({ "ok": true, "messages": n + 1, "pinned_before": pinned_before }))
        }
        "state" => Ok(state(ui)),
        other => Err(format!("unknown cmd {other}")),
    }
}

fn position_of(cmd: &Value) -> Result<LogicalPosition, String> {
    let x = cmd["x"].as_f64().ok_or("missing \"x\"")?;
    let y = cmd["y"].as_f64().ok_or("missing \"y\"")?;
    Ok(LogicalPosition::new(x as f32, y as f32))
}

fn press(ui: &AppWindow, text: SharedString) {
    ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window().dispatch_event(WindowEvent::KeyReleased { text });
}

fn state(ui: &AppWindow) -> Value {
    let peer_streams: Vec<Value> = crate::screen_capture::PEER_STREAMING_STATES
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|m| m.iter().map(|(uid, on)| json!({ "uid": uid.to_string(), "streaming": on })).collect()))
        .unwrap_or_default();
    let peer_addrs: Vec<Value> = crate::screen_capture::LAST_SEEN_PEER_ADDR
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|m| m.iter().map(|(uid, addr)| json!({ "uid": uid.to_string(), "addr": addr.to_string() })).collect()))
        .unwrap_or_default();

    let my_id = crate::gateway::get_my_user_id().to_string();
    let facts = crate::utils::test_hooks::guild_facts().lock().map(|f| f.clone()).unwrap_or_default();
    let guilds: Vec<Value> = ui
        .get_guilds()
        .iter()
        .map(|g| {
            let fact = facts.get(g.id.as_str());
            json!({
                "id": g.id.to_string(),
                "name": g.name.to_string(),
                "owned": fact.map(|f| f.0 == "@me" || (!my_id.is_empty() && f.0 == my_id)),
                "members": fact.map(|f| f.1).filter(|n| *n > 0),
            })
        })
        .collect();
    let channels: Vec<Value> = ui
        .get_channels()
        .iter()
        .map(|c| json!({ "id": c.id.to_string(), "name": c.name.to_string(), "voice": c.is_voice, "category": c.is_category, "voice_count": c.voice_count, "voice_member": c.is_voice_member }))
        .collect();
    let people: Vec<Value> = ui
        .get_voice_participants()
        .iter()
        .map(|p| json!({ "id": p.user_id.to_string(), "self": p.is_self, "muted": p.is_muted, "streaming": p.is_streaming }))
        .collect();

    let mut state = json!({
        "ok": true,
        "guilds": guilds,
        "channels": channels,
        "voice_participants": people,
        "version": env!("CARGO_PKG_VERSION"),
        "language": ui.get_current_language_code().to_string(),
        "is_logged_in": ui.get_is_logged_in(),
        "connection_status": ui.get_connection_status().to_string(),
        "login_alert_message": ui.get_login_alert_message().to_string(),
        "has_qr_code": ui.get_has_qr_code(),
        "user_tag": ui.get_user_tag().to_string(),
        "user_id": crate::gateway::get_my_user_id().to_string(),
        "active_guild": ui.get_active_guild_name().to_string(),
        "active_channel": ui.get_active_channel_name().to_string(),
        "active_channel_id": ui.get_active_channel_id().to_string(),
        "show_settings_modal": ui.get_show_settings_modal(),
        "settings_active_tab": ui.get_settings_active_tab(),
        "show_stream_modal": ui.get_show_stream_modal(),
        "show_web_share_modal": ui.get_show_web_share_modal(),
        "web_share_copied": ui.get_web_share_copied(),
        "net_diag_copied": ui.get_net_diag_copied(),
        "voice": {
            "is_in_voice": ui.get_is_in_voice(),
            "is_connecting": ui.get_is_voice_connecting(),
            "channel": ui.get_current_voice_channel().to_string(),
            "channel_id": crate::gateway::get_my_voice_channel_id().to_string(),
            "is_muted": ui.get_is_muted(),
            "is_deafened": ui.get_is_deafened(),
        },
        "stream": {
            "web_active": ui.get_web_share_active(),
            "web_pending": ui.get_web_share_pending(),
            "web_url": ui.get_web_share_url().to_string(),
            "web_error": ui.get_web_share_error().to_string(),
            "web_viewers": crate::screen_capture::web_share::viewer_count(),
            "web_auto_started": crate::screen_capture::web_link::is_auto_started(),
            "web_link_pending": ui.get_web_link_pending_uid().to_string(),
            "allow_web_link_requests": ui.get_allow_web_link_requests(),
            "is_screen_sharing": ui.get_is_screen_sharing(),
            "has_active_stream": ui.get_has_active_stream(),
            "active_stream_user": ui.get_active_stream_user().to_string(),
            "remote_fps": ui.get_remote_stream_fps().to_string(),
            "local_fps": ui.get_local_preview_fps().to_string(),
            "frames_shown": crate::screen_capture::RX_FRAMES_SHOWN.load(std::sync::atomic::Ordering::Relaxed),
            "frames_unwatched": crate::screen_capture::RX_FRAMES_UNWATCHED.load(std::sync::atomic::Ordering::Relaxed),
            "frames_gpu": crate::screen_capture::RX_GPU_FRAMES.load(std::sync::atomic::Ordering::Relaxed),
            "peers_streaming": peer_streams,
            "peers_seen": peer_addrs,
        },
    });
    // (Added here: one more key would take `json!` past the macro recursion limit.)
    state["stream"]["share_notice"] = json!(ui.get_share_notice().to_string());
    state
}

