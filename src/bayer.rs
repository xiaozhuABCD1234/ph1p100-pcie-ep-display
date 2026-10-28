//! Bayer 去马赛克（纯 Rust 双线性，无外部 crate）。
//!
//! 采集通路写进帧缓存的是每个像素高 8 位的 Bayer 马赛克 —— 每个像素只有 R/G/B
//! 里一个通道，整帧相位一致但未知，由 `--bayer` 指定。`BayerPhase::Gray` 表示
//! 「不当作马赛克」，走原来的单分量灰度通路。
//!
//! 本板实测相位是 **`gbrg`**（次选 `grbg`，两者逐像素只差红蓝互换），依据见
//! `main.rs` 里 `DEF_BAYER` 的注释 —— 注意不是早期 README 写的 `bggr`。
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
    /// 按 R10F 头 camera_id 选：A(0)=RGGB、B(1)=BGGR（当前机械安装）。
    /// 仅供 `encode_frame` 在拿到帧元数据后解析；`quad()` 不直接支持。
    Auto,
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
            "auto" => Some(BayerPhase::Auto),
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
            BayerPhase::Auto => "auto",
        }
    }

    /// 把 `auto` 解析成具体相位。当前机械安装：A 路 180° 与 B 相反，
    /// 两块模组都正装，A/B 均为 RGGB；R10F 头 0x28 的 3(GBRG) 是占位值，不能用。
    /// 同一办公室告示牌在 A/B 上都还原成黄色，而 BGGR 会把它们都变蓝。
    pub fn resolve(self, camera_id: Option<u8>) -> BayerPhase {
        let _ = camera_id;
        match self {
            BayerPhase::Auto => BayerPhase::Rggb,
            other => other,
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
            BayerPhase::Auto => None,
        }
    }
}

/// 某个相位类别（2×2 里的一种格子）下，某个输出通道可用的邻居偏移。
/// 最多 4 个：绿点取同轴两个邻居，红/蓝点取四个对角邻居。
#[cfg(test)]
#[derive(Clone, Copy, Default)]
struct Offsets {
    xs: [i8; 4],
    ys: [i8; 4],
    n: usize,
}

/// 预计算邻居表：`表[相位类别][输出通道]` → 3×3 邻域里属于该通道的偏移。
/// 相位类别 = `(y&1)*2 + (x&1)`，所以整帧只有 4 张表，每帧算一次就够。
#[cfg(test)]
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

/// Bayer 马赛克 → RGB8（长度 width*height*3）。
///
/// 双线性插值，边界按「夹到帧内」处理（越界邻居取边缘像素），所以奇数宽高、
/// 1 像素高的帧也能跑。
///
/// 实现上先把 3×3 的邻居索引算成 8 个数，再按「本格是什么颜色」分两种情况：
///   * 绿点：水平邻居同色、垂直邻居同色，正好是 R 和 B 各两个 → 各取轴向均值；
///   * 红/蓝点：另外两个通道是绿 + 另一个色 → 绿取四个轴向均值、另一色取四个对角均值。
/// 这覆盖了四种 Bayer 排布，不需要通用邻居表，也不用逐像素进 3 次分支。
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
    // 上限闸：RGB 输出是像素数的 3 倍，不设闸就会在畸形几何上触发分配失败 abort
    if width as u64 * height as u64 > crate::source::MAX_FRAME_BYTES as u64 {
        return Err(format!(
            "几何 {width}x{height} 超过本程序上限 {} 字节/帧",
            crate::source::MAX_FRAME_BYTES
        ));
    }
    if raw.len() < width * height {
        return Err(format!("帧数据不够：{} 字节 < {width}x{height}", raw.len()));
    }
    let mut rgb = vec![0u8; width * height * N_CH];
    demosaic_into(raw, width, height, quad, &mut rgb);
    Ok(rgb)
}

