//! Copy buttons: puts text on the system clipboard and reports whether it got there.

use slint::ComponentHandle;

/// Copies `text`; `done` hears whether the clipboard really holds it afterwards.
pub fn copy_text<C: ComponentHandle + 'static>(ui: &C, text: String, done: impl FnOnce(&C, bool) + 'static) {
    #[cfg(windows)]
    {
        let copied = copypasta::ClipboardContext::new()
            .and_then(|mut ctx| copypasta::ClipboardProvider::set_contents(&mut ctx, text))
            .is_ok();
        done(ui, copied);
    }
    #[cfg(not(windows))]
    {
        // A clipboard made here would be the X11 one even under Wayland, and an X11
        // selection dies with the object that set it. Slint's backend has the session's
        // own clipboard (the one the text inputs use) and keeps it alive.
        if !i_slint_backend_winit::set_clipboard_text(ui.window(), &text) {
            log::warn!("📋 A área de transferência recusou o texto copiado");
            return done(ui, false);
        }
        // Setting it reports no failure (a Wayland compositor ignores a window without
        // focus, for one) and lands asynchronously, so it is read back a moment later,
        // and once more in case the compositor was slow.
        confirm(ui.as_weak(), text, &[150, 450], done);
    }
}

/// Reads the clipboard back after each wait in `waits_ms` (not empty) until it holds `text`.
#[cfg(not(windows))]
fn confirm<C: ComponentHandle + 'static>(weak: slint::Weak<C>, text: String, waits_ms: &'static [u64], done: impl FnOnce(&C, bool) + 'static) {
    let (wait, later) = (waits_ms[0], &waits_ms[1..]);
    slint::Timer::single_shot(std::time::Duration::from_millis(wait), move || {
        let Some(ui) = weak.upgrade() else { return };
        let unix_lines = |s: &str| s.replace("\r\n", "\n");
        let copied = i_slint_backend_winit::clipboard_text(ui.window())
            .is_some_and(|now| unix_lines(&now) == unix_lines(&text));
        if !copied && !later.is_empty() {
            return confirm(weak, text, later, done);
        }
        if !copied {
            log::warn!("📋 O texto copiado não chegou à área de transferência");
        }
        done(&ui, copied);
    });
}
