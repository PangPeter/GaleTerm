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

use crate::frame::{DesktopInput, FrameBuffer, SoftCursor};
use crate::keysym::keysym_from_keystroke;
use crate::scale::{layout_image, DesktopScaleMode, ScaleLayout};

#[derive(Clone, Debug)]
pub enum DesktopPaneEvent {
    Input(DesktopInput),
    /// User asked to reconnect a dropped session (overlay click / action).
    Reconnect,
}

pub struct DesktopPane {
    id: SharedString,
    width: u16,
    height: u16,
    image: Option<Arc<RenderImage>>,
    status: SharedString,
    /// Session ended; show disconnect overlay (last frame may still be visible).
    disconnected: bool,
    disconnect_reason: SharedString,
    scale_mode: DesktopScaleMode,
    buttons: u8,
    /// Last painted layout (pane + image), for mapping mouse → framebuffer.
    layout: Rc<Cell<ScaleLayout>>,
    /// Soft-cursor from VNC Cursor / XCursor (overlay).
    soft_cursor: Option<Arc<RenderImage>>,
    soft_cursor_hotspot: (u16, u16),
    soft_cursor_size: (u16, u16),
    /// Last pointer position in framebuffer coords (for soft-cursor paint).
    pointer_fb: Option<(u16, u16)>,
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
            disconnected: false,
            disconnect_reason: SharedString::default(),
            scale_mode: DesktopScaleMode::Fit,
            buttons: 0,
            layout: Rc::new(Cell::new(ScaleLayout::default())),
            soft_cursor: None,
            soft_cursor_hotspot: (0, 0),
            soft_cursor_size: (0, 0),
            pointer_fb: None,
            focus: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn scale_mode(&self) -> DesktopScaleMode {
        self.scale_mode
    }

    pub fn set_scale_mode(&mut self, mode: DesktopScaleMode, cx: &mut Context<Self>) {
        if self.scale_mode != mode {
            self.scale_mode = mode;
            cx.notify();
        }
    }

    pub fn cycle_scale_mode(&mut self, cx: &mut Context<Self>) {
        self.set_scale_mode(self.scale_mode.cycle(), cx);
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
        self.disconnected = false;
        self.disconnect_reason = SharedString::default();
        cx.notify();
    }

    pub fn set_status(&mut self, status: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = status.into();
        cx.notify();
    }

    pub fn set_frame(&mut self, frame: FrameBuffer, cx: &mut Context<Self>) {
        self.width = frame.width;
        self.height = frame.height;
        self.disconnected = false;
        if let Some(image) = render_frame(&frame) {
            self.image = Some(image);
        }
        cx.notify();
    }

    /// Apply a VNC soft-cursor shape (`None` clears / hides).
    pub fn set_soft_cursor(&mut self, cursor: Option<SoftCursor>, cx: &mut Context<Self>) {
        match cursor {
            Some(c) if c.width > 0 && c.height > 0 => {
                self.soft_cursor_hotspot = (c.hotspot_x, c.hotspot_y);
                self.soft_cursor_size = (c.width, c.height);
                self.soft_cursor = render_soft_cursor(&c);
            }
            _ => {
                self.soft_cursor = None;
                self.soft_cursor_size = (0, 0);
                self.soft_cursor_hotspot = (0, 0);
            }
        }
        cx.notify();
    }

    pub fn is_disconnected(&self) -> bool {
        self.disconnected
    }

    /// Show a disconnect overlay; keeps the last frame underneath if any.
    pub fn set_disconnected(&mut self, reason: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.disconnected = true;
        self.disconnect_reason = reason.into();
        self.buttons = 0;
        self.soft_cursor = None;
        self.pointer_fb = None;
        cx.notify();
    }

    /// Clear disconnect state and show a connecting status (for reconnect).
    pub fn begin_reconnect(&mut self, cx: &mut Context<Self>) {
        self.disconnected = false;
        self.disconnect_reason = SharedString::default();
        self.image = None;
        self.status = "Reconnecting…".into();
        self.buttons = 0;
        self.soft_cursor = None;
        self.pointer_fb = None;
        cx.notify();
    }