/// 去马赛克主体。quad 是 2×2 单元里四个格子的通道号（0=R 1=G 2=B）。
///
/// 索引全部预先算好，内层用裸指针读写：这是每帧 200 万像素的热路径，
/// 边界检查会占掉三分之一时间。所有下标都能证明落在缓冲内：
/// row/up/dn 是整行起点，xm/xp 被夹在 0..width-1，dst+2 < width*height*3。
fn demosaic_into(raw: &[u8], width: usize, height: usize, quad: [u8; 4], out: &mut [u8]) {
    debug_assert!(raw.len() >= width * height);
    debug_assert!(out.len() >= width * height * N_CH);
    let last_row = (height - 1) * width;
    let src = raw.as_ptr();
    let dst_ptr = out.as_mut_ptr();
    for y in 0..height {
        let py = y & 1;
        let row = y * width;
        let up = if y == 0 { 0 } else { row - width };
        let dn = if y + 1 >= height {
            last_row
        } else {
            row + width
        };
        for x in 0..width {
            let px = x & 1;
            let own = quad[py * 2 + px] as usize;
            let xm = x.saturating_sub(1);
            let xp = if x + 1 >= width { width - 1 } else { x + 1 };
            let c = row + x;
            let dst = c * N_CH;
            // SAFETY: 见函数文档；up/dn 各行起点加 xm/xp 必在 0..w*h 内，
            // dst 是第 c 个像素的三字节起点，dst+2 <= w*h*3-1。
            unsafe {
                let l = *src.add(row + xm) as u32;
                let r = *src.add(row + xp) as u32;
                let u = *src.add(up + x) as u32;
                let d = *src.add(dn + x) as u32;
                *dst_ptr.add(dst + own) = *src.add(c);
                if own == 1 {
                    // 绿点：水平方向同色、垂直方向同色，两者分别是 R 和 B
                    let h_ch = quad[py * 2 + (1 - px)] as usize;
                    let v_ch = quad[(1 - py) * 2 + px] as usize;
                    *dst_ptr.add(dst + h_ch) = ((l + r + 1) >> 1) as u8;
                    *dst_ptr.add(dst + v_ch) = ((u + d + 1) >> 1) as u8;
                } else {
                    // 红/蓝点：绿取四个轴向邻居，另一个色取四个对角邻居
                    let ul = *src.add(up + xm) as u32;
                    let ur = *src.add(up + xp) as u32;
                    let dl = *src.add(dn + xm) as u32;
                    let dr = *src.add(dn + xp) as u32;
                    *dst_ptr.add(dst + 1) = ((l + r + u + d + 2) >> 2) as u8;
                    *dst_ptr.add(dst + (2 - own)) = ((ul + ur + dl + dr + 2) >> 2) as u8;
                }
            }
        }
    }
}

/// CFA binning：**去马赛克 + 降采样一步做**，输出 `(w/factor, h/factor)` 的 RGB8。
///
/// 为什么不让上层「先全分辨率 `demosaic`、再 `scale::box_rgb`」：实测 4K 彩色帧
/// 327ms/帧，其中绝大部分是那条 25MB 的 RGB 中间缓冲（去马赛克 + 逐像素后处理），
/// 而 JPEG 本身只占几十毫秒。2×2 块里恰好 R/G/G/B 各一个采样 —— 直接按通道
/// 累加就是**真正的传感器 binning**（无插值、无假彩色），还顺带把中间缓冲缩到 1/4。
///
/// 约定：`factor` 必须是 ≥2 的偶数（`--scale` 只允许 1/2/4/8），块边界与 2×2 相位
/// 对齐，所以每个输出像素拿到的通道数与 2×2 单元一致（R 1 : G 2 : B 1，按面积缩放）。
pub fn bin_rgb(
    raw: &[u8],
    width: usize,
    height: usize,
    factor: usize,
    phase: BayerPhase,
) -> Result<Vec<u8>, String> {
    let quad = phase
        .quad()
        .ok_or_else(|| "gray 相位不做 binning".to_string())?;
    if factor < 2 || factor % 2 != 0 {
        return Err(format!("binning 只接受 ≥2 的偶数倍率，收到 {factor}"));
    }
    let (ow, oh) = (width / factor, height / factor);
    if ow == 0 || oh == 0 {
        return Err(format!("几何 {width}x{height} 太小，缩不了 1/{factor}"));
    }
    if raw.len() < width * height {
        return Err(format!("帧数据不够：{} 字节 < {width}x{height}", raw.len()));
    }
    // 每个输出像素里各通道的采样数：2×2 单元是 1:2:1，factor 越大按 (factor/2)² 放大
    let mul = ((factor / 2) * (factor / 2)) as u32;
    let mut count = [0u32; N_CH];
    for &c in quad.iter() {
        count[c as usize] += mul;
    }
    // 归一化用移位：--scale 允许的 2/4/8 下 count 全是 2 的幂（(f/2)² × 1:2:1），
    // ARM 上整数除法比移位贵一个量级，而这是 4K 预览每帧固定要跑的一遍。
    // 其它偶数因子（比如 6）count 不是 2 的幂时才退回除法。
    let mut shift = [0u32; N_CH];
    let mut half = [0u32; N_CH];
    let mut pow2 = true;
    for c in 0..N_CH {
        shift[c] = count[c].trailing_zeros();
        half[c] = count[c] / 2;
        pow2 &= count[c].is_power_of_two();
    }

    let mut out = vec![0u8; ow * oh * N_CH];
    let mut acc = vec![0u32; ow * N_CH];
    for oy in 0..oh {
        acc.iter_mut().for_each(|v| *v = 0);
        for dy in 0..factor {
            let start = (oy * factor + dy) * width;
            let row = &raw[start..start + width];
            // 行内按 2 字节一对走：偶数列/奇数列各属于 quad 里固定的两个通道
            let (c0, c1) = if dy % 2 == 0 {
                (quad[0] as usize, quad[1] as usize)
            } else {
                (quad[2] as usize, quad[3] as usize)
            };
            for (ox, chunk) in row.chunks_exact(factor).take(ow).enumerate() {
                // 一个输出像素覆盖 factor 列 = factor/2 对；偶/奇列的通道固定是 c0/c1
                let mut s0 = 0u32;
                let mut s1 = 0u32;
                for pair in chunk.chunks_exact(2) {
                    s0 += pair[0] as u32;
                    s1 += pair[1] as u32;
                }
                acc[ox * N_CH + c0] += s0;
                acc[ox * N_CH + c1] += s1;
            }
        }
        let orow = &mut out[oy * ow * N_CH..(oy + 1) * ow * N_CH];
        if pow2 {
            for (o, a) in orow.chunks_exact_mut(N_CH).zip(acc.chunks_exact(N_CH)) {
                for c in 0..N_CH {
                    // (a + count/2) / count，count = 1 << shift：逐位等价，只是更快
                    o[c] = ((a[c] + half[c]) >> shift[c]) as u8;
                }
            }
        } else {
            for (o, a) in orow.chunks_exact_mut(N_CH).zip(acc.chunks_exact(N_CH)) {
                for c in 0..N_CH {
                    o[c] = ((a[c] + count[c] / 2) / count[c]) as u8;
                }
            }
        }
    }
    Ok(out)
}

