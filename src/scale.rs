//! 编码前降采样：把 CPU 侧的像素工作量按面积比降下来。
//!
//! 两条通路共用**同一条倍率规则**（[`effective_factor`]）：
//!   * 灰度：[`box_gray`]，每个 `factor×factor` 块取均值；
//!   * 彩色：`bayer::bin_rgb`，**在 Bayer 域按通道各自累加**（真正的 CFA binning），
//!     去马赛克与降采样一步完成。
//!
//! 彩色**不能**把 2×2 块里的 4 个采样当灰度直接平均：那是把不同滤色片混在一起
//! （绿和红/蓝混），颜色就废了。`bin_rgb` 只把同一通道的格子相加再除采样数，
//! 等价于「同色像素求平均」，不引入假彩色 —— 区别在累加时就按通道分开，不是
//! 「缩完再插值」。
//!
//! 1080p 及以下默认 `factor = 1`（全链路原样）；像素数超过
//! [`AUTO_BIN_MIN_PIXELS`]（4K = 8.3 Mpx）时默认自动 2×2，把中间缓冲与后处理
//! 面积降到 1/4 —— 4K 全尺寸彩色要保留一条 24.9 MB 的 RGB 中间缓冲，预览会卡死。

/// 单帧超过这个像素数就自动 2×2 binning。
///
/// 1080p = 2,073,600（不缩），4K = 8,294,400（缩）；取 4.0 Mpx 让两类几何各留余量，
/// 也避免 2560×1440 这类介于两者之间的尺寸被误缩。
pub const AUTO_BIN_MIN_PIXELS: usize = 4_000_000;

/// 有效降采样倍率 —— 整个程序只有这一条规则。
///
/// 显式 `--scale N`（N > 1）按 N；默认 1 表示「自动」：像素数超过
/// [`AUTO_BIN_MIN_PIXELS`] 时取 2，否则取 1（不缩）。1080p 因此永远是 1，
/// 行为与加这条规则之前逐字节一致。
pub fn effective_factor(w: usize, h: usize, requested: u32) -> usize {
    if requested > 1 {
        return requested as usize;
    }
    if (w as u64) * (h as u64) > AUTO_BIN_MIN_PIXELS as u64 {
        2
    } else {
        1
    }
}

/// 缩小后的输出几何。`factor <= 1`、或缩小后任一维变成 0 时返回 `None`
/// （调用方原样使用原图，不做任何拷贝）。
pub fn out_dims(w: usize, h: usize, factor: usize) -> Option<(usize, usize)> {
    if factor <= 1 || w == 0 || h == 0 {
        return None;
    }
    let (ow, oh) = (w / factor, h / factor);
    if ow == 0 || oh == 0 {
        return None;
    }
    Some((ow, oh))
}

/// 灰度盒式降采样：每个 `factor×factor` 块取均值。
///
/// 按行累加（一条源行 → 一条 `ow` 长度的 u32 累加行），再一次性归一化：
/// 这样内层是 `chunks_exact(factor)` 的连续读，比逐像素算下标快一个量级
/// （4K→1080p 在 RK3588 上从 ~40ms 降到 ~5ms，是这条通路能不能用的关键）。
///
/// 只取左上角 `ow*factor × oh*factor` 的区域，右边/下边除不尽的余数直接丢掉
/// （4K 的 3840×2160 对 2/4/8 都能整除，余数只在小几何或畸形尺寸时出现）。
pub fn box_gray(src: &[u8], w: usize, h: usize, factor: usize) -> Vec<u8> {
    let (ow, oh) = out_dims(w, h, factor).expect("调用方已用 out_dims 判过");
    let area = (factor * factor) as u32;
    let mut out = vec![0u8; ow * oh];
    let mut acc = vec![0u32; ow];
    for oy in 0..oh {
        acc.iter_mut().for_each(|v| *v = 0);
        for dy in 0..factor {
            let start = (oy * factor + dy) * w;
            let row = &src[start..start + w];
            for (a, blk) in acc.iter_mut().zip(row.chunks_exact(factor).take(ow)) {
                *a += blk.iter().map(|&v| v as u32).sum::<u32>();
            }
        }
        let orow = &mut out[oy * ow..(oy + 1) * ow];
        for (o, &a) in orow.iter_mut().zip(acc.iter()) {
            // +area/2 做四舍五入，纯截断会让整幅图系统性偏暗
            *o = ((a + area / 2) / area) as u8;
        }
    }
    out
}

