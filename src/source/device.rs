//! 设备源：从驱动的收帧缓冲节点取帧。所有驱动细节都在 [`crate::driver`]，
//! 本模块只把它适配成 [`FrameSource`]。

use super::{Frame, FrameSource, SourceError, MAX_FRAME_BYTES};
use crate::driver::{FrameDevice, SlotError};
use std::path::Path;

pub struct DeviceSource {
    dev: FrameDevice,
    /// 已消费到的帧序号，交给 ANLOGIC_IOCFRAMEWAIT 判「有没有更新的帧」
    last_seq: u32,
    /// 等帧超时（毫秒）
    timeout_ms: u32,
    gray: Vec<u8>,
    /// 因槽头不一致（写完前被读到等）跳过的帧数，只用于告警
    skipped: u64,
}

impl DeviceSource {
    pub fn open(path: &Path, timeout_ms: u32) -> std::io::Result<Self> {
        let dev = FrameDevice::open(path)?;
        // 驱动报的几何先过上限闸，再分配：畸形/异常的 width×height 不能把进程顶到 abort
        let bytes = dev.info().frame_bytes();
        if bytes > MAX_FRAME_BYTES {
            let i = dev.info();
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "驱动报的几何 {}x{} = {} 字节/帧，超过本程序上限 {} 字节",
                    i.width, i.height, bytes, MAX_FRAME_BYTES
                ),
            ));
        }
        let gray = vec![0u8; bytes];
        // ioctl 报的 seq/count：0 表示 FPGA 还没往这块缓冲写过帧（空转是正常状态）
        let i = dev.info();
        eprintln!(
            "收帧缓冲就绪：{}x{} 灰度，{} 槽；ioctl 报的 seq={} count={}",
            i.width, i.height, i.slots, i.seq, i.count
        );
        Ok(DeviceSource {
            dev,
            // 0 = 缓冲里已经有帧就先取最新的那帧，不干等下一帧
            last_seq: 0,
            timeout_ms,
            gray,
            skipped: 0,
        })
    }
}

impl FrameSource for DeviceSource {
    fn next_frame(&mut self) -> Result<Frame, SourceError> {
        // 一槽不一致（FPGA 还在写）就跳过重取，最多试到等到新帧为止
        for _ in 0..4 {
            let w = self.dev.wait_frame(self.last_seq, self.timeout_ms)?;
            if !w.ready {
                return Err(SourceError::Timeout(format!(
                    "等帧超时 {}ms：seq={}（last_seq={}）count={} —— FPGA 还没写新帧",
                    self.timeout_ms, w.seq, self.last_seq, w.count
                )));
            }
            match self.dev.read_slot(w.slot, w.seq, &mut self.gray) {
                Ok(read) => {
                    self.last_seq = w.seq;
                    if read.copied < self.info_bytes() {
                        // 槽头 len 小于一帧：像素已补 0，如实告警但不假装是好帧
                        eprintln!(
                            "警告：第 {} 帧（累计 {}）槽头报 {} 字节，只取到 {}/{} 字节（缺的补 0）",
                            read.seq,
                            read.count,
                            read.len,
                            read.copied,
                            self.info_bytes()
                        );
                    }
                    let info = self.dev.info();
                    return Ok(Frame::new(
                        info.width as u16,
                        info.height as u16,
                        self.gray.clone(),
                    ));
                }
                Err(SlotError::Inconsistent(why)) => {
                    self.skipped += 1;
                    eprintln!("跳过一帧（槽 {}，seq {}）：{}", w.slot, w.seq, why);
                    // last_seq 不推进，下一轮继续要这一帧
                    continue;
                }
                Err(SlotError::Io(e)) => return Err(SourceError::Io(e)),
            }
        }
        Err(SourceError::Timeout(
            "连续 4 次取到的槽都不一致（帧不一致就跳过，不推可疑数据）".into(),
        ))
    }

    fn describe(&self) -> String {
        format!("设备源 —— {}", self.dev.describe())
    }

    fn status(&self) -> Option<String> {
        Some(format!(
            "驱动 seq={} count={}，因槽头不一致跳过 {} 帧",
            self.dev.mailbox_seq(),
            self.dev.mailbox_count(),
            self.skipped
        ))
    }
}

impl DeviceSource {
    fn info_bytes(&self) -> usize {
        self.dev.info().frame_bytes()
    }
}
