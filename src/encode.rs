//! JPEG 编码。用纯 Rust 的 `jpeg-encoder`：`ColorType::Luma` 走单分量路径
//! （灰度 JPEG，SOF 里只有 1 个分量），`ColorType::Rgb` 走三分量路径（彩色 JPEG）。
//!
//! 彩色路径用编码器的默认色度抽样（质量 < 90 时为 4:2:0）。对去马赛克出来的图
//! 这一步正好顺手压掉一部分马赛克的假彩色，也不额外引入依赖。

use jpeg_encoder::{ColorType, Encoder, EncodingError};

/// 把单字节灰度像素编成灰度 JPEG。`gray.len()` 必须 ≥ width×height。
pub fn encode_gray(
    gray: &[u8],
    width: u16,
    height: u16,
    quality: u8,
) -> Result<Vec<u8>, EncodingError> {
    // 灰度图经验压缩比 ~1:4，按 1/2 预分配省掉中途扩容
    let mut out = Vec::with_capacity(gray.len() / 2 + 1024);
    Encoder::new(&mut out, quality).encode(gray, width, height, ColorType::Luma)?;
    Ok(out)
}

/// 把 RGB8（每像素 3 字节）编成三分量彩色 JPEG。
/// `rgb.len()` 必须 ≥ width×height×3。
pub fn encode_rgb(
    rgb: &[u8],
    width: u16,
    height: u16,
    quality: u8,
) -> Result<Vec<u8>, EncodingError> {
    // 彩色图三通道合起来经验压缩比 ~1:8，按 1/4 预分配
    let mut out = Vec::with_capacity(rgb.len() / 4 + 1024);
    Encoder::new(&mut out, quality).encode(rgb, width, height, ColorType::Rgb)?;
    Ok(out)
}

/// 从 JPEG 字节里读出 SOF 的「分量的个数」：1 = 灰度，3 = YCbCr。自检用。
pub fn jpeg_component_count(jpeg: &[u8]) -> Option<u8> {
    let mut i = 2usize; // 跳过 SOI
    while i + 3 < jpeg.len() {
        if jpeg[i] != 0xFF {
            return None;
        }
        let marker = jpeg[i + 1];
        if marker == 0xFF {
            i += 1; // 填充字节
            continue;
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2; // 无长度字段的标记
            continue;
        }
        let len = ((jpeg[i + 2] as usize) << 8) | jpeg[i + 3] as usize;
        // SOF0..SOF15，排除 DHT(C4)/JPG(C8)/DAC(CC)
        let is_sof =
            (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC;
        if is_sof {
            // SOF: 长度(2) 精度(1) 高(2) 宽(2) 分量个数(1)
            return jpeg.get(i + 9).copied();
        }
        if marker == 0xDA {
            return None; // 到扫描数据也没见到 SOF
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_single_component_grayscale() {
        // 平滑渐变图：既验证头，也验证确实在压缩
        let (w, h) = (320u16, 240u16);
        let gray: Vec<u8> = (0..(w as usize * h as usize))
            .map(|i| ((i / 320 + i % 320) % 256) as u8)
            .collect();
        let jpeg = encode_gray(&gray, w, h, 80).unwrap();

        // SOI 开头
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        // 单分量 = 灰度（3 就成彩色了）
        assert_eq!(jpeg_component_count(&jpeg), Some(1), "输出必须是灰度 JPEG");
        // 平滑图应该被压到远小于原始数据
        assert!(jpeg.len() < gray.len(), "{} >= {}", jpeg.len(), gray.len());
    }

    #[test]
    fn encodes_three_component_color() {
        let (w, h) = (64u16, 48u16);
        let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
        for y in 0..h as usize {
            for x in 0..w as usize {
                rgb.push((x * 3 % 256) as u8);
                rgb.push((y * 5 % 256) as u8);
                rgb.push(((x + y) % 256) as u8);
            }
        }
        let jpeg = encode_rgb(&rgb, w, h, 80).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        assert_eq!(
            jpeg_component_count(&jpeg),
            Some(3),
            "输出必须是三分量彩色 JPEG"
        );
    }
}
