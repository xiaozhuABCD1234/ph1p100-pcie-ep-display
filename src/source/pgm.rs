//! PGM 文件源：读一张 P5（二进制）或 P2（ASCII）灰度图，之后每帧返回同一张。
//!
//! 只支持单通道、maxval ≤ 255（`maxval` 小于 255 时按比例拉到 0..255）。
//! 文件在构造时一次性读进内存，取帧不再碰磁盘。

use super::{Frame, FrameSource, SourceError, MAX_FRAME_BYTES};
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

/// PGM 文件读取上限：像素上限 ×4（ASCII P2 每个像素最多 4 字节）再加 1 MiB
/// 头/注释余量。先按上限读，不会把整个（可能很大的）文件吞进内存。
const MAX_PGM_FILE_BYTES: u64 = (MAX_FRAME_BYTES as u64) * 4 + (1 << 20);

/// 读文件，最多读 `limit` 字节；超出就报错
fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let f = File::open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("读 PGM {}：{e}", path.display())))?;
    let mut buf = Vec::new();
    f.take(limit + 1)
        .read_to_end(&mut buf)
        .map_err(|e| io::Error::new(e.kind(), format!("读 PGM {}：{e}", path.display())))?;
    if buf.len() as u64 > limit {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("PGM {} 超过 {} 字节的读取上限", path.display(), limit),
        ));
    }
    Ok(buf)
}

pub struct PgmFile {
    path: PathBuf,
    width: u16,
    height: u16,
    gray: Vec<u8>,
}

impl PgmFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        let raw = read_bounded(path, MAX_PGM_FILE_BYTES)?;
        let (width, height, gray) = parse_pgm(&raw).map_err(|m| {
            io::Error::new(ErrorKind::InvalidData, format!("{}：{m}", path.display()))
        })?;
        Ok(PgmFile {
            path: path.to_path_buf(),
            width,
            height,
            gray,
        })
    }
}

impl FrameSource for PgmFile {
    fn next_frame(&mut self) -> Result<Frame, SourceError> {
        // 静态文件源：每帧返回同一张图（推流侧按 fps 重复推送，画面是静止的）
        Ok(Frame::new(self.width, self.height, self.gray.clone()))
    }

    fn describe(&self) -> String {
        format!(
            "PGM 文件源 {}（{}x{} 灰度，静态）",
            self.path.display(),
            self.width,
            self.height
        )
    }
}

/// 解析 P5/P2 灰度 PGM，返回 (宽, 高, 像素)。
fn parse_pgm(raw: &[u8]) -> Result<(u16, u16, Vec<u8>), String> {
    let mut p = 0usize;

    // 词法：跳过空白与 '#' 到行尾的注释
    let token = |p: &mut usize| -> Option<String> {
        loop {
            while *p < raw.len() && raw[*p].is_ascii_whitespace() {
                *p += 1;
            }
            if *p < raw.len() && raw[*p] == b'#' {
                while *p < raw.len() && raw[*p] != b'\n' {
                    *p += 1;
                }
                continue;
            }
            break;
        }
        let start = *p;
        while *p < raw.len() && !raw[*p].is_ascii_whitespace() {
            *p += 1;
        }
        if *p == start {
            None
        } else {
            Some(String::from_utf8_lossy(&raw[start..*p]).into_owned())
        }
    };

    let magic = token(&mut p).ok_or("空文件，没有魔数")?;
    let ascii = match magic.as_str() {
        "P5" => false,
        "P2" => true,
        other => return Err(format!("魔数 {other} 不是灰度 PGM（只支持 P5/P2）")),
    };
    let num = |t: Option<String>, what: &str| -> Result<u32, String> {
        let t = t.ok_or_else(|| format!("少了{what}"))?;
        t.parse::<u32>()
            .map_err(|_| format!("{what} '{t}' 不是数字"))
    };
    let width = num(token(&mut p), "宽度")?;
    let height = num(token(&mut p), "高度")?;
    let maxval = num(token(&mut p), "maxval")?;

    if width == 0 || height == 0 || width > u16::MAX as u32 || height > u16::MAX as u32 {
        return Err(format!("尺寸 {width}x{height} 越界（1..=65535）"));
    }
    if maxval == 0 || maxval > 255 {
        return Err(format!("maxval {maxval} 不支持（本程序只认 1..=255）"));
    }
    // 尺寸上限闸：在分配之前算乘积，别只校验单个维度（65535×65535 要 4 GiB）
    let npix64 = width as u64 * height as u64;
    if npix64 > MAX_FRAME_BYTES as u64 {
        return Err(format!(
            "尺寸 {width}x{height} = {npix64} 字节超过本程序上限 {} 字节/帧",
            MAX_FRAME_BYTES
        ));
    }
    let npix = npix64 as usize;

    if ascii {
        let mut gray = Vec::with_capacity(npix);
        for _ in 0..npix {
            let v = num(token(&mut p), "像素")?;
            if v > maxval {
                return Err(format!("像素值 {v} 超过 maxval {maxval}"));
            }
            gray.push(scale(v, maxval));
        }
        Ok((width as u16, height as u16, gray))
    } else {
        // P5：maxval 之后恰好一个空白（容忍 CRLF），随后就是裸像素
        if p >= raw.len() || !raw[p].is_ascii_whitespace() {
            return Err("maxval 后面少了空白字符".into());
        }
        p += if raw[p] == b'\r' && raw.get(p + 1) == Some(&b'\n') {
            2
        } else {
            1
        };
        // 先校验长度再分配，畸形头不会先要到 64 MiB 再报错
        if raw.len() - p < npix {
            return Err(format!("像素数据只有 {} 字节，需要 {npix}", raw.len() - p));
        }
        let mut gray = Vec::with_capacity(npix);
        for &b in &raw[p..p + npix] {
            let v = b as u32;
            if v > maxval {
                return Err(format!("像素值 {v} 超过 maxval {maxval}"));
            }
            gray.push(scale(v, maxval));
        }
        Ok((width as u16, height as u16, gray))
    }
}

/// maxval < 255 时按比例拉到满量程，避免整张图偏暗
fn scale(v: u32, maxval: u32) -> u8 {
    if maxval == 255 {
        v as u8
    } else {
        ((v * 255 + maxval / 2) / maxval) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p5_roundtrip() {
        let mut raw = b"P5\n# comment line\n3 2\n255\n".to_vec();
        raw.extend_from_slice(&[10, 20, 30, 40, 50, 60]);
        let (w, h, g) = parse_pgm(&raw).unwrap();
        assert_eq!((w, h), (3, 2));
        assert_eq!(g, vec![10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn p2_scaled() {
        let raw = b"P2 2 1 15\n0 15\n";
        let (w, h, g) = parse_pgm(raw).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(g, vec![0, 255]);
    }

    #[test]
    fn rejects_color_and_short_data() {
        assert!(parse_pgm(b"P6\n1 1\n255\n").is_err());
        assert!(parse_pgm(b"P5\n4 4\n255\n\x01\x02").is_err());
    }

    #[test]
    fn rejects_oversized_dimensions() {
        // 65535×65535 = 4294836225 字节，远超 64 MiB 上限；要干净报错，不能 abort
        let e = parse_pgm(b"P5\n65535 65535\n255\n").unwrap_err();
        assert!(e.contains("上限"), "{e}");
    }
}
