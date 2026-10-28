//! 在 RGB 图上画检测框和标签。
//!
//! 不依赖字体库：内置一套 5x7 点阵，只覆盖大写字母、数字和少量符号。
//! COCO 类别名先转大写再画；未知字符画成问号。

use super::labels;
use super::yolo::Detection;

/// 一组区分度较高的 BGR 风格颜色（实际按 RGB 写入）。
const COLORS: [[u8; 3]; 12] = [
    [255, 56, 56],
    [255, 157, 151],
    [255, 112, 31],
    [255, 178, 29],
    [207, 210, 49],
    [72, 249, 10],
    [146, 204, 23],
    [61, 219, 134],
    [26, 147, 52],
    [0, 212, 187],
    [44, 153, 168],
    [0, 194, 255],
];

pub fn color_for(class_id: usize) -> [u8; 3] {
    COLORS[class_id % COLORS.len()]
}

fn set_px(rgb: &mut [u8], w: usize, h: usize, x: i32, y: i32, c: [u8; 3]) {
    if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
        return;
    }
    let i = (y as usize * w + x as usize) * 3;
    if i + 2 < rgb.len() {
        rgb[i] = c[0];
        rgb[i + 1] = c[1];
        rgb[i + 2] = c[2];
    }
}

fn fill_rect(rgb: &mut [u8], w: usize, h: usize, x: i32, y: i32, rw: i32, rh: i32, c: [u8; 3]) {
    if rw <= 0 || rh <= 0 {
        return;
    }
    for yy in y..y + rh {
        for xx in x..x + rw {
            set_px(rgb, w, h, xx, yy, c);
        }
    }
}

fn stroke_rect(
    rgb: &mut [u8],
    w: usize,
    h: usize,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    c: [u8; 3],
    thick: i32,
) {
    let t = thick.max(1);
    fill_rect(rgb, w, h, x1, y1, x2 - x1 + 1, t, c);
    fill_rect(rgb, w, h, x1, y2 - t + 1, x2 - x1 + 1, t, c);
    fill_rect(rgb, w, h, x1, y1, t, y2 - y1 + 1, c);
    fill_rect(rgb, w, h, x2 - t + 1, y1, t, y2 - y1 + 1, c);
}

/// 5x7 点阵：每个字符 5 列，每列的低 7 位是行。
fn glyph(ch: char) -> &'static [u8; 5] {
    match ch {
        ' ' => &[0x00, 0x00, 0x00, 0x00, 0x00],
        'A' => &[0x7E, 0x11, 0x11, 0x11, 0x7E],
        'B' => &[0x7F, 0x49, 0x49, 0x49, 0x36],
        'C' => &[0x3E, 0x41, 0x41, 0x41, 0x22],
        'D' => &[0x7F, 0x41, 0x41, 0x22, 0x1C],
        'E' => &[0x7F, 0x49, 0x49, 0x49, 0x41],
        'F' => &[0x7F, 0x09, 0x09, 0x09, 0x01],
        'G' => &[0x3E, 0x41, 0x49, 0x49, 0x7A],
        'H' => &[0x7F, 0x08, 0x08, 0x08, 0x7F],
        'I' => &[0x00, 0x41, 0x7F, 0x41, 0x00],
        'J' => &[0x20, 0x40, 0x41, 0x3F, 0x01],
        'K' => &[0x7F, 0x08, 0x14, 0x22, 0x41],
        'L' => &[0x7F, 0x40, 0x40, 0x40, 0x40],
        'M' => &[0x7F, 0x02, 0x0C, 0x02, 0x7F],
        'N' => &[0x7F, 0x04, 0x08, 0x10, 0x7F],
        'O' => &[0x3E, 0x41, 0x41, 0x41, 0x3E],
        'P' => &[0x7F, 0x09, 0x09, 0x09, 0x06],
        'Q' => &[0x3E, 0x41, 0x51, 0x21, 0x5E],
        'R' => &[0x7F, 0x09, 0x19, 0x29, 0x46],
        'S' => &[0x46, 0x49, 0x49, 0x49, 0x31],
        'T' => &[0x01, 0x01, 0x7F, 0x01, 0x01],
        'U' => &[0x3F, 0x40, 0x40, 0x40, 0x3F],
        'V' => &[0x1F, 0x20, 0x40, 0x20, 0x1F],
        'W' => &[0x3F, 0x40, 0x38, 0x40, 0x3F],
        'X' => &[0x63, 0x14, 0x08, 0x14, 0x63],
        'Y' => &[0x07, 0x08, 0x70, 0x08, 0x07],
        'Z' => &[0x61, 0x51, 0x49, 0x45, 0x43],
        '0' => &[0x3E, 0x51, 0x49, 0x45, 0x3E],
        '1' => &[0x00, 0x42, 0x7F, 0x40, 0x00],
        '2' => &[0x42, 0x61, 0x51, 0x49, 0x46],
        '3' => &[0x21, 0x41, 0x45, 0x4B, 0x31],
        '4' => &[0x18, 0x14, 0x12, 0x7F, 0x10],
        '5' => &[0x27, 0x45, 0x45, 0x45, 0x39],
        '6' => &[0x3C, 0x4A, 0x49, 0x49, 0x30],
        '7' => &[0x01, 0x71, 0x09, 0x05, 0x03],
        '8' => &[0x36, 0x49, 0x49, 0x49, 0x36],
        '9' => &[0x06, 0x49, 0x49, 0x29, 0x1E],
        '.' => &[0x00, 0x00, 0x60, 0x00, 0x00],
        ':' => &[0x00, 0x36, 0x36, 0x00, 0x00],
        '%' => &[0x23, 0x13, 0x08, 0x64, 0x62],
        '-' => &[0x08, 0x08, 0x08, 0x08, 0x08],
        '/' => &[0x20, 0x10, 0x08, 0x04, 0x02],
        _ => &[0x02, 0x01, 0x51, 0x09, 0x06],
    }
}

