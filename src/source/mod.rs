//! 帧源抽象：一帧 = 8 位灰度像素 + 宽高。
//!
//! 三种实现：合成源（`synthetic`）、PGM 文件源（`pgm`）、驱动设备源（`device`，
//! 内部用 `crate::driver`）。

pub mod device;
pub mod pgm;
pub mod synthetic;

use std::io;

/// 一帧单字节灰度图。`gray.len()` 必须等于 `width * height`。
pub struct Frame {
    pub width: u16,
    pub height: u16,
    pub gray: Vec<u8>,
}

impl Frame {
    pub fn new(width: u16, height: u16, gray: Vec<u8>) -> Self {
        debug_assert_eq!(gray.len(), width as usize * height as usize);
        Frame {
            width,
            height,
            gray,
        }
    }
}

/// 取帧失败的原因。`Timeout`/`NoSource` 是「暂时没帧」，取帧循环可以继续等；
/// `Io` 视为设备真的坏了。
#[derive(Debug)]
pub enum SourceError {
    /// 等帧超时（FPGA 还没写新帧，属正常空转）
    Timeout(String),
    /// 系统调用/设备错误
    Io(io::Error),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Timeout(s) => write!(f, "{s}"),
            SourceError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for SourceError {
    fn from(e: io::Error) -> Self {
        SourceError::Io(e)
    }
}

/// 帧源。`next_frame` 阻塞到有一帧可用（设备源会在内部等帧就绪）。
pub trait FrameSource: Send {
    fn next_frame(&mut self) -> Result<Frame, SourceError>;
    /// 一行式描述，给启动日志用
    fn describe(&self) -> String;
    /// 周期性状态里的补充信息（设备源报驱动序号等），无则 None
    fn status(&self) -> Option<String> {
        None
    }
}
