//! Геометрия вывода: где в окне лежит кадр гостя при сохранении пропорций (letterbox) и как
//! перевести точку окна в долю кадра для касаний.

/// Прямоугольник в пикселях окна.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Левый край.
    pub x: f32,
    /// Верхний край.
    pub y: f32,
    /// Ширина.
    pub w: f32,
    /// Высота.
    pub h: f32,
}

/// Кадр `frame_w`×`frame_h`, вписанный по центру в окно `win_w`×`win_h` с сохранением
/// пропорций; свободные полосы — сверху/снизу или слева/справа. Нулевые размеры дают пустой
/// прямоугольник.
#[must_use]
#[allow(clippy::cast_precision_loss, reason = "пиксели окна и кадра")]
pub fn fit(frame_w: u32, frame_h: u32, win_w: u32, win_h: u32) -> Rect {
    if frame_w == 0 || frame_h == 0 || win_w == 0 || win_h == 0 {
        return Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
    }
    let (fw, fh, ww, wh) = (frame_w as f32, frame_h as f32, win_w as f32, win_h as f32);
    let scale = (ww / fw).min(wh / fh);
    // Целые пиксели: без полупиксельного сдвига картинка не размывается при масштабе 1:1.
    let (w, h) = ((fw * scale).round(), (fh * scale).round());
    Rect {
        x: ((ww - w) / 2.0).floor(),
        y: ((wh - h) / 2.0).floor(),
        w,
        h,
    }
}

/// Точка окна `(px, py)` в долях кадра `rect`, приведённая к `0..=1` (касание на чёрной полосе
/// попадает на ближний край кадра).
#[must_use]
pub fn to_frame(rect: Rect, px: f32, py: f32) -> (f32, f32) {
    let fx = if rect.w > 0.0 {
        (px - rect.x) / rect.w
    } else {
        0.0
    };
    let fy = if rect.h > 0.0 {
        (py - rect.y) / rect.h
    } else {
        0.0
    };
    (clamp01(fx), clamp01(fy))
}

fn clamp01(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_aspect_fills_window() {
        let r = fit(1280, 720, 1920, 1080);
        assert_eq!(
            r,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0
            }
        );
    }

    #[test]
    fn wide_window_gets_side_bars() {
        // 16:9 в 2000×1000: высота 1000, ширина 1778, поля по 111 слева и справа.
        let r = fit(1280, 720, 2000, 1000);
        assert_eq!((r.w, r.h), (1778.0, 1000.0));
        assert_eq!((r.x, r.y), (111.0, 0.0));
    }

    #[test]
    fn tall_window_gets_top_bars() {
        // Портрет 720×1280 гостя в окне 1280×720 — полосы слева/справа; 16:9 в квадрате — сверху/снизу.
        let r = fit(1280, 720, 800, 800);
        assert_eq!((r.w, r.h), (800.0, 450.0));
        assert_eq!((r.x, r.y), (0.0, 175.0));
    }

    #[test]
    fn zero_sizes_are_empty() {
        assert_eq!(fit(0, 720, 800, 800).w, 0.0);
        assert_eq!(fit(1280, 720, 0, 0).h, 0.0);
    }

    #[test]
    fn window_point_to_frame() {
        let r = fit(1280, 720, 800, 800);
        assert_eq!(to_frame(r, 400.0, 400.0), (0.5, 0.5));
        assert_eq!(to_frame(r, 0.0, 175.0), (0.0, 0.0));
        // Чёрная полоса сверху — верхний край кадра.
        assert_eq!(to_frame(r, 800.0, 10.0), (1.0, 0.0));
        let empty = fit(0, 0, 0, 0);
        assert_eq!(to_frame(empty, 5.0, 5.0), (0.0, 0.0));
    }
}
