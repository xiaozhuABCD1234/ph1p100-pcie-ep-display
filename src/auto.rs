//! 主机侧图像后处理：黑电平扣除 → 白平衡 → 自动曝光 → gamma。
//!
//! 为什么需要这一层：FPGA 送过来的是**传感器 RAW**（Bayer 马赛克、10bit 只取高 8 位、
//! 传感器侧没有 AE/AWB/gamma 在跑），所以直接显示/推流就是「很暗 + 偏色 + 顶着一段
//! 黑电平台阶」。这一层在主机侧把它补成一张能看的图，**不改动 FPGA 通路**。
//!
//! 顺序与理由（增益都在线性域算，最后合成进查找表，逐像素只剩查表）：

//!   1) **黑电平扣除**：IMX415 的 10bit black pedestal = 50，截成 8 位就是 12。不扣掉的话
//!      gamma 会把这段台阶整体抬起来，暗部发灰、对比度也被吃掉。
//!   2) **白平衡（AWB）**：扣黑电平后的灰世界，以绿通道为参考。先排除接近饱和的
//!      像素（max >= 250），再取三通道均值；画面被绿色墙面/衣服主导时它会把绿色
//!      拉回中性，这是本板真实 CFA 下的物理结果（不是算法故障）。只有彩色通路生效。
//!   3) **自动曝光（AE）**：以「gamma 编码后的中位数」为基准定一个数字增益。用中位数
//!      而不是均值：对高光和大块暗背景都稳。而且 gamma 单调 ⇒
//!      `median(gamma(x)) == gamma(median(x))`，所以目标亮度是**精确命中**的，不用迭代。
//!      只提亮（gain ≥ 1），不压暗已经正常曝光的帧 —— 固定曝光传感器压暗只会白丢动态范围。
//!   4) **gamma**：默认 2.2（`--auto` 时），把线性 RAW 映射到显示域。
//!
//! 两个实现上的要点：
//!   * **抖动**：8 位输入配大增益必然丢灰度级（暗帧线性值只有 0..50 级），查表后会出现
//!     明显色带。所以查表带 4×4 有序抖动（ordered dither，均匀量化误差）。反过来说，
//!     若这张表恰好是**一一映射**（没丢级），就不抖 —— 免得白白加噪。
//!   * **时域平滑**：增益按帧做对数域指数滑动平均（EMA），抑制逐帧闪烁；第一帧直接采用
//!     本帧算出的值不等收敛。
//!
//! 这一层**不**负责「图为什么是裁过的角 / 顶上 36 行黑带」——那是 FPGA 侧裁剪窗的事
//! （见 fpga/rtl/ep_side/img/cam_fb_writer.v 的 SKIP_ROWS）。

/// 增益的时域平滑系数（对数域 EMA）。越小越稳、越大收敛越快。
const EMA: f32 = 0.35;
/// 白平衡增益夹紧范围：±2 个八度，防止某个通道过曝时把另两个拉爆
const WB_MIN: f32 = 0.25;
const WB_MAX: f32 = 4.0;

/// 数字增益夹紧范围
const GAIN_MIN: f32 = 0.25;
const GAIN_MAX: f32 = 64.0;
/// 统计采样目标：一帧最多看这么多像素（4K 下按步长抽），够稳且与分辨率无关
const SAMPLE_TARGET: usize = 1 << 15;
/// 4×4 有序抖动矩阵（经典 Bayer 矩阵），取值 0..15，用的时候除以 16 得到 [0,1) 的小数
const DITHER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// 后处理参数 + 时域状态。一帧调一次 [`AutoLevel::apply`]，跨帧持有以做平滑。
pub struct AutoLevel {
    black: u8,
    gamma: f32,
    /// AE 目标（gamma 编码后的中位数亮度），0 = 关
    ae_target: u8,
    awb: bool,
    // ---- 时域状态 ----
    gain: f32,
    wb: [f32; 3],
    primed: bool,
}

impl AutoLevel {
    pub fn new(black: u8, gamma: f32, ae_target: u8, awb: bool) -> Self {
        AutoLevel {
            black,
            gamma,
            ae_target,
            awb,
            gain: 1.0,
            wb: [1.0; 3],
            primed: false,
        }
    }

