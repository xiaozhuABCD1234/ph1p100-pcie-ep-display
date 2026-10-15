//! Bayer 去马赛克（纯 Rust 双线性，无外部 crate）。
//!
//! 采集通路写进帧缓存的是每个像素高 8 位的 Bayer 马赛克 —— 每个像素只有 R/G/B
//! 里一个通道，整帧相位一致但未知，由 `--bayer` 指定。`BayerPhase::Gray` 表示
//! 「不当作马赛克」，走原来的单分量灰度通路。
//!
//! 算法是最朴素的双线性：某输出通道若正好采样在本像素就直接取原值，否则取
//! 3×3 邻域里**属于该通道**的邻居求平均。对 2×2 相位图这个统一规则展开后就是
//! 经典双线性 —— 绿点上 R/B 各取同轴的两个邻居，红/蓝点上 G 取四个正交邻居、
//! 另一个色取四个对角邻居。相位只进这张「哪个位置是哪个通道」的表，所以四种
//! 相位共用同一段代码，不会出现「只换标签不出结果」。

/// Bayer 相位：2×2 重复单元里 R/G/B 的排布，口语按「第一行左起、第二行左起」写。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BayerPhase {
    /// 不做去马赛克，保持原有单分量灰度输出
    Gray,
    /// 第一行 R G、第二行 G B
    Rggb,
    /// 第一行 B G、第二行 G R
    Bggr,
    /// 第一行 G R、第二行 B G
    Grbg,
    /// 第一行 G B、第二行 G R
    Gbrg,
}

/// 通道编号：0=红 1=绿 2=蓝
const N_CH: usize = 3;

impl BayerPhase {
    /// 命令行取值 → 相位。大小写不敏感；`gray` 是默认值。
    pub fn parse(s: &str) -> Option<BayerPhase> {
        match s.to_ascii_lowercase().as_str() {
            "gray" | "grey" => Some(BayerPhase::Gray),
            "rggb" => Some(BayerPhase::Rggb),
            "bggr" => Some(BayerPhase::Bggr),
            "grbg" => Some(BayerPhase::Grbg),
            "gbrg" => Some(BayerPhase::Gbrg),
            _ => None,
        }
    }

    /// 命令行取值的规范写法，给日志与 README 用
    pub fn as_str(self) -> &'static str {
        match self {
            BayerPhase::Gray => "gray",
            BayerPhase::Rggb => "rggb",
            BayerPhase::Bggr => "bggr",
            BayerPhase::Grbg => "grbg",
            BayerPhase::Gbrg => "gbrg",
        }
    }

    pub fn is_gray(self) -> bool {
        self == BayerPhase::Gray
    }

    /// 2×2 相位图，按 `(y&1)*2 + (x&1)` 索引，元素是通道号（0=红 1=绿 2=蓝）。
    /// 灰度相位没有相位图，返回 `None`。
    pub fn quad(self) -> Option<[u8; 4]> {
        match self {
            BayerPhase::Gray => None,
            // 顺序：第一行左、第一行右、第二行左、第二行右
            BayerPhase::Rggb => Some([0, 1, 1, 2]),
            BayerPhase::Bggr => Some([2, 1, 1, 0]),
            BayerPhase::Grbg => Some([1, 0, 2, 1]),
            BayerPhase::Gbrg => Some([1, 2, 0, 1]),
        }
    }
}

/// 某个相位类别（2×2 里的一种格子）下，某个输出通道可用的邻居偏移。
/// 最多 4 个：绿点取同轴两个邻居，红/蓝点取四个对角邻居。
#[derive(Clone, Copy, Default)]
struct Offsets {
    xs: [i8; 4],
    ys: [i8; 4],
    n: usize,
}

/// 预计算邻居表：`表[相位类别][输出通道]` → 3×3 邻域里属于该通道的偏移。
/// 相位类别 = `(y&1)*2 + (x&1)`，所以整帧只有 4 张表，每帧算一次就够。
fn build_offsets(quad: [u8; 4]) -> [[Offsets; N_CH]; 4] {
    let mut table = [[Offsets::default(); N_CH]; 4];
    for (p, per_parity) in table.iter_mut().enumerate() {
        // p/2 = 行奇偶，p%2 = 列奇偶
        let (py, px) = (p as i8 / 2, p as i8 % 2);
        for (c, slot) in per_parity.iter_mut().enumerate() {
            for dy in -1i8..=1 {
                for dx in -1i8..=1 {
                    if dx == 0 && dy == 0 {
                        continue; // 中心若是本通道，直接取原值，不进这张表
                    }
                    let ny = (py + dy).rem_euclid(2) as usize;
                    let nx = (px + dx).rem_euclid(2) as usize;
                    if quad[ny * 2 + nx] as usize == c {
                        slot.xs[slot.n] = dx;
                        slot.ys[slot.n] = dy;
                        slot.n += 1;
                    }
                }
            }
        }
    }
    table
}

