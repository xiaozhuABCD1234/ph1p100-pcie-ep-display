//! YOLO11n 的 letterbox 与单输出 [1,84,8400] 解码。
//!
//! 这个模型来自 Ultralytics 导出：84 = 4 个 xywh + 80 个类别概率，
//! 框已经是 640x640 输入像素坐标，不需要 DFL。NMS 按类别做（class-aware），
//! 与 Ultralytics 默认行为一致。

pub const NUM_CLASSES: usize = 80;
pub const CHANNELS: usize = 4 + NUM_CLASSES;
pub const MAX_DETECTIONS: usize = 100;
pub const PAD_VALUE: u8 = 114;

#[derive(Clone, Debug)]
pub struct Detection {
    /// 归一化到源图（0..1）的左上/右下角
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub class_id: usize,
    pub score: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct Letterbox {
    pub src_w: u32,
    pub src_h: u32,
    pub scale: f32,
    pub pad_x: f32,
    pub pad_y: f32,
    pub resized_w: u32,
    pub resized_h: u32,
}

impl Letterbox {
    pub fn new(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Self {
        let scale = (dst_w as f32 / src_w.max(1) as f32).min(dst_h as f32 / src_h.max(1) as f32);
        let resized_w = ((src_w as f32 * scale).round() as u32).clamp(1, dst_w);
        let resized_h = ((src_h as f32 * scale).round() as u32).clamp(1, dst_h);
        let pad_x = (dst_w - resized_w) as f32 / 2.0;
        let pad_y = (dst_h - resized_h) as f32 / 2.0;
        Letterbox {
            src_w,
            src_h,
            scale,
            pad_x,
            pad_y,
            resized_w,
            resized_h,
        }
    }

    /// 模型输入坐标 -> 源图归一化坐标
    pub fn model_to_norm(&self, x: f32, y: f32) -> (f32, f32) {
        (
            (x - self.pad_x) / self.scale / self.src_w as f32,
            (y - self.pad_y) / self.scale / self.src_h as f32,
        )
    }
}

/// RGB8（HWC）-> letterbox 后的 uint8 NHWC RGB。灰色边用 114。
pub fn letterbox_rgb_nhwc(
    rgb: &[u8],
    w: u32,
    h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Result<(Vec<u8>, Letterbox), String> {
    if w == 0 || h == 0 || dst_w == 0 || dst_h == 0 {
        return Err("letterbox 几何为 0".into());
    }
    let need = w as usize * h as usize * 3;
    if rgb.len() < need {
        return Err(format!("RGB 长度 {} 不够 {}x{}x3", rgb.len(), w, h));
    }
    let lb = Letterbox::new(w, h, dst_w, dst_h);
    let plane = dst_w as usize * dst_h as usize;
    let mut out = vec![PAD_VALUE; plane * 3];
    let (src_w, src_h) = (w as usize, h as usize);

    for dy in 0..lb.resized_h as usize {
        let sy = ((dy as f32 + 0.5) / lb.scale - 0.5).max(0.0);
        let y0 = (sy.floor() as usize).min(src_h - 1);
        let y1 = (y0 + 1).min(src_h - 1);
        let fy = sy - y0 as f32;
        let oy = dy + lb.pad_y as usize;
        for dx in 0..lb.resized_w as usize {
            let sx = ((dx as f32 + 0.5) / lb.scale - 0.5).max(0.0);
            let x0 = (sx.floor() as usize).min(src_w - 1);
            let x1 = (x0 + 1).min(src_w - 1);
            let fx = sx - x0 as f32;
            let ox = dx + lb.pad_x as usize;
            let i00 = (y0 * src_w + x0) * 3;
            let i10 = (y0 * src_w + x1) * 3;
            let i01 = (y1 * src_w + x0) * 3;
            let i11 = (y1 * src_w + x1) * 3;
            let dst = (oy * dst_w as usize + ox) * 3;
            for c in 0..3 {
                let top = rgb[i00 + c] as f32 * (1.0 - fx) + rgb[i10 + c] as f32 * fx;
                let bot = rgb[i01 + c] as f32 * (1.0 - fx) + rgb[i11 + c] as f32 * fx;
                out[dst + c] = (top * (1.0 - fy) + bot * fy).round() as u8;
            }
        }
    }
    Ok((out, lb))
}

fn iou(a: &Detection, b: &Detection) -> f32 {
    let x1 = a.x1.max(b.x1);
    let y1 = a.y1.max(b.y1);
    let x2 = a.x2.min(b.x2);
    let y2 = a.y2.min(b.y2);
    let iw = (x2 - x1).max(0.0);
    let ih = (y2 - y1).max(0.0);
    let inter = iw * ih;
    let ua = (a.x2 - a.x1).max(0.0) * (a.y2 - a.y1).max(0.0)
        + (b.x2 - b.x1).max(0.0) * (b.y2 - b.y1).max(0.0)
        - inter;
    if ua <= 0.0 {
        0.0
    } else {
        inter / ua
    }
}

/// 解码单输出。dims 是 rknn 查询到的 output dims（可带 batch 1）。
/// 支持 [1,84,N]（channels-first）与 [1,N,84]（channels-last）。
pub fn decode_yolo11(
    out: &[f32],
    dims: &[u32],
    lb: &Letterbox,
    conf: f32,
    nms: f32,
) -> Result<Vec<Detection>, String> {
    let shape: Vec<u32> = dims.iter().copied().filter(|&d| d != 1).collect();
    let (ch_first, n) = match shape.as_slice() {
        [c, n] if *c as usize == CHANNELS => (true, *n as usize),
        [n, c] if *c as usize == CHANNELS => (false, *n as usize),
        _ => {
            return Err(format!(
                "输出 shape 不接受：dims={dims:?}，只支持 [1,{CHANNELS},N] 或 [1,N,{CHANNELS}]"
            ))
        }
    };
    let need = CHANNELS * n;
    if out.len() < need {
        return Err(format!("输出长度 {}，期望 {need}", out.len()));
    }
    let at = |c: usize, i: usize| -> f32 {
        if ch_first {
            out[c * n + i]
        } else {
            out[i * CHANNELS + c]
        }
    };

    let mut raw: Vec<Detection> = Vec::new();
    for i in 0..n {
        let mut best = 0usize;
        let mut best_score = at(4, i);
        for c in 1..NUM_CLASSES {
            let s = at(4 + c, i);
            if s > best_score {
                best_score = s;
                best = c;
            }
        }
        if !best_score.is_finite() || best_score < conf {
            continue;
        }
        let cx = at(0, i);
        let cy = at(1, i);
        let bw = at(2, i);
        let bh = at(3, i);
        if ![cx, cy, bw, bh].iter().all(|v| v.is_finite()) || bw <= 0.0 || bh <= 0.0 {
            continue;
        }
        let (x1, y1) = lb.model_to_norm(cx - bw * 0.5, cy - bh * 0.5);
        let (x2, y2) = lb.model_to_norm(cx + bw * 0.5, cy + bh * 0.5);
        let x1 = x1.clamp(0.0, 1.0);
        let y1 = y1.clamp(0.0, 1.0);
        let x2 = x2.clamp(0.0, 1.0);
        let y2 = y2.clamp(0.0, 1.0);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        raw.push(Detection {
            x1,
            y1,
            x2,
            y2,
            class_id: best,
            score: best_score,
        });
    }

    // 按分数降序，class-aware NMS
    raw.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut keep: Vec<Detection> = Vec::new();
    for d in raw {
        if keep.len() >= MAX_DETECTIONS {
            break;
        }
        let suppressed = keep
            .iter()
            .any(|k| k.class_id == d.class_id && iou(k, &d) > nms);
        if !suppressed {
            keep.push(d);
        }
    }
    Ok(keep)
}

fn split_shape(dims: &[u32], channels: usize) -> Result<(bool, usize), String> {
    let shape: Vec<u32> = dims.iter().copied().filter(|&d| d != 1).collect();
    match shape.as_slice() {
        [c, n] if *c as usize == channels => Ok((true, *n as usize)),
        [n, c] if *c as usize == channels => Ok((false, *n as usize)),
        _ => Err(format!(
            "输出 shape 不接受：dims={dims:?}，期望 [{channels},N] 或 [N,{channels}]"
        )),
    }
}

/// 解码 split 模型：boxes [1,4,N] 与 scores [1,80,N] 是两个独立输出。
/// 分开量化后类别分数不会被框坐标的尺度压成 0。
pub fn decode_yolo11_split(
    boxes: &[f32],
    scores: &[f32],
    box_dims: &[u32],
    score_dims: &[u32],
    lb: &Letterbox,
    conf: f32,
    nms: f32,
) -> Result<Vec<Detection>, String> {
    let (b_first, n) = split_shape(box_dims, 4)?;
    let (s_first, sn) = split_shape(score_dims, NUM_CLASSES)?;
    if sn != n {
        return Err(format!("boxes N={n} 与 scores N={sn} 不一致"));
    }
    if boxes.len() < 4 * n || scores.len() < NUM_CLASSES * n {
        return Err(format!(
            "输出长度不足（boxes {}，scores {}，N={n}）",
            boxes.len(),
            scores.len()
        ));
    }
    let bat = |c: usize, i: usize| -> f32 {
        if b_first {
            boxes[c * n + i]
        } else {
            boxes[i * 4 + c]
        }
    };
    let sat = |c: usize, i: usize| -> f32 {
        if s_first {
            scores[c * n + i]
        } else {
            scores[i * NUM_CLASSES + c]
        }
    };

    let mut raw: Vec<Detection> = Vec::new();
    for i in 0..n {
        let mut best = 0usize;
        let mut best_score = sat(0, i);
        for c in 1..NUM_CLASSES {
            let s = sat(c, i);
            if s > best_score {
                best_score = s;
                best = c;
            }
        }
        if !best_score.is_finite() || best_score < conf {
            continue;
        }
        let cx = bat(0, i);
        let cy = bat(1, i);
        let bw = bat(2, i);
        let bh = bat(3, i);
        if ![cx, cy, bw, bh].iter().all(|v| v.is_finite()) || bw <= 0.0 || bh <= 0.0 {
            continue;
        }
        let (x1, y1) = lb.model_to_norm(cx - bw * 0.5, cy - bh * 0.5);
        let (x2, y2) = lb.model_to_norm(cx + bw * 0.5, cy + bh * 0.5);
        let x1 = x1.clamp(0.0, 1.0);
        let y1 = y1.clamp(0.0, 1.0);
        let x2 = x2.clamp(0.0, 1.0);
        let y2 = y2.clamp(0.0, 1.0);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        raw.push(Detection {
            x1,
            y1,
            x2,
            y2,
            class_id: best,
            score: best_score,
        });
    }

    raw.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut keep: Vec<Detection> = Vec::new();
    for d in raw {
        if keep.len() >= MAX_DETECTIONS {
            break;
        }
        let suppressed = keep
            .iter()
            .any(|k| k.class_id == d.class_id && iou(k, &d) > nms);
        if !suppressed {
            keep.push(d);
        }
    }
    Ok(keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_1920x1080_has_expected_geometry() {
        let lb = Letterbox::new(1920, 1080, 640, 640);
        assert_eq!(lb.resized_w, 640);
        assert_eq!(lb.resized_h, 360);
        assert!((lb.pad_y - 140.0).abs() < 0.01);
        assert!((lb.pad_x - 0.0).abs() < 0.01);
        let (nx, ny) = lb.model_to_norm(320.0, 320.0);
        assert!((nx - 0.5).abs() < 0.001, "{nx}");
        assert!((ny - 0.5).abs() < 0.001, "{ny}");
        let (_, top) = lb.model_to_norm(320.0, 0.0);
        assert!(top < 0.0);
    }

    #[test]
    fn letterbox_preserves_color_inside_roi_and_pads_gray() {
        let w = 4u32;
        let h = 2u32;
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = (y * w as usize + x) * 3;
                rgb[i] = 10;
                rgb[i + 1] = 20;
                rgb[i + 2] = 30;
            }
        }
        let (nhwc, lb) = letterbox_rgb_nhwc(&rgb, w, h, 8, 8).unwrap();
        assert_eq!(lb.resized_w, 8);
        assert_eq!(lb.resized_h, 4);
        assert_eq!(lb.pad_y, 2.0);
        assert_eq!(nhwc[0], PAD_VALUE);
        assert_eq!(nhwc[nhwc.len() - 1], PAD_VALUE);
        let idx = (3 * 8 + 4) * 3;
        assert_eq!(nhwc[idx], 10);
        assert_eq!(nhwc[idx + 1], 20);
        assert_eq!(nhwc[idx + 2], 30);
    }

    fn make_tensor(ch_first: bool, n: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; CHANNELS * n];
        let put = |v: &mut Vec<f32>, c: usize, i: usize, x: f32| {
            if ch_first {
                v[c * n + i] = x;
            } else {
                v[i * CHANNELS + c] = x;
            }
        };
        for (i, (cx, cy, bw, bh, cls, score)) in [
            (100.0, 100.0, 50.0, 50.0, 0usize, 0.9f32),
            (105.0, 100.0, 50.0, 50.0, 0, 0.8),
            (400.0, 400.0, 80.0, 60.0, 2, 0.7),
        ]
        .iter()
        .enumerate()
        {
            put(&mut v, 0, i, *cx);
            put(&mut v, 1, i, *cy);
            put(&mut v, 2, i, *bw);
            put(&mut v, 3, i, *bh);
            put(&mut v, 4 + cls, i, *score);
        }
        v
    }