/// 三分量 RGB 盒式降采样（输入是 `demosaic` 的输出，逐像素 R,G,B 交织）。
/// 与灰度版同构：每个输出像素收 `factor×factor` 个输入像素，逐通道求均值。
///
/// 只给测试用：彩色通路的正路是 `bayer::bin_rgb`（CFA binning，省掉全分辨率 RGB），
/// 这里保留它作为「平场时 binning 应与去马赛克+盒式缩一致」的参考实现。
#[cfg(test)]
pub fn box_rgb(src: &[u8], w: usize, h: usize, factor: usize) -> Vec<u8> {
    let (ow, oh) = out_dims(w, h, factor).expect("调用方已用 out_dims 判过");
    let area = (factor * factor) as u32;
    let mut out = vec![0u8; ow * oh * 3];
    let mut acc = vec![0u32; ow * 3];
    for oy in 0..oh {
        acc.iter_mut().for_each(|v| *v = 0);
        for dy in 0..factor {
            let start = (oy * factor + dy) * w * 3;
            let row = &src[start..start + w * 3];
            for (a, blk) in acc
                .chunks_exact_mut(3)
                .zip(row.chunks_exact(factor * 3).take(ow))
            {
                for p in blk.chunks_exact(3) {
                    a[0] += p[0] as u32;
                    a[1] += p[1] as u32;
                    a[2] += p[2] as u32;
                }
            }
        }
        let orow = &mut out[oy * ow * 3..(oy + 1) * ow * 3];
        for (o, a) in orow.chunks_exact_mut(3).zip(acc.chunks_exact(3)) {
            o[0] = ((a[0] + area / 2) / area) as u8;
            o[1] = ((a[1] + area / 2) / area) as u8;
            o[2] = ((a[2] + area / 2) / area) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gray_2x2_block_average() {
        // 4×4 → 2×2，每块 4 个像素取均值（含四舍五入）
        let src: Vec<u8> = (0..16).map(|i| i as u8).collect();
        let got = box_gray(&src, 4, 4, 2);
        // 块 (0,0)：0,1,4,5 均值 2.5 → 3；块 (0,1)：2,3,6,7 均值 4.5 → 5
        assert_eq!(got, vec![3, 5, 11, 13]);
    }

    #[test]
    fn gray_rounds_instead_of_truncating() {
        let src = vec![1u8, 1, 1, 2]; // 均值 1.25 → 1
        assert_eq!(box_gray(&src, 2, 2, 2), vec![1]);
        let src = vec![2u8, 2, 2, 3]; // 均值 2.25 → 2
        assert_eq!(box_gray(&src, 2, 2, 2), vec![2]);
        let src = vec![2u8, 3, 3, 3]; // 均值 2.75 → 3
        assert_eq!(box_gray(&src, 2, 2, 2), vec![3]);
    }

    #[test]
    fn rgb_averages_each_channel_separately() {
        // 2×2 像素，每像素 3 通道；R 通道 10,20,30,40 → 25
        let src = vec![
            10, 0, 200, 20, 0, 200, // 行 0
            30, 0, 200, 40, 0, 200, // 行 1
        ];
        assert_eq!(box_rgb(&src, 2, 2, 2), vec![25, 0, 200]);
    }

    #[test]
    fn non_divisible_dims_drop_the_remainder() {
        // 5×3、factor 2 → 2×1：只看左上 4×2
        let src = vec![9u8; 15];
        assert_eq!(box_gray(&src, 5, 3, 2), vec![9, 9]);
        assert_eq!(out_dims(5, 3, 2), Some((2, 1)));
    }

    #[test]
    fn auto_factor_leaves_1080p_alone_and_bins_4k() {
        // 默认（--scale 1）= 自动：1080p 不缩，4K 缩 2×2
        assert_eq!(effective_factor(1920, 1080, 1), 1);
        assert_eq!(effective_factor(3840, 2160, 1), 2);
        // 更小的合成源同样不缩
        assert_eq!(effective_factor(960, 540, 1), 1);
        assert_eq!(effective_factor(640, 480, 1), 1);
        // 显式倍率永远优先：4K 也可以被指定成 4 或 8
        assert_eq!(effective_factor(3840, 2160, 2), 2);
        assert_eq!(effective_factor(3840, 2160, 4), 4);
        assert_eq!(effective_factor(1920, 1080, 4), 4);
        // 阈值两侧（按像素总数，与宽高比无关）
        assert_eq!(effective_factor(2000, 2000, 1), 1, "4.0 Mpx 恰好不缩");
        assert_eq!(effective_factor(2001, 2000, 1), 2, "超过 4.0 Mpx 才缩");
        // 0 维不会算出奇怪倍率
        assert_eq!(effective_factor(0, 0, 1), 1);
        assert_eq!(effective_factor(0, 5000, 1), 1);
    }

    #[test]
    fn too_small_or_identity_returns_none() {
        assert_eq!(out_dims(4, 4, 1), None);
        assert_eq!(out_dims(4, 4, 0), None);
        assert_eq!(out_dims(3, 3, 4), None, "缩到 0 维时应退回原图");
        assert_eq!(out_dims(4, 1, 2), None, "高只有 1 时不能缩");
        assert_eq!(out_dims(3840, 2160, 2), Some((1920, 1080)));
        assert_eq!(out_dims(3840, 2160, 4), Some((960, 540)));
    }
}
