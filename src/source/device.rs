//! 设备源：从驱动的收帧缓冲节点取帧。所有驱动细节都在 [`crate::driver`]，
//! 本模块只把它适配成 [`FrameSource`]。

use super::{Frame, FrameSource, SourceError, MAX_FRAME_BYTES};
use crate::driver::{FrameDevice, SlotError};
use crate::r10f;
use std::path::Path;

pub struct DeviceSource {
    dev: FrameDevice,
    /// 已消费到的帧序号，交给 ANLOGIC_IOCFRAMEWAIT 判「有没有更新的帧」
    last_seq: u32,
    /// 等帧超时（毫秒）
    timeout_ms: u32,
    /// 一槽原始内容（R10F 或旧 8 bit 像素），长度 = 驱动报的 slot_bytes
    slot: Vec<u8>,
    /// 因槽头不一致（写完前被读到等）跳过的帧数，只用于告警
    skipped: u64,
    /// R10F 头 0x28 只打印一次；当前 RTL 写的是占位值，不是实测相位
    warned_phase: bool,
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
        let slot = vec![0u8; bytes];
        // ioctl 报的 seq/count：0 表示 FPGA 还没往这块缓冲写过帧（空转是正常状态）
        let i = dev.info();
        eprintln!(
            "收帧缓冲就绪：{}x{}，{} 槽，槽内容 {} 字节（R10F 时是 128B 头 + packed RAW10）；ioctl 报的 seq={} count={}",
            i.width, i.height, i.slots, i.slot_bytes, i.seq, i.count
        );
        Ok(DeviceSource {
            dev,
            // 0 = 缓冲里已经有帧就先取最新的那帧，不干等下一帧
            last_seq: 0,
            timeout_ms,
            slot,
            skipped: 0,
            warned_phase: false,
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
            match self.dev.read_slot(w.slot, w.seq, &mut self.slot) {
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
                    let content = &self.slot[..read.copied];
                    match r10f::parse(content) {
                        Ok(Some(f)) => {
                            let info = self.dev.info();
                            if f.width != info.width as usize || f.height != info.height as usize {
                                eprintln!(
                                    "!! R10F 几何 {}x{} 与驱动报的 {}x{} 不一致，跳过这一帧",
                                    f.width, f.height, info.width, info.height
                                );
                                continue;
                            }
                            if !self.warned_phase {
                                self.warned_phase = true;
                                eprintln!(
                                    "R10F：{}x{} stride={} camera_id={}，头 0x28 声明 phase={}（当前 RTL 为占位值，不可信；当前安装 A/B 均 RGGB）",
                                    f.width, f.height, f.stride, f.camera_id, f.header_phase
                                );
                            }
                            return Ok(Frame::new(f.width as u16, f.height as u16, f.raw)
                                .with_camera(f.camera_id));
                        }
                        Ok(None) => {
                            // 旧 8 bit 灰度槽：整槽就是像素，按驱动几何取前 width*height 字节
                            let info = self.dev.info();
                            let n = info.width as usize * info.height as usize;
                            if content.len() < n {
                                eprintln!(
                                    "!! 旧 8bit 槽只有 {} 字节，不够 {}x{}，跳过这一帧",
                                    content.len(),
                                    info.width,
                                    info.height
                                );
                                continue;
                            }
                            return Ok(Frame::new(
                                info.width as u16,
                                info.height as u16,
                                content[..n].to_vec(),
                            ));
                        }
                        Err(why) => {
                            eprintln!("!! R10F 解析失败：{why}（跳过这一帧，不猜）");
                            continue;
                        }
                    }
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
