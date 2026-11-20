//! Remote framebuffer placement inside the local pane.

use gpui_kit::{point, px, size, Bounds, Pixels};

/// How the remote framebuffer is placed inside the pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DesktopScaleMode {
    /// Preserve aspect ratio; letterbox inside the pane.
    #[default]
    Fit,
    /// One remote pixel ≈ one logical pixel; centered (may clip).
    Native,
    /// Stretch to fill the pane (may distort).
    Stretch,
}

impl DesktopScaleMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Fit => Self::Native,
            Self::Native => Self::Stretch,
            Self::Stretch => Self::Fit,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fit => "Fit",
            Self::Native => "1:1",
            Self::Stretch => "Stretch",
        }
    }
}

/// Painted image rect + pane clip, used for pointer mapping.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ScaleLayout {
    pub pane: Bounds<Pixels>,
    pub image: Bounds<Pixels>,
}

/// Place the remote framebuffer inside `pane` according to `mode`.
pub fn layout_image(
    mode: DesktopScaleMode,
    pane: Bounds<Pixels>,
    remote_w: u16,
    remote_h: u16,
) -> Bounds<Pixels> {
    let rw = f32::from(remote_w);
    let rh = f32::from(remote_h);
    if rw <= 0. || rh <= 0. {
        return pane;
    }
    let pw = f32::from(pane.size.width);
    let ph = f32::from(pane.size.height);
    if pw <= 0. || ph <= 0. {
        return pane;
    }
    let (iw, ih) = match mode {
        DesktopScaleMode::Stretch => (pw, ph),
        DesktopScaleMode::Fit => {
            let s = (pw / rw).min(ph / rh);
            (rw * s, rh * s)
        }
        DesktopScaleMode::Native => (rw, rh),
    };
    Bounds {
        origin: point(
            px(f32::from(pane.origin.x) + (pw - iw) * 0.5),
            px(f32::from(pane.origin.y) + (ph - ih) * 0.5),
        ),
        size: size(px(iw), px(ih)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(w), px(h)),
        }
    }

    #[test]
    fn fit_letterboxes_wide_remote() {
        let image = layout_image(DesktopScaleMode::Fit, rect(800., 600.), 1600, 900);
        // 1600:900 = 16:9 → height-limited in 800x600 → 800x450, centered Y.
        assert!((f32::from(image.size.width) - 800.).abs() < 0.01);
        assert!((f32::from(image.size.height) - 450.).abs() < 0.01);
        assert!((f32::from(image.origin.y) - 75.).abs() < 0.01);
    }

    #[test]
    fn native_centers_1_to_1() {
        let image = layout_image(DesktopScaleMode::Native, rect(800., 600.), 400, 300);
        assert!((f32::from(image.size.width) - 400.).abs() < 0.01);
        assert!((f32::from(image.size.height) - 300.).abs() < 0.01);
        assert!((f32::from(image.origin.x) - 200.).abs() < 0.01);
        assert!((f32::from(image.origin.y) - 150.).abs() < 0.01);
    }

    #[test]
    fn stretch_fills_pane() {
        let image = layout_image(DesktopScaleMode::Stretch, rect(800., 600.), 100, 100);
        assert!((f32::from(image.size.width) - 800.).abs() < 0.01);
        assert!((f32::from(image.size.height) - 600.).abs() < 0.01);
    }

    #[test]
    fn cycle_order() {
        assert_eq!(DesktopScaleMode::Fit.cycle(), DesktopScaleMode::Native);
        assert_eq!(DesktopScaleMode::Native.cycle(), DesktopScaleMode::Stretch);
        assert_eq!(DesktopScaleMode::Stretch.cycle(), DesktopScaleMode::Fit);
    }
}