/// Bayer 马赛克 → RGB8（长度 `width*height*3`）。
///
/// 边界按「夹到帧内」处理（越界邻居取边缘像素），因此奇数宽高、1 像素高的帧也能跑。
pub fn demosaic(
    raw: &[u8],
    width: usize,
    height: usize,
    phase: BayerPhase,
) -> Result<Vec<u8>, String> {
    let quad = phase
        .quad()
        .ok_or_else(|| "gray 相位不做去马赛克".to_string())?;
    if width == 0 || height == 0 {
        return Err(format!("几何非法：{width}x{height}"));
    }
    if raw.len() < width * height {
        return Err(format!("帧数据不够：{} 字节 < {width}x{height}", raw.len()));
    }
    let table = build_offsets(quad);
    let mut rgb = vec![0u8; width * height * N_CH];
    let (w_i, h_i) = (width as i32, height as i32);

    for y in 0..height {
        for x in 0..width {
            let src = y * width + x;
            let own = quad[(y & 1) * 2 + (x & 1)] as usize;
            let dst = src * N_CH;
            for (c, out) in rgb[dst..dst + N_CH].iter_mut().enumerate() {
                *out = if own == c {
                    raw[src]
                } else {
                    let nb = &table[(y & 1) * 2 + (x & 1)][c];
                    debug_assert!(nb.n > 0, "每个通道在 3×3 邻域里至少有一个采样点");
                    let mut sum = 0u32;
                    for k in 0..nb.n {
                        let sx = (x as i32 + nb.xs[k] as i32).clamp(0, w_i - 1) as usize;
                        let sy = (y as i32 + nb.ys[k] as i32).clamp(0, h_i - 1) as usize;
                        sum += raw[sy * width + sx] as u32;
                    }
                    ((sum + nb.n as u32 / 2) / nb.n as u32) as u8
                };
            }
        }
    }
    Ok(rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把 RGB8 按相位图抽成 Bayer 马赛克（测试用的「反向」操作）
    fn mosaic(rgb: &[u8], width: usize, height: usize, quad: [u8; 4]) -> Vec<u8> {
        let mut out = vec![0u8; width * height];
        for y in 0..height {
            for x in 0..width {
                let c = quad[(y & 1) * 2 + (x & 1)] as usize;
                out[y * width + x] = rgb[(y * width + x) * N_CH + c];
            }
        }
        out
    }

    /// 一张线性渐变的「真彩色」图：三个通道都是坐标的一次函数。
    /// 一次函数在双线性插值下是精确的，所以去马赛克应当逐像素还原出原值。
    fn linear_rgb(width: usize, height: usize) -> Vec<u8> {
        let mut rgb = vec![0u8; width * height * N_CH];
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * N_CH;
                rgb[i] = (20 + 3 * x) as u8; // 红
                rgb[i + 1] = (40 + 2 * y) as u8; // 绿
                rgb[i + 2] = (10 + 2 * x + 3 * y) as u8; // 蓝
            }
        }
        rgb
    }

    #[test]
    fn parses_all_names() {
        assert_eq!(BayerPhase::parse("gray"), Some(BayerPhase::Gray));
        assert_eq!(BayerPhase::parse("RGGB"), Some(BayerPhase::Rggb));
        assert_eq!(BayerPhase::parse("bggr"), Some(BayerPhase::Bggr));
        assert_eq!(BayerPhase::parse("grbg"), Some(BayerPhase::Grbg));
        assert_eq!(BayerPhase::parse("Gbrg"), Some(BayerPhase::Gbrg));
        assert_eq!(BayerPhase::parse("rgbg"), None);
    }

    #[test]
    fn quads_match_their_names() {
        // 名字里第 1 个字母 = 第一行左边，第 2 个 = 第一行右边，以此类推
        assert_eq!(BayerPhase::Rggb.quad(), Some([0, 1, 1, 2]));
        assert_eq!(BayerPhase::Bggr.quad(), Some([2, 1, 1, 0]));
        assert_eq!(BayerPhase::Grbg.quad(), Some([1, 0, 2, 1]));
        assert_eq!(BayerPhase::Gbrg.quad(), Some([1, 2, 0, 1]));
        assert_eq!(BayerPhase::Gray.quad(), None);
        // 四个相位里绿都占两格、红蓝各一格
        for p in [
            BayerPhase::Rggb,
            BayerPhase::Bggr,
            BayerPhase::Grbg,
            BayerPhase::Gbrg,
        ] {
            let q = p.quad().unwrap();
            assert_eq!(q.iter().filter(|&&c| c == 1).count(), 2, "{p:?}");
            assert_eq!(q.iter().filter(|&&c| c == 0).count(), 1, "{p:?}");
            assert_eq!(q.iter().filter(|&&c| c == 2).count(), 1, "{p:?}");
        }
    }

    #[test]
    fn bilinear_reproduces_linear_ramps_exactly() {
        let (w, h) = (24usize, 20usize);
        let src = linear_rgb(w, h);
        for phase in [
            BayerPhase::Rggb,
            BayerPhase::Bggr,
            BayerPhase::Grbg,
            BayerPhase::Gbrg,
        ] {
            let raw = mosaic(&src, w, h, phase.quad().unwrap());
            let got = demosaic(&raw, w, h, phase).unwrap();
            assert_eq!(got.len(), w * h * 3);
            // 内部像素（3×3 邻域完整、不受边界夹取影响）必须逐像素还原
            for y in 1..h - 1 {
                for x in 1..w - 1 {
                    for c in 0..3 {
                        let i = (y * w + x) * 3 + c;
                        assert_eq!(
                            got[i], src[i],
                            "相位 {phase:?} 在 ({x},{y}) 的通道 {c} 没还原"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rggb_and_bggr_differ_only_by_r_b_swap() {
        // 同一帧按 RGGB 与 BGGR 去马赛克，输出恰好是 R↔B 互换 —— 这正是「单帧
        // 数据无法区分这两个相位」的代数原因，写进测试免得以后有人以为能靠指标定。
        let (w, h) = (16usize, 12usize);
        let mut raw = vec![0u8; w * h];
        let mut seed = 12345u32;
        for v in raw.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = (seed >> 24) as u8;
        }
        let a = demosaic(&raw, w, h, BayerPhase::Rggb).unwrap();
        let b = demosaic(&raw, w, h, BayerPhase::Bggr).unwrap();
        for i in 0..w * h {
            assert_eq!(a[i * 3], b[i * 3 + 2], "像素 {i}：R 与 B 应当互换");
            assert_eq!(a[i * 3 + 2], b[i * 3], "像素 {i}：B 与 R 应当互换");
            assert_eq!(a[i * 3 + 1], b[i * 3 + 1], "像素 {i}：G 应当相同");
        }
    }

    #[test]
    fn green_sites_use_axis_aligned_neighbours() {
        // RGGB 里绿点在 (x 奇, y 偶) 与 (x 偶, y 奇)，红蓝点在两条对角上。
        // 绿点的红/蓝必须来自同轴的两个邻居，不能拿四个对角：把红只放在偶数列，
        // 绿点的红就应当取到 200（同轴），若误用对角邻居会得到 0。
        let (w, h) = (8usize, 8usize);
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                rgb[i] = if x % 2 == 0 { 200 } else { 0 }; // 红只出现在偶数列
                rgb[i + 1] = 100; // 绿处处 100
                rgb[i + 2] = 50; // 蓝处处 50
            }
        }
        let raw = mosaic(&rgb, w, h, BayerPhase::Rggb.quad().unwrap());
        let got = demosaic(&raw, w, h, BayerPhase::Rggb).unwrap();
        // 绿点 (1,0)：左右邻居都是红点（200）→ 红应当是 200
        let g = 3; // 像素 (x=1, y=0) 的起始字节
        assert_eq!(got[g], 200, "绿点的红应当取同轴邻居");
        assert_eq!(got[g + 1], 100, "绿点自身就是绿");
        // 蓝点 (1,1)：蓝取自身；红取四个对角红点（都是 200）；绿取四个正交绿点（100）
        let b = (w + 1) * 3;
        assert_eq!(got[b], 200, "蓝点的红应当取四个对角邻居");
        assert_eq!(got[b + 1], 100, "蓝点的绿应当取四个正交邻居");
        assert_eq!(got[b + 2], 50, "蓝点自身就是蓝");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(demosaic(&[0; 4], 2, 2, BayerPhase::Gray).is_err());
        assert!(demosaic(&[0; 3], 2, 2, BayerPhase::Rggb).is_err());
        assert!(demosaic(&[], 0, 0, BayerPhase::Rggb).is_err());
        // 1×1 也要能跑（边界夹取）
        assert_eq!(demosaic(&[9], 1, 1, BayerPhase::Rggb).unwrap().len(), 3);
    }
}