    pub fn request_reconnect(&mut self, cx: &mut Context<Self>) {
        if self.disconnected {
            cx.emit(DesktopPaneEvent::Reconnect);
        }
    }

    /// Push local clipboard (image preferred, else text) to the remote session.
    pub fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        if let Some(png) = clipboard_png(&item) {
            self.emit_input(DesktopInput::CutImage(png), cx);
            return;
        }
        let Some(text) = item.text() else {
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
        if self.disconnected || self.width == 0 || self.height == 0 {
            return None;
        }
        let layout = self.layout.get();
        let image = layout.image;
        let visible = image.intersect(&layout.pane);
        if visible.size.width <= px(0.) || visible.size.height <= px(0.) {
            return None;
        }
        if image.size.width <= px(0.) || image.size.height <= px(0.) {
            return None;
        }
        // Only accept hits on the visible portion of the image.
        let local_vis = position - visible.origin;
        if local_vis.x < px(0.)
            || local_vis.y < px(0.)
            || local_vis.x >= visible.size.width
            || local_vis.y >= visible.size.height
        {
            return None;
        }
        let local = position - image.origin;
        let x = ((local.x / image.size.width) * f32::from(self.width)).floor() as u16;
        let y = ((local.y / image.size.height) * f32::from(self.height)).floor() as u16;
        Some((
            x.min(self.width.saturating_sub(1)),
            y.min(self.height.saturating_sub(1)),
        ))
    }