    #[test]
    fn decode_channels_first_and_nms() {
        let lb = Letterbox::new(640, 640, 640, 640);
        let t = make_tensor(true, 10);
        let d = decode_yolo11(&t, &[1, 84, 10], &lb, 0.25, 0.45).unwrap();
        assert_eq!(d.len(), 2, "{d:?}");
        assert_eq!(d[0].class_id, 0);
        assert!((d[0].score - 0.9).abs() < 1e-6);
        assert_eq!(d[1].class_id, 2);
        assert!((d[1].x1 - 0.5625).abs() < 0.001, "{:?}", d[1]);
    }

    #[test]
    fn decode_channels_last_matches() {
        let lb = Letterbox::new(640, 640, 640, 640);
        let t = make_tensor(false, 10);
        let d = decode_yolo11(&t, &[1, 10, 84], &lb, 0.25, 0.45).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].class_id, 0);
    }

    #[test]
    fn decode_rejects_wrong_shape() {
        let lb = Letterbox::new(640, 640, 640, 640);
        let e = decode_yolo11(&vec![0.0; 100], &[1, 10, 10], &lb, 0.25, 0.45).unwrap_err();
        assert!(e.contains("shape"), "{e}");
    }

    #[test]
    fn decode_split_outputs() {
        let lb = Letterbox::new(640, 640, 640, 640);
        let n = 10usize;
        let mut boxes = vec![0.0f32; 4 * n];
        let mut scores = vec![0.0f32; NUM_CLASSES * n];
        for (i, (cx, cy, bw, bh, cls, score)) in [
            (100.0, 100.0, 50.0, 50.0, 0usize, 0.9f32),
            (105.0, 100.0, 50.0, 50.0, 0, 0.8),
            (400.0, 400.0, 80.0, 60.0, 2, 0.7),
        ]
        .iter()
        .enumerate()
        {
            boxes[i] = *cx;
            boxes[n + i] = *cy;
            boxes[2 * n + i] = *bw;
            boxes[3 * n + i] = *bh;
            scores[cls * n + i] = *score;
        }
        let d = decode_yolo11_split(
            &boxes,
            &scores,
            &[1, 4, n as u32],
            &[1, NUM_CLASSES as u32, n as u32],
            &lb,
            0.25,
            0.45,
        )
        .unwrap();
        assert_eq!(d.len(), 2, "{d:?}");
        assert_eq!(d[0].class_id, 0);
        assert!((d[0].score - 0.9).abs() < 1e-6);
        assert_eq!(d[1].class_id, 2);
    }
}
