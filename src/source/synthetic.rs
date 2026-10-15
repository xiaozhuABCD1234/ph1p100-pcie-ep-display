//! 合成源：自己造图，没有硬件也能把「编码 → 推流」整条链路跑通。
//!
//! 每帧在上一帧基础上横向平移，所以画面是动的；另外固定画一个纯白块和一个
//! 纯黑块，保证任何一帧的灰度极值都是 0 与 255（便于验证与自检）。

use super::{Frame, FrameSource, SourceError};

pub struct Synthetic {
    width: u16,
    height: u16,
    /// 帧序号，用来驱动画面平移
    phase: u32,
}

/// 画布上一块矩形区域
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Synthetic {
    pub fn new(width: u16, height: u16) -> Self {
        Synthetic {
            width,
            height,
            phase: 0,
        }
    }

    /// 把画布上 `r` 这块填成 `v`（超出画布的部分自动裁掉）
    fn fill_rect(gray: &mut [u8], canvas_w: usize, canvas_h: usize, r: Rect, v: u8) {
        for y in r.y..(r.y + r.h).min(canvas_h) {
            for x in r.x..(r.x + r.w).min(canvas_w) {
                gray[y * canvas_w + x] = v;
            }
        }
    }
}

impl FrameSource for Synthetic {
    fn next_frame(&mut self) -> Result<Frame, SourceError> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut gray = vec![0u8; w * h];
        let phase = self.phase as usize;

        // 斜向渐变（左上行暗、右下行亮）叠一层横向移动的棋盘纹理
        let (wx, wy) = ((w.max(2) - 1) as u32, (h.max(2) - 1) as u32);
        for y in 0..h {
            for x in 0..w {
                let grad = (x as u32 * 255 / wx + y as u32 * 255 / wy) / 2; // 0..=255
                let checker = (((x + phase * 3) / 16) + y / 16) % 2 == 1;
                let v = if checker {
                    (grad / 2 + 128).min(255)
                } else {
                    grad / 2
                };
                gray[y * w + x] = v as u8;
            }
        }

        // 两个标记块：白块向右跑，黑块固定，用来肉眼确认画面在动、且极值确定
        let bw = (w / 8).max(2);
        let bh = (h / 8).max(2);
        let x_white = if w > bw { (phase * 4) % (w - bw) } else { 0 };
        Synthetic::fill_rect(
            &mut gray,
            w,
            h,
            Rect {
                x: x_white,
                y: h / 8,
                w: bw,
                h: bh,
            },
            255,
        );
        Synthetic::fill_rect(
            &mut gray,
            w,
            h,
            Rect {
                x: w / 8,
                y: h * 5 / 8,
                w: bw,
                h: bh,
            },
            0,
        );

        self.phase = self.phase.wrapping_add(1);
        Ok(Frame::new(self.width, self.height, gray))
    }

    fn describe(&self) -> String {
        format!(
            "合成源 {}x{}（斜向渐变 + 移动棋盘 + 移动白块/固定黑块）",
            self.width, self.height
        )
    }
}