    /// 完全等价于「什么都不做」的参数组合。是的话整帧直接跳过，连统计都不做。
    pub fn is_identity(&self) -> bool {
        self.black == 0 && (self.gamma - 1.0).abs() < 1e-6 && self.ae_target == 0 && !self.awb
    }

    /// 一行式状态，给周期日志用
    pub fn status(&self) -> String {
        if self.is_identity() {
            return "后处理：关（原样透传）".to_string();
        }
        format!(
            "后处理：黑电平 -{}，gamma {:.2}，AE ×{:.2}，白平衡 R/G/B ×{:.2}/{:.2}/{:.2}",
            self.black, self.gamma, self.gain, self.wb[0], self.wb[1], self.wb[2]
        )
    }

    /// 就地处理一帧。`channels` = 1（灰度）或 3（RGB 交错）。灰度通路不做白平衡。
    ///
    /// `width × channels` 必须整除 `buf.len()`（等于帧高）。
    pub fn apply(&mut self, buf: &mut [u8], width: usize, channels: usize) {
        if self.is_identity() || width == 0 || !(1..=3).contains(&channels) {
            return;
        }
        let npx = buf.len() / channels;
        if npx < width {
            return;
        }

        // ---- 1. 采样（扣黑电平；排除接近饱和的像素，避免灯管高光带偏均值）----
        let step = (npx / SAMPLE_TARGET).max(1);
        let mut sum = [0u64; 3];
        let mut cnt = 0u64;
        let mut i = 0usize;
        while i < npx {
            let off = i * channels;
            let mut mx = 0u8;
            let mut v = [0u8; 3];
            for c in 0..channels {
                let x = buf[off + c].saturating_sub(self.black);
                v[c] = x;
                mx = mx.max(x);
            }
            if mx < 250 {
                for c in 0..channels {
                    sum[c] += v[c] as u64;
                }
                cnt += 1;
            }
            i += step;
        }
        if cnt == 0 {
            return;
        }

        // ---- 2. 白平衡（灰世界，以绿为参考）----
        if channels == 3 && self.awb {
            let m = [
                sum[0] as f32 / cnt as f32,
                sum[1] as f32 / cnt as f32,
                sum[2] as f32 / cnt as f32,
            ];
            let mut wb = [1.0f32; 3];
            if m[1] > 0.5 {
                for c in [0usize, 2] {
                    if m[c] > 0.5 {
                        wb[c] = (m[1] / m[c]).clamp(WB_MIN, WB_MAX);
                    }
                }
            }
            self.wb = if self.primed {
                smooth3(self.wb, wb)
            } else {
                wb
            };
        } else {
            self.wb = [1.0; 3];
        }

        // ---- 3. 自动曝光（把线性域的中位数定到目标）----
        if self.ae_target > 0 {
            let refv = self.reference_level(buf, npx, channels, step);
            if refv > 0.0 {
                // 想让 gamma 编码后的中位数 == ae_target，则线性域的中位数该落在
                // target_pre = 255 · (ae_target/255)^gamma
                let target_pre = 255.0 * (self.ae_target as f32 / 255.0).powf(self.gamma);
                // 定焦/固定曝光的传感器：AE 只负责把欠曝帧提亮，不把已经正常的帧压暗。
                // 压暗会白丢动态范围，还会让灯管等高光在 gamma 后偏色。
                let g = (target_pre / refv).clamp(GAIN_MIN, GAIN_MAX).max(1.0);
                self.gain = if self.primed { smooth(self.gain, g) } else { g };
            }
        } else {
            self.gain = 1.0;
        }
        self.primed = true;

        // ---- 4. 建表 + 应用 ----
        let lut = build_lut(channels, self.gain, &self.wb, self.gamma);
        let black = self.black as usize;
        for y in 0..(npx / width) {
            let drow = (y & 3) * 4;
            let rowoff = y * width * channels;
            for x in 0..width {
                let p = drow + (x & 3);
                let off = rowoff + x * channels;
                for c in 0..channels {
                    let v = buf[off + c] as usize;
                    buf[off + c] = lut[c * 16 + p][v.saturating_sub(black)];
                }
            }
        }
        // ---- 5. 高光去饱和 ----
        // 灯管等高光在传感器上已经接近/达到饱和，各通道 clipping 点不同会留下
        // 品红/绿色边。只把「接近白」的像素往亮度拉，不碰正常彩色物体。
        if channels == 3 {
            desaturate_highlights(buf, npx);
        }
    }