fn text_width(s: &str, scale: i32) -> i32 {
    s.chars().count() as i32 * 6 * scale
}

fn draw_text(
    rgb: &mut [u8],
    w: usize,
    h: usize,
    s: &str,
    x: i32,
    y: i32,
    scale: i32,
    color: [u8; 3],
) {
    let mut cx = x;
    for ch in s.chars() {
        let upper = ch.to_ascii_uppercase();
        let g = glyph(upper);
        for (col, bits) in g.iter().enumerate() {
            for row in 0..7 {
                if bits & (1 << row) != 0 {
                    fill_rect(
                        rgb,
                        w,
                        h,
                        cx + col as i32 * scale,
                        y + row * scale,
                        scale,
                        scale,
                        color,
                    );
                }
            }
        }
        cx += 6 * scale;
    }
}

/// 把检测结果画到 RGB 上。框按归一化坐标映射到当前图尺寸。
pub fn draw_detections(rgb: &mut [u8], w: usize, h: usize, dets: &[Detection]) {
    if w == 0 || h == 0 || rgb.len() < w * h * 3 {
        return;
    }
    for d in dets {
        let x1 = (d.x1 * w as f32).round() as i32;
        let y1 = (d.y1 * h as f32).round() as i32;
        let x2 = (d.x2 * w as f32).round() as i32 - 1;
        let y2 = (d.y2 * h as f32).round() as i32 - 1;
        let x1 = x1.clamp(0, w as i32 - 1);
        let y1 = y1.clamp(0, h as i32 - 1);
        let x2 = x2.clamp(0, w as i32 - 1);
        let y2 = y2.clamp(0, h as i32 - 1);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        let c = color_for(d.class_id);
        stroke_rect(rgb, w, h, x1, y1, x2, y2, c, 2);

        let label = format!(
            "{} {:.2}",
            labels::name(d.class_id).to_ascii_uppercase(),
            d.score
        );
        let scale = if w < 640 { 1 } else { 2 };
        let tw = text_width(&label, scale);
        let th = 9 * scale;
        let mut lx = x1;
        let mut ly = y1 - th - 2;
        if ly < 0 {
            ly = y1 + 2;
        }
        if lx + tw >= w as i32 {
            lx = (w as i32 - tw - 1).max(0);
        }
        if ly + th >= h as i32 {
            ly = (h as i32 - th - 1).max(0);
        }
        fill_rect(rgb, w, h, lx, ly, tw + 2, th, c);
        draw_text(rgb, w, h, &label, lx + 1, ly + 1, scale, [255, 255, 255]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(x1: f32, y1: f32, x2: f32, y2: f32) -> Detection {
        Detection {
            x1,
            y1,
            x2,
            y2,
            class_id: 0,
            score: 0.87,
        }
    }

    #[test]
    fn draws_inside_and_clips_outside() {
        let (w, h) = (100usize, 80usize);
        let mut rgb = vec![0u8; w * h * 3];
        draw_detections(&mut rgb, w, h, &[det(0.1, 0.1, 0.9, 0.9)]);
        assert!(rgb.iter().any(|&v| v != 0));

        let mut rgb2 = vec![0u8; w * h * 3];
        draw_detections(&mut rgb2, w, h, &[det(-0.5, -0.2, 1.5, 1.3)]);
        assert!(rgb2.iter().any(|&v| v != 0));
    }

    #[test]
    fn empty_and_tiny_boxes_do_not_panic() {
        let (w, h) = (10usize, 10usize);
        let mut rgb = vec![0u8; w * h * 3];
        draw_detections(&mut rgb, w, h, &[]);
        draw_detections(&mut rgb, w, h, &[det(0.5, 0.5, 0.5, 0.5)]);
        draw_detections(&mut rgb, 0, 0, &[det(0.0, 0.0, 1.0, 1.0)]);
    }
}
