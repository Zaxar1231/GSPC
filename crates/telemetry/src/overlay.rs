//! Оверлей производительности: текст растровым шрифтом 5×7 и график времени кадра.
//!
//! Рисуется прямо в кадр BGRA/RGBA (одинаково для обоих порядков, цвета — оттенки серого и
//! один акцент в зелёном канале), один проход по небольшой области в углу экрана.

/// Глифы 5×7: по строке на ряд, `#` — пиксель.
const GLYPHS: &[(char, [&str; 7])] = &[
    (
        '0',
        [
            ".###.", "#...#", "#..##", "#.#.#", "##..#", "#...#", ".###.",
        ],
    ),
    (
        '1',
        [
            "..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###.",
        ],
    ),
    (
        '2',
        [
            ".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####",
        ],
    ),
    (
        '3',
        [
            "####.", "....#", "....#", ".###.", "....#", "....#", "####.",
        ],
    ),
    (
        '4',
        [
            "...#.", "..##.", ".#.#.", "#..#.", "#####", "...#.", "...#.",
        ],
    ),
    (
        '5',
        [
            "#####", "#....", "####.", "....#", "....#", "#...#", ".###.",
        ],
    ),
    (
        '6',
        [
            ".###.", "#....", "#....", "####.", "#...#", "#...#", ".###.",
        ],
    ),
    (
        '7',
        [
            "#####", "....#", "...#.", "..#..", ".#...", ".#...", ".#...",
        ],
    ),
    (
        '8',
        [
            ".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###.",
        ],
    ),
    (
        '9',
        [
            ".###.", "#...#", "#...#", ".####", "....#", "....#", ".###.",
        ],
    ),
    (
        'A',
        [
            ".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
        ],
    ),
    (
        'B',
        [
            "####.", "#...#", "#...#", "####.", "#...#", "#...#", "####.",
        ],
    ),
    (
        'C',
        [
            ".###.", "#...#", "#....", "#....", "#....", "#...#", ".###.",
        ],
    ),
    (
        'E',
        [
            "#####", "#....", "#....", "####.", "#....", "#....", "#####",
        ],
    ),
    (
        'F',
        [
            "#####", "#....", "#....", "####.", "#....", "#....", "#....",
        ],
    ),
    (
        'G',
        [
            ".###.", "#...#", "#....", "#.###", "#...#", "#...#", ".###.",
        ],
    ),
    (
        'H',
        [
            "#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
        ],
    ),
    (
        'I',
        [
            ".###.", "..#..", "..#..", "..#..", "..#..", "..#..", ".###.",
        ],
    ),
    (
        'L',
        [
            "#....", "#....", "#....", "#....", "#....", "#....", "#####",
        ],
    ),
    (
        'M',
        [
            "#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#",
        ],
    ),
    (
        'N',
        [
            "#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#", "#...#",
        ],
    ),
    (
        'O',
        [
            ".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
        ],
    ),
    (
        'P',
        [
            "####.", "#...#", "#...#", "####.", "#....", "#....", "#....",
        ],
    ),
    (
        'R',
        [
            "####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#",
        ],
    ),
    (
        'S',
        [
            ".####", "#....", "#....", ".###.", "....#", "....#", "####.",
        ],
    ),
    (
        'T',
        [
            "#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#..",
        ],
    ),
    (
        'U',
        [
            "#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
        ],
    ),
    (
        'V',
        [
            "#...#", "#...#", "#...#", "#...#", "#...#", ".#.#.", "..#..",
        ],
    ),
    (
        'W',
        [
            "#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#",
        ],
    ),
    (
        'X',
        [
            "#...#", "#...#", ".#.#.", "..#..", ".#.#.", "#...#", "#...#",
        ],
    ),
    (
        '.',
        [
            ".....", ".....", ".....", ".....", ".....", ".##..", ".##..",
        ],
    ),
    (
        ':',
        [
            ".....", ".##..", ".##..", ".....", ".##..", ".##..", ".....",
        ],
    ),
    (
        '%',
        [
            "##...", "##..#", "...#.", "..#..", ".#...", "#..##", "...##",
        ],
    ),
    (
        '-',
        [
            ".....", ".....", ".....", "#####", ".....", ".....", ".....",
        ],
    ),
    (
        '/',
        [
            "....#", "....#", "...#.", "..#..", ".#...", "#....", "#....",
        ],
    ),
];

const GLYPH_W: usize = 5;
const GLYPH_H: usize = 7;

fn glyph(c: char) -> Option<&'static [&'static str; 7]> {
    let c = c.to_ascii_uppercase();
    GLYPHS.iter().find(|(g, _)| *g == c).map(|(_, rows)| rows)
}

/// Целевой буфер: 4 байта на пиксель.
#[derive(Debug)]
pub struct Canvas<'a> {
    /// Пиксели.
    pub data: &'a mut [u8],
    /// Ширина.
    pub width: usize,
    /// Высота.
    pub height: usize,
    /// Байт на строку.
    pub stride: usize,
}