    /// AE 的基准亮度：采样点上「白平衡之后的三通道均值」的中位数（单通道就是它自己的中位数）。
    /// 返回中位数所在格的格值 + 0.5（格子是整数量化的，加半天偏）。
    fn reference_level(&self, buf: &[u8], npx: usize, channels: usize, step: usize) -> f32 {
        if channels == 1 {
            let mut h = [0u32; 256];
            let mut n = 0u32;
            let mut i = 0usize;
            while i < npx {
                h[buf[i].saturating_sub(self.black) as usize] += 1;
                n += 1;
                i += step;
            }
            return median_of(&h, n);
        }
        let mut h = [0u32; 256];
        let mut n = 0u32;
        let mut i = 0usize;
        let b = self.black as f32;
        while i < npx {
            let off = i * channels;
            let y = ((buf[off] as f32 - b).max(0.0) * self.wb[0]
                + (buf[off + 1] as f32 - b).max(0.0) * self.wb[1]
                + (buf[off + 2] as f32 - b).max(0.0) * self.wb[2])
                / 3.0;
            h[y.clamp(0.0, 255.0) as usize] += 1;
            n += 1;
            i += step;
        }
        median_of(&h, n)
    }
}

/// 高光去饱和：输出域亮度 > 220 的像素按比例向亮度混合，最多 85%。
/// 只处理 3 通道交错缓冲；npx 是像素数。
fn desaturate_highlights(buf: &mut [u8], npx: usize) {
    const START: f32 = 220.0;
    const FULL: f32 = 255.0;
    const MAX_MIX: f32 = 0.85;
    for i in 0..npx {
        let o = i * 3;
        let r = buf[o] as f32;
        let g = buf[o + 1] as f32;
        let b = buf[o + 2] as f32;
        let mx = r.max(g).max(b);
        if mx <= START {
            continue;
        }
        let t = ((mx - START) / (FULL - START)).clamp(0.0, 1.0) * MAX_MIX;
        let y = 0.299 * r + 0.587 * g + 0.114 * b;
        buf[o] = (r + (y - r) * t + 0.5) as u8;
        buf[o + 1] = (g + (y - g) * t + 0.5) as u8;
        buf[o + 2] = (b + (y - b) * t + 0.5) as u8;
    }
}

/// 直方图的中位数所在格值（+0.5 取格中心）；空直方图返回 0
fn median_of(h: &[u32; 256], n: u32) -> f32 {
    if n == 0 {
        return 0.0;
    }
    let half = n / 2;
    let mut acc = 0u32;
    for (v, &c) in h.iter().enumerate() {
        acc += c;
        if acc > half {
            return v as f32 + 0.5;
        }
    }
    255.0
}

/// 对数域 EMA：增益是乘性量，在对数域平滑才不会被大值主导
fn smooth(old: f32, new: f32) -> f32 {
    let (o, n) = (old.max(1e-6), new.max(1e-6));
    (o.ln() * (1.0 - EMA) + n.ln() * EMA).exp()
}

fn smooth3(old: [f32; 3], new: [f32; 3]) -> [f32; 3] {
    [
        smooth(old[0], new[0]),
        smooth(old[1], new[1]),
        smooth(old[2], new[2]),
    ]
}

/// 线性 8 位值 → gamma 编码后的**浮点**输出（未量化、未抖动）
fn gamma_encode(x: f32, gamma: f32) -> f32 {
    let x = x.clamp(0.0, 255.0);
    if (gamma - 1.0).abs() < 1e-6 {
        return x;
    }
    255.0 * (x / 255.0).powf(1.0 / gamma)
}