/// 改造前的通用实现（邻居表 + 逐像素三通道循环）。
/// 只用于测试：保证新的算术实现与它逐字节一致，颜色不会因为提速而漂移。
#[cfg(test)]
fn demosaic_reference(raw: &[u8], width: usize, height: usize, quad: [u8; 4]) -> Vec<u8> {
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
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_demosaic_matches_reference() {
        // 逐字节对齐旧实现：提速不能改变任何像素，否则颜色又要重调一遍
        let mut seed = 0x1234_5678u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 16) as u8
        };
        let sizes = [
            (1, 1),
            (2, 2),
            (3, 3),
            (5, 4),
            (8, 6),
            (17, 9),
            (1, 7),
            (7, 1),
            (64, 48),
        ];
        for (w, h) in sizes {
            let raw: Vec<u8> = (0..w * h).map(|_| rnd()).collect();
            for phase in [
                BayerPhase::Rggb,
                BayerPhase::Bggr,
                BayerPhase::Grbg,
                BayerPhase::Gbrg,
            ] {
                let quad = phase.quad().expect("彩相位都有 quad");
                let mut fast = vec![0u8; w * h * N_CH];
                demosaic_into(&raw, w, h, quad, &mut fast);
                let want = demosaic_reference(&raw, w, h, quad);
                assert_eq!(fast, want, "{w}x{h} {phase:?} 与旧实现不一致");
            }
        }
    }

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
        assert_eq!(BayerPhase::parse("auto"), Some(BayerPhase::Auto));
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
        assert_eq!(BayerPhase::Auto.quad(), None);
        assert_eq!(BayerPhase::Auto.resolve(Some(0)), BayerPhase::Rggb);
        assert_eq!(BayerPhase::Auto.resolve(Some(1)), BayerPhase::Rggb);
        assert_eq!(BayerPhase::Auto.resolve(None), BayerPhase::Rggb);
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

    /// 缓变的三通道渐变（每像素 1~2 级）：双线性插值对一次函数是精确的，
    /// 所以去马赛克在内部像素能还原原值；2×2 binning 取的是块内同色站点，
    /// 与 2×2 盒均值只差半个像素的台阶（≤2 级）。
    fn ramp_rgb(width: usize, height: usize) -> Vec<u8> {
        let mut rgb = vec![0u8; width * height * N_CH];
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * N_CH;
                rgb[i] = (40 + 2 * x) as u8;
                rgb[i + 1] = (100 + y) as u8;
                rgb[i + 2] = (180 - x - y) as u8;
            }
        }
        rgb
    }

    /// 把 4×4 原始 Bayer 镜像外扩成 8×8（四周各 2 像素 = 偶数，2×2 相位不变），
    /// 再去马赛克 + 2×2 盒式缩，取覆盖原图的那 2×2 个箱作为参考。
    ///
    /// 外扩是为了让参考像素避开双线性的帧边界夹取：4×4 直接跑去马赛克时，
    /// 边界会把本像素当邻居、读到别的滤色片，最大差能到 35 级以上 —— 那是参考
    /// 实现的边界处理，不是 binning 的色差。镜像外扩保持相位，参考就全是内部像素。
    fn padded_reference_4x4(raw4: &[u8], phase: BayerPhase) -> Vec<u8> {
        fn mirror(i: isize) -> usize {
            if i < 0 {
                (-i) as usize
            } else if i > 3 {
                (6 - i) as usize
            } else {
                i as usize
            }
        }
        let mut raw8 = vec![0u8; 8 * 8];
        for y in 0..8 {
            for x in 0..8 {
                raw8[y * 8 + x] = raw4[mirror(y as isize - 2) * 4 + mirror(x as isize - 2)];
            }
        }
        let full = demosaic(&raw8, 8, 8, phase).unwrap();
        let boxed = crate::scale::box_rgb(&full, 8, 8, 2);
        let mut out = Vec::with_capacity(4 * N_CH);
        for by in 0..2 {
            for bx in 0..2 {
                let src = ((by + 1) * 4 + (bx + 1)) * N_CH;
                out.extend_from_slice(&boxed[src..src + N_CH]);
            }
        }
        out
    }

    /// 4×4 已知图案：binning 与「全分辨率双线性去马赛克再 2×2 盒式平均」的
    /// 色差 ≤ 2 级（实测 1 级），四种相位都成立；首块再对一次手算值，
    /// 证明每个通道只加自己那一格、不混别的滤色片。
    ///
    /// 4×4 输入只有 2×2 输出，每个输出像素都贴着帧边界，因此参考必须在
    /// 「4×4 是某张更大图的一部分」的前提下算（见 `padded_reference_4x4`）；
    /// 直接对 4×4 跑去马赛克再平均，最大差是 35 级以上，那是边界夹取误差。
    #[test]
    fn binning_matches_demosaic_then_box_on_known_4x4() {
        let src = ramp_rgb(4, 4);
        for phase in [
            BayerPhase::Rggb,
            BayerPhase::Bggr,
            BayerPhase::Grbg,
            BayerPhase::Gbrg,
        ] {
            let raw = mosaic(&src, 4, 4, phase.quad().unwrap());
            let binned = bin_rgb(&raw, 4, 4, 2, phase).unwrap();
            let reference = padded_reference_4x4(&raw, phase);
            assert_eq!(binned.len(), reference.len());
            for (i, (&got, &want)) in binned.iter().zip(reference.iter()).enumerate() {
                let d = (got as i32 - want as i32).abs();
                assert!(d <= 2, "{phase:?} 输出第 {i} 个字节色差 {d} > 2");
            }
        }
        // RGGB 首箱手算：R(0,0)=40；G=(G(0,1)+G(1,0))/2=(101+100+1)/2=101；
        // B(1,1)=180-1-1=178
        let raw = mosaic(&src, 4, 4, BayerPhase::Rggb.quad().unwrap());
        let binned = bin_rgb(&raw, 4, 4, 2, BayerPhase::Rggb).unwrap();
        assert_eq!(&binned[..N_CH], &[40, 101, 178], "首箱必须是手算的同色均值");
    }

    /// 16×16 渐变：binning 与「全分辨率去马赛克再 2×2 平均」在内部像素上的
    /// 最大色差 ≤ 2 级（实测 1 级，只来自四舍五入），四种相位都要成立。
    ///
    /// 只比内部：帧边界那一圈是双线性去马赛克的夹取误差（与 binning 无关），
    /// 16×16 输入才留得出 1 圈内部（输出 8×8 → 内部 6×6）。
    #[test]
    fn binning_matches_demosaic_then_box_on_gradient() {
        let (w, h) = (16usize, 16usize);
        let src = ramp_rgb(w, h);
        let (ow, oh) = (w / 2, h / 2);
        for phase in [
            BayerPhase::Rggb,
            BayerPhase::Bggr,
            BayerPhase::Grbg,
            BayerPhase::Gbrg,
        ] {
            let raw = mosaic(&src, w, h, phase.quad().unwrap());
            let binned = bin_rgb(&raw, w, h, 2, phase).unwrap();
            let full = demosaic(&raw, w, h, phase).unwrap();
            let boxed = crate::scale::box_rgb(&full, w, h, 2);
            for oy in 1..oh - 1 {
                for ox in 1..ow - 1 {
                    for c in 0..3 {
                        let i = (oy * ow + ox) * 3 + c;
                        let d = (binned[i] as i32 - boxed[i] as i32).abs();
                        assert!(d <= 2, "{phase:?} 在输出 ({ox},{oy}) 通道 {c} 色差 {d} > 2");
                    }
                }
            }
        }
    }

    #[test]
    fn bin_rgb_2x2_is_true_binning() {
        // gbrg 的 2×2 单元：G B / R G
        //   10 20
        //   30 40
        let raw = [10u8, 20, 30, 40];
        let got = bin_rgb(&raw, 2, 2, 2, BayerPhase::Gbrg).unwrap();
        // R = 30（只有一个红点）、G = (10+40)/2 = 25、B = 20
        assert_eq!(got, vec![30, 25, 20]);
    }

    #[test]
    fn bin_rgb_factor4_averages_the_whole_block() {
        // 同一相位、4×4 全同值 → 输出仍是该值（不会因为累加/归一化跑偏）
        let raw = vec![7u8; 16];
        let got = bin_rgb(&raw, 4, 4, 4, BayerPhase::Rggb).unwrap();
        assert_eq!((got.len(), got[0], got[1], got[2]), (3, 7, 7, 7));
    }

    #[test]
    fn bin_rgb_shift_normalisation_matches_division() {
        // 移位是 std 优化，结果必须与「(sum + count/2) / count」逐字节一致：
        // 用 6 这种 count 不是 2 的幂的因子走除法分支，2/4/8 走移位分支，两者都要对。
        let mut seed = 0x5eed_1234u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 17) as u8
        };
        let (w, h) = (24usize, 16usize);
        let raw: Vec<u8> = (0..w * h).map(|_| rnd()).collect();
        for factor in [2usize, 4, 8] {
            for phase in [
                BayerPhase::Rggb,
                BayerPhase::Bggr,
                BayerPhase::Grbg,
                BayerPhase::Gbrg,
            ] {
                let quad = phase.quad().unwrap();
                let got = bin_rgb(&raw, w, h, factor, phase).unwrap();
                let (ow, oh) = (w / factor, h / factor);
                let mul = ((factor / 2) * (factor / 2)) as u32;
                let mut count = [0u32; N_CH];
                for &c in quad.iter() {
                    count[c as usize] += mul;
                }
                let mut want = vec![0u8; ow * oh * N_CH];
                for oy in 0..oh {
                    for ox in 0..ow {
                        let mut sum = [0u32; N_CH];
                        for dy in 0..factor {
                            for dx in 0..factor {
                                let x = ox * factor + dx;
                                let y = oy * factor + dy;
                                let c = quad[(y & 1) * 2 + (x & 1)] as usize;
                                sum[c] += raw[y * w + x] as u32;
                            }
                        }
                        for c in 0..N_CH {
                            want[(oy * ow + ox) * N_CH + c] =
                                ((sum[c] + count[c] / 2) / count[c]) as u8;
                        }
                    }
                }
                assert_eq!(got, want, "factor {factor} {phase:?}");
            }
        }
    }

    #[test]
    fn bin_rgb_matches_box_downsample_of_demosaic_on_flat_input() {
        // 平场（各点相同）时 binning 与「去马赛克再盒式缩」结果一致
        let raw = vec![42u8; 8 * 8];
        let binned = bin_rgb(&raw, 8, 8, 2, BayerPhase::Grbg).unwrap();
        let full = demosaic(&raw, 8, 8, BayerPhase::Grbg).unwrap();
        let boxed = crate::scale::box_rgb(&full, 8, 8, 2);
        assert_eq!(binned, boxed);
    }

    #[test]
    fn bin_rgb_rejects_odd_factor_and_gray() {
        let raw = vec![1u8; 16];
        assert!(bin_rgb(&raw, 4, 4, 3, BayerPhase::Rggb).is_err());
        assert!(bin_rgb(&raw, 4, 4, 2, BayerPhase::Gray).is_err());
        assert!(
            bin_rgb(&raw[..4], 4, 4, 2, BayerPhase::Rggb).is_err(),
            "数据不够应当报错"
        );
        assert!(
            bin_rgb(&raw, 2, 2, 4, BayerPhase::Rggb).is_err(),
            "缩到 0 维应当报错"
        );
    }
}
