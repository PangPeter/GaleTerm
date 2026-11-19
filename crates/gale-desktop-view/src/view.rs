//! gpui entity that shows a remote framebuffer and reports input.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::input::Paste;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use image::{Frame, RgbaImage};
use smallvec::smallvec;

use crate::frame::{DesktopInput, FrameBuffer};
use crate::keysym::keysym_from_keystroke;

#[derive(Clone, Debug)]
pub enum DesktopPaneEvent {
    Input(DesktopInput),
}

pub struct DesktopPane {
    id: SharedString,
    width: u16,
    height: u16,
    image: Option<Arc<RenderImage>>,
    status: SharedString,
    buttons: u8,
    /// Last painted content bounds, for mapping mouse → framebuffer.
    content: Rc<Cell<Bounds<Pixels>>>,
    focus: FocusHandle,
}

impl DesktopPane {
    pub fn new(id: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            id: id.into(),
            width: 0,
            height: 0,
            image: None,
            status: "Connecting…".into(),
            buttons: 0,
            content: Rc::new(Cell::new(Bounds::default())),
            focus: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn set_ready(
        &mut self,
        width: u16,
        height: u16,
        name: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.width = width;
        self.height = height;
        self.status = name.into();
        cx.notify();
    }

    pub fn set_status(&mut self, status: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = status.into();
        cx.notify();
    }

    pub fn set_frame(&mut self, frame: FrameBuffer, cx: &mut Context<Self>) {
        self.width = frame.width;
        self.height = frame.height;
        if let Some(image) = render_frame(&frame) {
            self.image = Some(image);
        }
        cx.notify();
    }

    /// Push local clipboard text to the remote session (ClientCutText / RDP paste).
    pub fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.emit_input(DesktopInput::CutText(text), cx);
    }

    fn emit_input(&mut self, input: DesktopInput, cx: &mut Context<Self>) {
        cx.emit(DesktopPaneEvent::Input(input));
    }

    fn pointer_pos(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let bounds = self.content.get();
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return None;
        }
        let local = position - bounds.origin;
        if local.x < px(0.)
            || local.y < px(0.)
            || local.x >= bounds.size.width
            || local.y >= bounds.size.height
        {
            return None;
        }
        let x = ((local.x / bounds.size.width) * f32::from(self.width)).floor() as u16;
        let y = ((local.y / bounds.size.height) * f32::from(self.height)).floor() as u16;
        Some((
            x.min(self.width.saturating_sub(1)),
            y.min(self.height.saturating_sub(1)),
        ))
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some((x, y)) = self.pointer_pos(ev.position) else {
            return;
        };
        self.buttons |= button_mask(ev.button);
        self.emit_input(
            DesktopInput::Pointer {
                x,
                y,
                buttons: self.buttons,
            },
            cx,
        );
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some((x, y)) = self.pointer_pos(ev.position) else {
            return;
        };
        self.buttons &= !button_mask(ev.button);
        self.emit_input(
            DesktopInput::Pointer {
                x,
                y,
                buttons: self.buttons,
            },
            cx,
        );
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some((x, y)) = self.pointer_pos(ev.position) else {
            return;
        };
        self.emit_input(
            DesktopInput::Pointer {
                x,
                y,
                buttons: self.buttons,
            },
            cx,
        );
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, cx: &mut Context<Self>) {
        let Some((x, y)) = self.pointer_pos(ev.position) else {
            return;
        };
        let delta = ev.delta.pixel_delta(px(16.));
        let wheel = if delta.y > px(0.) {
            1 << 3
        } else if delta.y < px(0.) {
            1 << 4
        } else {
            return;
        };
        self.emit_input(
            DesktopInput::Pointer {
                x,
                y,
                buttons: self.buttons | wheel,
            },
            cx,
        );
        self.emit_input(
            DesktopInput::Pointer {
                x,
                y,
                buttons: self.buttons,
            },
            cx,
        );
    }

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        // ⌘/Ctrl+C/V are handled as clipboard actions, not remote keystrokes.
        if is_clipboard_chord(&ev.keystroke) {
            return;
        }
        let Some(keysym) = keysym_from_keystroke(&ev.keystroke) else {
            return;
        };
        self.emit_input(DesktopInput::Key { down: true, keysym }, cx);
        if !is_modifier(&ev.keystroke.key) {
            self.emit_input(DesktopInput::Key { down: false, keysym }, cx);
        }
    }

    fn on_key_up(&mut self, ev: &KeyUpEvent, cx: &mut Context<Self>) {
        if let Some(keysym) = keysym_from_keystroke(&ev.keystroke) {
            self.emit_input(DesktopInput::Key { down: false, keysym }, cx);
        }
    }
}

fn is_modifier(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "shift" | "control" | "ctrl" | "alt" | "option" | "meta" | "cmd" | "super" | "win"
    )
}

fn is_clipboard_chord(keystroke: &Keystroke) -> bool {
    let key = keystroke.key.to_ascii_lowercase();
    if key != "c" && key != "v" {
        return false;
    }
    if cfg!(target_os = "macos") {
        keystroke.modifiers.platform
    } else {
        keystroke.modifiers.control && keystroke.modifiers.shift
    }
}

fn button_mask(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1 << 0,
        MouseButton::Middle => 1 << 1,
        MouseButton::Right => 1 << 2,
        _ => 0,
    }
}

fn render_frame(frame: &FrameBuffer) -> Option<Arc<RenderImage>> {
    let (w, h) = (u32::from(frame.width), u32::from(frame.height));
    let buffer = RgbaImage::from_raw(w, h, frame.pixels.as_ref().to_vec())?;
    Some(Arc::new(RenderImage::new(smallvec![Frame::new(buffer)])))
}

impl EventEmitter<DesktopPaneEvent> for DesktopPane {}

impl Focusable for DesktopPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DesktopPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let image = self.image.clone();
        let status = self.status.clone();
        let content = self.content.clone();

        div()
            .id(self.id.clone())
            .key_context("GaleDesktop")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .overflow_hidden()
            .cursor(CursorStyle::Arrow)
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste_clipboard(cx)))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.on_key(ev, cx)))
            .on_key_up(cx.listener(|this, ev: &KeyUpEvent, _, cx| this.on_key_up(ev, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    this.on_mouse_down(ev, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| this.on_mouse_down(ev, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| this.on_mouse_down(ev, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _, cx| this.on_mouse_up(ev, cx)),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseUpEvent, _, cx| this.on_mouse_up(ev, cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, ev: &MouseUpEvent, _, cx| this.on_mouse_up(ev, cx)),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| this.on_mouse_move(ev, cx)))
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| this.on_scroll(ev, cx)))
            .child(
                canvas(
                    move |bounds, _, _| content.set(bounds),
                    move |bounds, (), window, _cx| {
                        if let Some(image) = &image {
                            let _ = window.paint_image(
                                bounds,
                                bounds,
                                Corners::default(),
                                image.clone(),
                                0,
                                false,
                            );
                        }
                    },
                )
                .size_full(),
            )
            .when(self.image.is_none(), |s| {
                s.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(status),
                )
            })
    }
}
