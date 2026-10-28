//! R10F 文件源：离线回放一帧 R10F（.dma）内容，用于颜色/去马赛克回归。
//!
//! 文件从 R10F 魔数开始，即主机槽内 hdr_bytes 偏移起的整段；不是带 16 B 槽头的
//! 整槽。解析交给 crate::r10f，与设备源走同一条代码路径。

use super::{Frame, FrameSource, SourceError};
use crate::r10f;
use std::path::{Path, PathBuf};

pub struct R10fFile {
    path: PathBuf,
    width: u16,
    height: u16,
    camera_id: u8,
    header_phase: u8,
    raw: Vec<u8>,
}

impl R10fFile {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let data = std::fs::read(path)?;
        let f = match r10f::parse(&data) {
            Ok(Some(f)) => f,
            Ok(None) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "文件不是 R10F（前 4 字节不是 R10F）",
                ))
            }
            Err(e) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("R10F 解析失败：{e}"),
                ))
            }
        };
        if f.width > u16::MAX as usize || f.height > u16::MAX as usize {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("R10F 几何 {}x{} 超出 u16", f.width, f.height),
            ));
        }
        Ok(R10fFile {
            path: path.to_path_buf(),
            width: f.width as u16,
            height: f.height as u16,
            camera_id: f.camera_id,
            header_phase: f.header_phase,
            raw: f.raw,
        })
    }
}

impl FrameSource for R10fFile {
    fn next_frame(&mut self) -> Result<Frame, SourceError> {
        Ok(Frame::new(self.width, self.height, self.raw.clone()).with_camera(self.camera_id))
    }

    fn describe(&self) -> String {
        format!(
            "R10F 文件源 —— {}（{}x{}，camera_id={}，头 0x28 phase={} 为占位值）",
            self.path.display(),
            self.width,
            self.height,
            self.camera_id,
            self.header_phase
        )
    }
}