/// 建查找表：`channels` 组 × 16 个抖动相位，每组 256 项。
///
/// 先算不带抖动的表判断它是不是**一一映射**：是就不抖（没有丢级，抖了只是加噪）；
/// 不是（大增益/强 gamma 把多个输入挤到同一个输出）才按 4×4 有序抖动量化。
fn build_lut(channels: usize, gain: f32, wb: &[f32; 3], gamma: f32) -> Vec<[u8; 256]> {
    let mut lut = Vec::with_capacity(channels * 16);
    for &w in wb.iter().take(channels) {
        let k = gain * w;
        let mut base = [0u8; 256];
        for (v, slot) in base.iter_mut().enumerate() {
            *slot = (gamma_encode(v as f32 * k, gamma) + 0.5).min(255.0) as u8;
        }
        let mut seen = [false; 256];
        let injective = base.iter().all(|&o| {
            let first = !seen[o as usize];
            seen[o as usize] = true;
            first
        });
        if injective {
            for _ in 0..16 {
                lut.push(base);
            }
            continue;
        }
        for row in DITHER.iter() {
            for &dv in row.iter() {
                let d = dv as f32 / 16.0;
                let mut t = [0u8; 256];
                for (v, slot) in t.iter_mut().enumerate() {
                    *slot = (gamma_encode(v as f32 * k, gamma) + d).min(255.0) as u8;
                }
                lut.push(t);
            }
        }
    }
    lut
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张 `channels` 通道的图，`f(x, y, c)` 给出像素值
    fn make(w: usize, h: usize, channels: usize, f: impl Fn(usize, usize, usize) -> u8) -> Vec<u8> {
        let mut v = vec![0u8; w * h * channels];
        for y in 0..h {
            for x in 0..w {
                for c in 0..channels {
                    v[(y * w + x) * channels + c] = f(x, y, c);
                }
            }
        }
        v
    }

    fn mean(buf: &[u8]) -> f32 {
        buf.iter().map(|&v| v as f64).sum::<f64>() as f32 / buf.len() as f32
    }

    fn median(buf: &[u8]) -> u8 {
        let mut v = buf.to_vec();
        v.sort_unstable();
        v[v.len() / 2]
    }

    #[test]
    fn identity_params_leave_frame_untouched() {
        let mut fx = AutoLevel::new(0, 1.0, 0, false);
        assert!(fx.is_identity());
        let mut buf = make(8, 8, 3, |x, y, c| ((x * 7 + y * 11 + c * 23) % 256) as u8);
        let before = buf.clone();
        fx.apply(&mut buf, 8, 3);
        assert_eq!(
            buf, before,
            "恒等参数不该改任何一个字节（尤其不能加抖动噪声）"
        );
    }

    #[test]
    fn black_level_is_subtracted() {
        // 整帧正好压在 pedestal 上：扣掉后应当全 0，且不能出 NaN/负值
        let mut fx = AutoLevel::new(12, 2.2, 0, false);
        let mut buf = make(16, 16, 3, |_, _, _| 12);
        fx.apply(&mut buf, 16, 3);
        assert!(buf.iter().all(|&v| v == 0), "pedestal 扣完应当全 0");
        // 比 pedestal 低的（传感器噪声）也要夹到 0，不能回绕成 255
        let mut buf = make(16, 16, 1, |_, _, _| 11);
        fx.apply(&mut buf, 16, 1);
        assert!(buf.iter().all(|&v| v == 0), "低于黑电平的像素必须夹到 0");
    }

    #[test]
    fn gamma_lut_is_monotone_per_phase_and_hits_endpoints() {
        // 单调性要按**单个抖动相位**看：相邻像素故意用不同相位，整帧逐像素当然会
        // 有 ±1 的来回（那就是抖动的目的）。
        let lut = build_lut(1, 1.0, &[1.0; 3], 2.2);
        assert_eq!(lut.len(), 16);
        for (p, t) in lut.iter().enumerate() {
            assert_eq!(t[0], 0, "相位 {p}：0 应当映射到 0");
            assert_eq!(t[255], 255, "相位 {p}：255 应当映射到 255");
            for v in 0..255 {
                assert!(
                    t[v + 1] >= t[v],
                    "相位 {p} 必须单调不减：v={v} {} -> {}",
                    t[v],
                    t[v + 1]
                );
            }
        }
        // gamma 2.2 是提亮：中间调要真的被抬起来
        let base = &lut[0];
        assert!(base[64] > 64, "64 → {} 应当 > 64", base[64]);
        assert!(base[128] > 128, "128 → {} 应当 > 128", base[128]);
    }

    #[test]
    fn gamma_apply_keeps_endpoints_and_only_wiggles_by_one() {
        // 整帧走一遍：端点像素必须精确不变；相邻像素之间的来回不能超过 1（就是抖动）
        let mut fx = AutoLevel::new(0, 2.2, 0, false);
        let mut buf = make(256, 1, 1, |x, _, _| x as u8);
        let before: Vec<u8> = buf.clone();
        fx.apply(&mut buf, 256, 1);
        assert_eq!(buf[0], 0);
        assert_eq!(buf[255], 255);
        for i in 0..256 {
            let plain = (gamma_encode(before[i] as f32, 2.2) + 0.5).min(255.0) as i32;
            assert!(
                (buf[i] as i32 - plain).abs() <= 1,
                "像素 {i}：抖动只该差 ±1，得 {} 期 {plain}",
                buf[i]
            );
        }
    }

    #[test]
    fn auto_exposure_hits_target_median() {
        // 一张压在 12..52 的暗帧（就是板上实际那张的分布），AE 目标 128
        let mut fx = AutoLevel::new(12, 2.2, 128, false);
        let mut buf = make(64, 64, 1, |x, y, _| 12 + ((x + y) % 40) as u8);
        fx.apply(&mut buf, 64, 1);
        let m = median(&buf);
        assert!(
            (100..=155).contains(&m),
            "AE 后中位数应当落在 128 附近，实际 {m}"
        );
        assert!(fx.gain > 1.0, "暗帧应当被提亮，增益 {}", fx.gain);
    }

    #[test]
    fn auto_exposure_brightens_but_does_not_blow_out() {
        let mut fx = AutoLevel::new(12, 2.2, 100, false);
        let mut buf = make(64, 64, 3, |x, y, _| 12 + ((x * y) % 30) as u8);
        fx.apply(&mut buf, 64, 3);
        // 绝大多数像素不该被压到 255（目标只有 100，不是拉满）
        let blown = buf.iter().filter(|&&v| v == 255).count();
        assert!(
            blown * 100 < buf.len(),
            "过曝像素太多：{blown}/{}",
            buf.len()
        );
    }

    #[test]
    fn awb_neutralises_a_green_cast() {
        // 绿通道整体是红/蓝的两倍：灰世界应当把三通道拉到差不多
        let mut fx = AutoLevel::new(12, 1.0, 0, true);
        let mut buf = make(64, 64, 3, |x, y, c| {
            let base = 12 + ((x + y) % 40) as u8;
            match c {
                0 => base,
                2 => base,
                _ => 12 + ((x + y) % 40) as u8 * 2,
            }
        });
        fx.apply(&mut buf, 64, 3);
        let (mr, mg, mb) = (
            mean(&buf.iter().step_by(3).copied().collect::<Vec<_>>()),
            mean(&buf.iter().skip(1).step_by(3).copied().collect::<Vec<_>>()),
            mean(&buf.iter().skip(2).step_by(3).copied().collect::<Vec<_>>()),
        );
        let spread = [mr, mg, mb].iter().cloned().fold(0.0f32, f32::max)
            - [mr, mg, mb].iter().cloned().fold(255.0f32, f32::min);
        assert!(
            spread < 12.0,
            "AWB 后三通道均值应当接近，实际 R={mr:.1} G={mg:.1} B={mb:.1}"
        );
        assert!(
            fx.wb[0] > 1.5 && fx.wb[2] > 1.5,
            "红蓝增益应当被抬起来：{:?}",
            fx.wb
        );
    }

    #[test]
    fn highlights_are_desaturated() {
        let mut buf = make(8, 8, 3, |_, _, c| match c {
            0 => 255,
            1 => 250,
            _ => 230,
        });
        desaturate_highlights(&mut buf, 64);
        let p = [buf[0], buf[1], buf[2]];
        let spread = p.iter().max().unwrap() - p.iter().min().unwrap();
        assert!(spread < 8, "高光应当被拉白，实际 {p:?}");
    }

    #[test]
    fn gray_world_gains_follow_the_frame() {
        // 灰世界：R/G/B 均值 60/120/30 -> 增益应约 2.0 / 1.0 / 4.0（夹到 4.0 上限）
        let mut fx = AutoLevel::new(0, 1.0, 0, true);
        let mut buf = make(64, 64, 3, |_, _, c| match c {
            0 => 60,
            1 => 120,
            _ => 30,
        });
        fx.apply(&mut buf, 64, 3);
        assert!((fx.wb[0] - 2.0).abs() < 0.01, "R 增益 {:?}", fx.wb);
        assert!((fx.wb[2] - 4.0).abs() < 0.01, "B 增益 {:?}", fx.wb);
    }

    #[test]
    fn awb_gain_is_clamped() {
        // 极端偏色（只有红有值）：增益必须被夹住，不能起飞
        let mut fx = AutoLevel::new(0, 1.0, 0, true);
        let mut buf = make(32, 32, 3, |_, _, c| if c == 0 { 200 } else { 0 });
        fx.apply(&mut buf, 32, 3);
        for g in fx.wb {
            assert!(
                (WB_MIN..=WB_MAX).contains(&g),
                "白平衡增益越界：{g} 不在 {WB_MIN}..{WB_MAX}"
            );
        }
    }

    #[test]
    fn gray_path_ignores_awb() {
        let mut fx = AutoLevel::new(0, 1.0, 0, true);
        let mut buf = make(16, 16, 1, |x, _, _| x as u8);
        let before = buf.clone();
        fx.apply(&mut buf, 16, 1);
        assert_eq!(buf, before, "单通道下 AWB 不该产生任何效果");
        assert_eq!(fx.wb, [1.0; 3]);
    }

    #[test]
    fn smoothing_is_applied_across_frames() {
        // 第二帧换一个亮度：增益应当只走一部分（EMA 0.35），不是直接跳过去
        let mut fx = AutoLevel::new(0, 1.0, 128, false);
        let mut dark = make(32, 32, 1, |_, _, _| 20);
        fx.apply(&mut dark, 32, 1);
        let g1 = fx.gain;
        // 第一帧直接采用本帧算出的值：128 目标 / 中位数 20.5
        assert!(
            (g1 - 128.0 / 20.5).abs() < 0.01,
            "第一帧应当直接采用，实际 {g1}"
        );

        let mut bright = make(32, 32, 1, |_, _, _| 200);
        fx.apply(&mut bright, 32, 1);
        let g2 = fx.gain;
        // 本帧目标增益 128/200.5 < 1，被「只提亮不压暗」夹到 1；EMA 只往 1 走 35%。
        let g_target = 1.0f32;
        assert!(
            g_target < g2 && g2 < g1,
            "第二帧应当只往目标走一部分：目标 {g_target:.3} < 实际 {g2:.3} < 上一帧 {g1:.3}"
        );
        let want = (g1.ln() * 0.65 + g_target.ln() * 0.35).exp();
        assert!((g2 - want).abs() < 1e-3, "实际 {g2:.4} 期望 {want:.4}");
    }

    #[test]
    fn rejects_odd_shapes_without_panicking() {
        let mut fx = AutoLevel::new(12, 2.2, 100, true);
        let mut empty: Vec<u8> = Vec::new();
        fx.apply(&mut empty, 0, 3); // 宽 0
        fx.apply(&mut empty, 4, 3); // 不足一行
        fx.apply(&mut empty, 4, 0); // 通道数非法
        fx.apply(&mut empty, 4, 7);
        let mut buf = vec![5u8; 3];
        fx.apply(&mut buf, 1, 3); // 1×1 也要能跑
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn dither_keeps_average_close_to_undithered() {
        // 强增益下逐像素会有 ±1 的抖动差，但整帧均值必须贴着「无抖动的量化值」
        let mut fx = AutoLevel::new(12, 2.2, 0, false);
        fx.gain = 6.0;
        let lut = build_lut(1, fx.gain, &[1.0; 3], fx.gamma);
        let base: Vec<f64> = (0..256)
            .map(|v| (gamma_encode(v as f32 * 6.0, 2.2) + 0.5).min(255.0) as f64)
            .collect();
        for (p, phase) in lut.iter().enumerate() {
            let s: f64 = phase.iter().map(|&v| v as f64).sum();
            let want: f64 = base.iter().sum();
            assert!(
                (s - want).abs() < 130.0,
                "相位 {p} 的抖动偏置太大：{s} vs {want}"
            );
            for (v, &got) in phase.iter().enumerate() {
                assert!(
                    (got as i32 - base[v] as i32).abs() <= 1,
                    "抖动只该差 ±1：相位 {p} v={v} 得 {got} 期望 {}",
                    base[v]
                );
            }
        }
    }

    #[test]
    fn status_line_reports_state() {
        let fx = AutoLevel::new(0, 1.0, 0, false);
        assert!(fx.status().contains("关"));
        let fx = AutoLevel::new(12, 2.2, 100, true);
        let s = fx.status();
        assert!(s.contains("12") && s.contains("2.20"), "{s}");
    }
}
