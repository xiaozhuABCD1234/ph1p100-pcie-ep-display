//! R10F v1 非压缩帧解析 + packed RAW10 解包。
//!
//! 收帧缓冲里的一槽内容（从槽数据偏移起）可能是：
//!   * R10F v1：128 B 头 + packed RAW10（4 像素/5 字节）+ 4 KiB 对齐零尾；
//!   * 旧 8 bit 灰度：整槽就是像素。
//!
//! 本模块只做第一类。严格校验 magic/version/header/format/codec/几何/长度，
//! 然后把 packed RAW10 解成 8 bit Bayer。8 bit 取每个 10 bit 采样的高 8 位
//! （与 `frame_grab` 的 PGM 预览完全一致，即每组前 4 个字节）。
//!
//! 相位不在这里猜：R10F 头 0x28 当前是 RTL 的占位值（3=GBRG），板测已知
//! 与交付像素不符。真实相位由 `--bayer` 指定；当前安装 A/B 均为 **RGGB**。
//! 头里的 phase 只作为告警信息返回给上层。

/// R10F v1 固定头长。
pub const HDR_BYTES: usize = 128;

/// 一帧 R10F 解析结果。`raw` 是 `width*height` 字节的 8 bit Bayer。
pub struct R10fFrame {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    /// 头 0x0A 的 camera_id（0=A，1=B），用于 `--bayer auto` 选相位。
    pub camera_id: u8,
    /// 头 0x28 声明的相位（0=RGGB,1=BGGR,2=GRBG,3=GBRG）。
    /// 当前 RTL 写的是占位值，不能直接拿去去马赛克。
    pub header_phase: u8,
    pub raw: Vec<u8>,
}

fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// 解析槽内容。不是 R10F 时返回 `Ok(None)`（旧 8 bit 路径）；是 R10F 但
/// 头/几何/长度不合法时返回 `Err`，调用方应拒绝这一帧，不能猜。
pub fn parse(content: &[u8]) -> Result<Option<R10fFrame>, String> {
    if content.len() < HDR_BYTES || &content[0..4] != b"R10F" {
        return Ok(None);
    }
    let version_major = content[0x04];
    let version_minor = content[0x06];
    let header_bytes = le16(content, 0x08) as usize;
    let format_id = content[0x0B];
    let codec_id = content[0x0C];
    let width = le16(content, 0x1C) as usize;
    let height = le16(content, 0x1E) as usize;
    let stride = le32(content, 0x20) as usize;
    let camera_id = content[0x0A];
    let header_phase = content[0x28];
    let valid_bits = content[0x2A];
    let packing = content[0x2B];
    let raw_bytes = le32(content, 0x2C) as usize;
    let payload_bytes = le32(content, 0x30) as usize;

    if version_major != 1 || version_minor != 0 {
        return Err(format!("R10F 版本 {version_major}.{version_minor} != 1.0"));
    }
    if header_bytes != HDR_BYTES {
        return Err(format!("R10F header_bytes={header_bytes} != {HDR_BYTES}"));
    }
    if format_id != 0 || codec_id != 0 {
        return Err(format!(
            "R10F format_id={format_id} codec_id={codec_id}，本工具只支持非压缩 RAW"
        ));
    }
    if valid_bits != 10 || packing != 0 {
        return Err(format!(
            "R10F valid_bits={valid_bits} packing={packing}，只支持 10bit packed"
        ));
    }
    if width == 0 || height == 0 || width % 4 != 0 {
        return Err(format!(
            "R10F 几何 {width}x{height} 非法（width 须为 4 的倍数）"
        ));
    }
    let row_bytes = (width / 4).checked_mul(5).ok_or("R10F 行字节数溢出")?;
    if stride < row_bytes {
        return Err(format!(
            "R10F stride={stride} 装不下一行（需要 {row_bytes}）"
        ));
    }
    if raw_bytes != payload_bytes {
        return Err(format!(
            "R10F raw_bytes={raw_bytes} != payload_bytes={payload_bytes}（本工具只支持非压缩）"
        ));
    }
    let need_payload = stride
        .checked_mul(height - 1)
        .and_then(|v| v.checked_add(row_bytes))
        .ok_or("R10F 载荷长度溢出")?;
    if payload_bytes < need_payload {
        return Err(format!(
            "R10F payload={payload_bytes} 不够 {height} 行 × stride {stride}"
        ));
    }
    let need_total = HDR_BYTES
        .checked_add(payload_bytes)
        .ok_or("R10F 总长度溢出")?;
    if content.len() < need_total {
        return Err(format!(
            "R10F 内容 {} 字节，头+payload 需要 {} 字节",
            content.len(),
            need_total
        ));
    }
    let npix = width.checked_mul(height).ok_or("R10F 像素数溢出")?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(npix)
        .map_err(|e| format!("分配 {npix} 字节 RAW 失败：{e}"))?;
    raw.resize(npix, 0u8);
    let payload = &content[HDR_BYTES..HDR_BYTES + payload_bytes];
    for y in 0..height {
        let src = &payload[y * stride..y * stride + row_bytes];
        let dst = &mut raw[y * width..(y + 1) * width];
        for (g, d) in src.chunks_exact(5).zip(dst.chunks_exact_mut(4)) {
            // 高 8 位就是每组前 4 字节（bits[9:2]）。
            d[0] = g[0];
            d[1] = g[1];
            d[2] = g[2];
            d[3] = g[3];
        }
    }
    Ok(Some(R10fFrame {
        width,
        height,
        stride,
        camera_id,
        header_phase,
        raw,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_r10f(width: usize, height: usize) -> Vec<u8> {
        let row_bytes = width / 4 * 5;
        let payload_bytes = row_bytes * height;
        let total = HDR_BYTES + payload_bytes;
        let mut b = vec![0u8; total];
        b[0..4].copy_from_slice(b"R10F");
        b[0x04] = 1;
        b[0x06] = 0;
        b[0x08..0x0A].copy_from_slice(&(HDR_BYTES as u16).to_le_bytes());
        b[0x0A] = 1;
        b[0x0B] = 0;
        b[0x0C] = 0;
        b[0x1C..0x1E].copy_from_slice(&(width as u16).to_le_bytes());
        b[0x1E..0x20].copy_from_slice(&(height as u16).to_le_bytes());
        b[0x20..0x24].copy_from_slice(&(row_bytes as u32).to_le_bytes());
        b[0x28] = 3;
        b[0x2A] = 10;
        b[0x2B] = 0;
        b[0x2C..0x30].copy_from_slice(&(payload_bytes as u32).to_le_bytes());
        b[0x30..0x34].copy_from_slice(&(payload_bytes as u32).to_le_bytes());
        // 每组 5 字节：前 4 字节是高 8 位，最后 1 字节是低 2 位；高 8 位按像素序号填。
        for y in 0..height {
            for (i, g) in b[HDR_BYTES + y * row_bytes..HDR_BYTES + (y + 1) * row_bytes]
                .chunks_exact_mut(5)
                .enumerate()
            {
                for k in 0..4 {
                    g[k] = ((y * width + i * 4 + k) & 0xFF) as u8;
                }
                g[4] = 0;
            }
        }
        b
    }

    #[test]
    fn parses_and_unpacks_high_eight_bits() {
        let b = tiny_r10f(8, 2);
        let f = parse(&b).unwrap().unwrap();
        assert_eq!((f.width, f.height, f.stride), (8, 2, 10));
        assert_eq!(f.header_phase, 3);
        assert_eq!(f.camera_id, 1);
        assert_eq!(f.raw.len(), 16);
        for (i, &v) in f.raw.iter().enumerate() {
            assert_eq!(v, (i & 0xFF) as u8, "pixel {i}");
        }
    }

    #[test]
    fn rejects_truncated_and_bad_geometry() {
        let mut b = tiny_r10f(8, 2);
        b.truncate(HDR_BYTES + 9);
        assert!(parse(&b).is_err());
        let mut b = tiny_r10f(8, 2);
        b[0x1C] = 6; // width 不是 4 的倍数
        assert!(parse(&b).is_err());
    }

    #[test]
    fn non_r10f_is_none() {
        assert!(parse(b"not an R10F frame at all").unwrap().is_none());
    }

    /// 有真实 dump 时做一次端到端校验（不设环境变量就跳过，保持仓库自包含）。
    /// 运行：R10F_DUMP=../log/dual_fps_20261028/fa.dma cargo test parses_real_dump
    #[test]
    fn parses_real_dump_when_env_set() {
        let Ok(path) = std::env::var("R10F_DUMP") else {
            return;
        };
        let b = std::fs::read(&path).expect("读 R10F_DUMP 失败");
        let f = parse(&b).expect("真实 R10F 解析失败").expect("不是 R10F");
        assert_eq!((f.width, f.height, f.stride), (1920, 1080, 2400));
        assert_eq!(f.raw.len(), 1920 * 1080);
        // 真实帧第一行不是全 0，也不是全 255（防止解包读错段）
        let row = &f.raw[..1920];
        assert!(row.iter().any(|&v| v != 0));
        assert!(row.iter().any(|&v| v != 255));
    }
}
