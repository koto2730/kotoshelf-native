//! Keeps the Windows IME on/off (open) state across text-field focus changes.
//!
//! egui-winit calls `Window::set_ime_allowed(false/true)` whenever a text
//! field loses/gains focus. winit's Windows backend implements that as
//! detaching/re-attaching the IME input context, and a freshly attached
//! context starts in the IME's default mode (usually direct/alphanumeric
//! input) - so Japanese input silently flips to OFF every time focus
//! passes through a non-text widget (menu, tree, dialog...).
//!
//! This remembers the open status while a text field has focus and
//! re-applies it on the first frame after the context is re-attached.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::UI::Input::Ime::{ImmGetContext, ImmGetOpenStatus, ImmReleaseContext, ImmSetOpenStatus};

#[derive(Default)]
pub struct ImeKeeper {
    saved_open: Option<bool>,
    prev_text_focus: bool,
    restore_pending: bool,
}

impl ImeKeeper {
    /// Call once per frame. `text_focus`: a text field wants keyboard input
    /// (i.e. egui is asking winit to allow IME).
    pub fn update(&mut self, frame: &eframe::Frame, window_focused: bool, text_focus: bool) {
        let text_focus = text_focus && window_focused;
        let Some(hwnd) = hwnd_of(frame) else { return };

        if text_focus {
            if !self.prev_text_focus {
                // egui-winit re-attaches the IME context at the end of this
                // frame; restore on the next one.
                self.restore_pending = self.saved_open.is_some();
            } else if self.restore_pending {
                if let Some(open) = self.saved_open {
                    with_context(hwnd, |himc| unsafe {
                        let _ = ImmSetOpenStatus(himc, BOOL::from(open));
                    });
                }
                self.restore_pending = false;
            } else if let Some(open) = with_context(hwnd, |himc| unsafe { ImmGetOpenStatus(himc).as_bool() }) {
                self.saved_open = Some(open);
            }
        } else {
            self.restore_pending = false;
        }
        self.prev_text_focus = text_focus;
    }
}

fn hwnd_of(frame: &eframe::Frame) -> Option<HWND> {
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut core::ffi::c_void)),
        _ => None,
    }
}

fn with_context<R>(hwnd: HWND, f: impl FnOnce(windows::Win32::UI::Input::Ime::HIMC) -> R) -> Option<R> {
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc.is_invalid() {
            return None;
        }
        let r = f(himc);
        let _ = ImmReleaseContext(hwnd, himc);
        Some(r)
    }
}