    fn track_pointer(&mut self, x: u16, y: u16, cx: &mut Context<Self>) {
        if self.pointer_fb != Some((x, y)) {
            self.pointer_fb = Some((x, y));
            if self.soft_cursor.is_some() {
                cx.notify();
            }
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some((x, y)) = self.pointer_pos(ev.position) else {
            return;
        };
        self.track_pointer(x, y, cx);
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
        self.track_pointer(x, y, cx);
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
        self.track_pointer(x, y, cx);
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
        if self.disconnected {
            return;
        }
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
        if self.disconnected {
            return;
        }
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

fn render_soft_cursor(cursor: &SoftCursor) -> Option<Arc<RenderImage>> {
    let (w, h) = (u32::from(cursor.width), u32::from(cursor.height));
    // Same byte order as FrameBuffer → RenderImage path.
    let buffer = RgbaImage::from_raw(w, h, cursor.pixels.as_ref().to_vec())?;
    Some(Arc::new(RenderImage::new(smallvec![Frame::new(buffer)])))
}

/// Prefer a PNG payload from the system clipboard (re-encode other formats).
fn clipboard_png(item: &ClipboardItem) -> Option<Vec<u8>> {
    for entry in item.entries() {
        let ClipboardEntry::Image(img) = entry else {
            continue;
        };
        if matches!(img.format, ImageFormat::Png) {
            return Some(img.bytes.clone());
        }
        let format = match img.format {
            ImageFormat::Jpeg => image::ImageFormat::Jpeg,
            ImageFormat::Gif => image::ImageFormat::Gif,
            ImageFormat::Webp => image::ImageFormat::WebP,
            _ => continue,
        };
        let Ok(decoded) = image::load_from_memory_with_format(&img.bytes, format) else {
            continue;
        };
        let mut png = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png);
        use image::ImageEncoder;
        if encoder
            .write_image(
                decoded.to_rgba8().as_raw(),
                decoded.width(),
                decoded.height(),
                image::ExtendedColorType::Rgba8,
            )
            .is_ok()
        {
            return Some(png);
        }
    }
    None
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
        let layout_cell = self.layout.clone();
        let scale_mode = self.scale_mode;
        let remote_w = self.width;
        let remote_h = self.height;
        let scale_label: SharedString = if remote_w > 0 && remote_h > 0 {
            format!("{} · {}×{}", scale_mode.label(), remote_w, remote_h).into()
        } else {
            scale_mode.label().into()
        };
        let disconnected = self.disconnected;
        let disconnect_reason = self.disconnect_reason.clone();
        let soft_cursor = self.soft_cursor.clone();
        let soft_hot = self.soft_cursor_hotspot;
        let soft_size = self.soft_cursor_size;
        let pointer_fb = self.pointer_fb;
        // Soft-cursor is drawn in-canvas; keep Arrow until gpui gains an invisible style.
        let mouse_style = CursorStyle::Arrow;

        div()
            .id(self.id.clone())
            .key_context("GaleDesktop")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .overflow_hidden()
            .cursor(mouse_style)
            .on_action(cx.listener(|this, _: &Paste, _, cx| {
                if !this.disconnected {
                    this.paste_clipboard(cx);
                }
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.on_key(ev, cx)))
            .on_key_up(cx.listener(|this, ev: &KeyUpEvent, _, cx| this.on_key_up(ev, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    if this.disconnected {
                        this.request_reconnect(cx);
                    } else {
                        this.on_mouse_down(ev, cx);
                    }
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
                    move |bounds, _, _| {
                        let image_bounds = layout_image(scale_mode, bounds, remote_w, remote_h);
                        layout_cell.set(ScaleLayout {
                            pane: bounds,
                            image: image_bounds,
                        });
                    },
                    move |bounds, (), window, _cx| {
                        if let Some(image) = &image {
                            let image_bounds = layout_image(scale_mode, bounds, remote_w, remote_h);
                            let _ = window.paint_image(
                                bounds,
                                image_bounds,
                                Corners::default(),
                                image.clone(),
                                0,
                                false,
                            );
                            if let (Some(cur), Some((fb_x, fb_y))) = (&soft_cursor, pointer_fb) {
                                if remote_w > 0
                                    && remote_h > 0
                                    && soft_size.0 > 0
                                    && soft_size.1 > 0
                                {
                                    let sx = f32::from(image_bounds.size.width)
                                        / f32::from(remote_w);
                                    let sy = f32::from(image_bounds.size.height)
                                        / f32::from(remote_h);
                                    let cw = f32::from(soft_size.0) * sx;
                                    let ch = f32::from(soft_size.1) * sy;
                                    let ox = f32::from(image_bounds.origin.x)
                                        + (f32::from(fb_x) - f32::from(soft_hot.0)) * sx;
                                    let oy = f32::from(image_bounds.origin.y)
                                        + (f32::from(fb_y) - f32::from(soft_hot.1)) * sy;
                                    let cursor_bounds = Bounds {
                                        origin: point(px(ox), px(oy)),
                                        size: size(px(cw), px(ch)),
                                    };
                                    let _ = window.paint_image(
                                        bounds,
                                        cursor_bounds,
                                        Corners::default(),
                                        cur.clone(),
                                        0,
                                        false,
                                    );
                                }
                            }
                        }
                    },
                )
                .size_full(),
            )
            .when(self.image.is_none() && !disconnected, |s| {
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
            .when(self.image.is_some() && !disconnected, |s| {
                s.child(
                    div()
                        .absolute()
                        .right_3()
                        .bottom_3()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(theme.background.opacity(0.72))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(scale_label),
                )
            })
            .when(disconnected, |s| {
                let reason = disconnect_reason;
                s.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .bg(hsla(0., 0., 0., 0.55))
                        .cursor(CursorStyle::PointingHand)
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child("Disconnected"),
                        )
                        .when(!reason.is_empty(), |s| {
                            s.child(
                                div()
                                    .max_w(px(420.))
                                    .px_4()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(reason),
                            )
                        })
                        .child(
                            div()
                                .mt_2()
                                .text_xs()
                                .text_color(theme.accent)
                                .child("Click or ⌘R to reconnect"),
                        ),
                )
            })
    }
}