impl Canvas<'_> {
    fn px(&mut self, x: usize, y: usize) -> Option<&mut [u8]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let o = y * self.stride + x * 4;
        self.data.get_mut(o..o + 4)
    }

    fn put(&mut self, x: usize, y: usize, rgb: [u8; 3]) {
        if let Some(p) = self.px(x, y) {
            // Каналы 0 и 2 одинаковы во всех наших цветах, поэтому BGRA и RGBA совпадают.
            p[0] = rgb[2];
            p[1] = rgb[1];
            p[2] = rgb[0];
            p[3] = 0xFF;
        }
    }

    /// Затемняет прямоугольник (фон под текстом), сохраняя читаемость игры под ним.
    pub fn darken(&mut self, x: usize, y: usize, w: usize, h: usize) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                if let Some(p) = self.px(xx, yy) {
                    for c in &mut p[..3] {
                        *c /= 3;
                    }
                }
            }
        }
    }

    /// Текст шрифтом 5×7 с масштабом `scale`. Неизвестные символы — пробел.
    pub fn text(&mut self, x: usize, y: usize, scale: usize, s: &str, rgb: [u8; 3]) {
        let scale = scale.max(1);
        for (i, c) in s.chars().enumerate() {
            let Some(rows) = glyph(c) else { continue };
            let gx = x + i * (GLYPH_W + 1) * scale;
            for (ry, row) in rows.iter().enumerate() {
                for (rx, b) in row.bytes().enumerate() {
                    if b != b'#' {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            self.put(gx + rx * scale + dx, y + ry * scale + dy, rgb);
                        }
                    }
                }
            }
        }
    }
}

/// Ширина строки текста в пикселях.
#[must_use]
pub fn text_width(s: &str, scale: usize) -> usize {
    s.chars().count() * (GLYPH_W + 1) * scale.max(1)
}

/// Данные для оверлея.
#[derive(Debug, Clone, Default)]
pub struct OverlayData {
    /// Строки текста (`A`–`X` без части букв, цифры, `.:%-/`; см. `GLYPHS`).
    pub lines: Vec<String>,
    /// Последние времена кадра, мс (для графика).
    pub frame_times_ms: Vec<f32>,
    /// Целевое время кадра, мс (линия на графике).
    pub target_ms: f32,
}

const WHITE: [u8; 3] = [0xF0, 0xF0, 0xF0];
const GREEN: [u8; 3] = [0x40, 0xE0, 0x40];
const GREY: [u8; 3] = [0x80, 0x80, 0x80];

/// Рисует оверлей в левом верхнем углу.
pub fn draw(canvas: &mut Canvas<'_>, data: &OverlayData, scale: usize) {
    let scale = scale.max(1);
    let pad = 4 * scale;
    let line_h = (GLYPH_H + 3) * scale;
    let text_w = data
        .lines
        .iter()
        .map(|l| text_width(l, scale))
        .max()
        .unwrap_or(0);
    let graph_w = data.frame_times_ms.len().min(240);
    let graph_h = 32 * scale;
    let w = text_w.max(graph_w) + 2 * pad;
    let h = data.lines.len() * line_h + if graph_w > 0 { graph_h + pad } else { 0 } + 2 * pad;
    canvas.darken(0, 0, w, h);
    for (i, line) in data.lines.iter().enumerate() {
        canvas.text(pad, pad + i * line_h, scale, line, WHITE);
    }
    if graph_w == 0 {
        return;
    }
    // График: столбик на кадр, шкала 0..2×target.
    let top = pad + data.lines.len() * line_h;
    let max_ms = (data.target_ms * 2.0).max(1.0);
    let samples = &data.frame_times_ms[data.frame_times_ms.len() - graph_w..];
    for (i, &ms) in samples.iter().enumerate() {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "пиксели графика"
        )]
        let bar = ((ms / max_ms).clamp(0.0, 1.0) * graph_h as f32) as usize;
        let color = if ms > data.target_ms * 1.5 {
            WHITE
        } else {
            GREEN
        };
        for y in 0..bar {
            canvas.put(pad + i, top + graph_h - 1 - y, color);
        }
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "пиксели графика"
    )]
    let target_y = top + graph_h - 1 - ((data.target_ms / max_ms) * graph_h as f32) as usize;
    for i in 0..graph_w {
        if i % 4 < 2 {
            canvas.put(pad + i, target_y, GREY);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_glyphs_are_5x7() {
        for (c, rows) in GLYPHS {
            for r in rows {
                assert_eq!(r.len(), 5, "{c}");
                assert!(r.bytes().all(|b| b == b'#' || b == b'.'), "{c}");
            }
        }
    }

    #[test]
    fn draws_text_and_graph_within_bounds() {
        let (w, h) = (300, 200);
        let mut buf = vec![0x90_u8; w * h * 4];
        let mut c = Canvas {
            data: &mut buf,
            width: w,
            height: h,
            stride: w * 4,
        };
        let data = OverlayData {
            lines: vec![
                "FPS 144.0".into(),
                "1% LOW 131.2".into(),
                "FT 6.9 MS".into(),
            ],
            frame_times_ms: (0..400).map(|i| 6.9 + (i % 7) as f32).collect(),
            target_ms: 6.94,
        };
        draw(&mut c, &data, 1);
        // Фон затемнён в углу, а дальний угол кадра не тронут.
        assert!(buf[0] < 0x90);
        let far = (h - 1) * w * 4 + (w - 1) * 4;
        assert_eq!(buf[far], 0x90);
        // Есть белые пиксели текста.
        assert!(buf.chunks(4).any(|p| p[0] == 0xF0 && p[1] == 0xF0));
    }

    #[test]
    fn tiny_canvas_does_not_panic() {
        let mut buf = vec![0_u8; 4 * 4 * 4];
        let mut c = Canvas {
            data: &mut buf,
            width: 4,
            height: 4,
            stride: 16,
        };
        let data = OverlayData {
            lines: vec!["FPS 60".into()],
            frame_times_ms: vec![16.0; 10],
            target_ms: 16.6,
        };
        draw(&mut c, &data, 3);
    }

    #[test]
    fn text_width_counts_spacing() {
        assert_eq!(text_width("AB", 2), 24);
    }
}
