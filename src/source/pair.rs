//! 双路设备源：一次 `ARM_PAIR` 取 A/B 两槽，交给 display 的推流线程。
//!
//! 与单路 `DeviceSource` 的区别：
//!   * 打开控制节点，直接写 BAR0 的 pair 寄存器（目标/门铃/ACK）；
//!   * `ARM_PAIR` 后等 A/B 两个门铃，两槽都过 R10F strict；
//!   * `ACK_PAIR` 后补一拍 `CANCEL_PAIR` 兜底，避免门铃早于 HELD 造成残留。
//!
//! 帧几何/R10F 校验复用 `crate::r10f`，不在这里另写一套。

use std::ffi::CString;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::{Frame, SourceError};
use crate::driver::{AnlogicIocBar, FrameBufferInfo, FrameDevice};
use crate::r10f;

const BAR_CTRL: u32 = 0x44;
const BAR_TX_CAP: u32 = 0x100;
const BAR_TX_SESSION: u32 = 0x104;
const BAR_TX_CMD: u32 = 0x108;
const BAR_TX_ACK_SEQ: u32 = 0x10C;
const BAR_TX_ACK_SESSION: u32 = 0x110;
const BAR_TX_STATUS: u32 = 0x114;
const BAR_PUSH_DST_LO: u32 = 0xB0;
const BAR_PUSH_DST_HI: u32 = 0xB4;
const BAR_PUSH_DB_LO: u32 = 0xB8;
const BAR_PUSH_DB_HI: u32 = 0xBC;
const BAR_PUSH_DONE: u32 = 0xC0;
const BAR_PAIR_ACK_SEQ: u32 = 0x220;
const BAR_PAIR_DST_LO: u32 = 0x224;
const BAR_PAIR_DST_HI: u32 = 0x228;
const BAR_PAIR_DB_LO: u32 = 0x22C;
const BAR_PAIR_DB_HI: u32 = 0x230;
const BAR_PAIR_DONE: u32 = 0x210;

const CTRL_PAIR: u32 = 0x261; // enable + ddr_en + push_en + pair_en
const TX_CMD_ARM_PAIR: u32 = 8;
const TX_CMD_ACK_PAIR: u32 = 16;
const TX_CMD_CANCEL_PAIR: u32 = 32;
const PAIR_SESSION: u32 = 0xA1B2_C3D4;
const DBELL_A_OFF: usize = 32;
const DBELL_B_OFF: usize = 36;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> u64 {
    ((dir << IOC_DIRSHIFT) as u64)
        | ((ty as u64) << IOC_TYPESHIFT)
        | ((nr as u64) << IOC_NRSHIFT)
        | ((size as u64) << IOC_SIZESHIFT)
}

fn ctrl_open(path: &Path) -> io::Result<OwnedFd> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "控制节点路径里有 NUL"))?;
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ioctl_bar(fd: RawFd, off: u32, wdata: Option<u32>) -> io::Result<u32> {
    let mut b = AnlogicIocBar {
        bar_id: 0,
        bar_type: 0,
        bar_offaddr: off,
        wdata: wdata.unwrap_or(0),
        rdata: 0,
    };
    let req = if wdata.is_some() {
        ioc(IOC_WRITE, b'x', 4, size_of::<AnlogicIocBar>())
    } else {
        ioc(IOC_READ, b'x', 5, size_of::<AnlogicIocBar>())
    };
    let rc = unsafe { libc::ioctl(fd, req as libc::c_ulong, &mut b as *mut AnlogicIocBar) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(b.rdata)
}

fn wr(fd: RawFd, off: u32, v: u32) -> io::Result<()> {
    ioctl_bar(fd, off, Some(v)).map(|_| ())
}

fn wr64(fd: RawFd, lo: u32, hi: u32, v: u64) -> io::Result<()> {
    wr(fd, lo, v as u32)?;
    wr(fd, hi, (v >> 32) as u32)
}

fn rd(fd: RawFd, off: u32) -> io::Result<u32> {
    ioctl_bar(fd, off, None)
}

pub struct PairSource {
    dev: FrameDevice,
    ctrl: OwnedFd,
    timeout_ms: u32,
    raw_a: Vec<u8>,
    raw_b: Vec<u8>,
    last_pair: u32,
    last_count: u32,
}

impl PairSource {
    pub fn open(frame_path: &Path, ctrl_path: &Path, timeout_ms: u32) -> io::Result<Self> {
        let dev = FrameDevice::open(frame_path)?;
        if dev.info().slots < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("双路需要 frame_slots=2（当前 {}）", dev.info().slots),
            ));
        }
        let ctrl = ctrl_open(ctrl_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("打不开控制节点 {}：{e}", ctrl_path.display()),
            )
        })?;
        let n = dev.info().frame_bytes();
        Ok(PairSource {
            dev,
            ctrl,
            timeout_ms,
            raw_a: vec![0u8; n],
            raw_b: vec![0u8; n],
            last_pair: 0,
            last_count: 0,
        })
    }

    pub fn start(&mut self) -> io::Result<()> {
        let cfd = self.ctrl.as_raw_fd();
        let ctrl = (rd(cfd, BAR_CTRL)? | CTRL_PAIR) & !0x180;
        wr(cfd, BAR_CTRL, ctrl)?;
        let info = *self.dev.info();
        let cap = info.stride.saturating_mul(2);
        wr(cfd, BAR_TX_CAP, cap)?;
        wr(cfd, BAR_TX_SESSION, PAIR_SESSION)?;
        wr(cfd, BAR_TX_ACK_SESSION, PAIR_SESSION)?;
        let dst_a = info.phys_addr + info.hdr_bytes as u64;
        let dst_b = info.phys_addr + info.stride as u64 + info.hdr_bytes as u64;
        let db_a = info.phys_addr + info.mb_off as u64 + DBELL_A_OFF as u64;
        let db_b = info.phys_addr + info.mb_off as u64 + DBELL_B_OFF as u64;
        wr64(cfd, BAR_PUSH_DST_LO, BAR_PUSH_DST_HI, dst_a)?;
        wr64(cfd, BAR_PUSH_DB_LO, BAR_PUSH_DB_HI, db_a)?;
        wr64(cfd, BAR_PAIR_DST_LO, BAR_PAIR_DST_HI, dst_b)?;
        wr64(cfd, BAR_PAIR_DB_LO, BAR_PAIR_DB_HI, db_b)?;
        // 清掉可能残留的 held；两路都发布过完整帧后才允许 ARM。
        let _ = wr(cfd, BAR_TX_CMD, TX_CMD_CANCEL_PAIR);
        std::thread::sleep(std::time::Duration::from_millis(20));
        self.wait_ddr()
    }

    fn wait_ddr(&self) -> io::Result<()> {
        let cfd = self.ctrl.as_raw_fd();
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(self.timeout_ms.max(1000) as u64);
        while std::time::Instant::now() < deadline {
            let a = rd(cfd, 0xA8).unwrap_or(0);
            let b = rd(cfd, 0xE8).unwrap_or(0);
            if a & 1 != 0 && b & 1 != 0 {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "两路 DDR 还没发布完整帧",
        ))
    }

    fn wait_dbell(&self, off: usize, old: u32) -> Option<u32> {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(self.timeout_ms as u64);
        loop {
            let v = self.dev.mailbox_u32_field(off);
            if v != old {
                return Some(v);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    pub fn next_pair(&mut self) -> Result<(Frame, Frame), SourceError> {
        let cfd = self.ctrl.as_raw_fd();
        let old_a = self.dev.mailbox_u32_field(DBELL_A_OFF);
        let old_b = self.dev.mailbox_u32_field(DBELL_B_OFF);
        wr(cfd, BAR_TX_CMD, TX_CMD_ARM_PAIR).map_err(SourceError::Io)?;
        std::thread::sleep(std::time::Duration::from_millis(2));
        if rd(cfd, BAR_TX_STATUS).unwrap_or(0) & 0x8 != 0 {
            let _ = wr(cfd, BAR_TX_CMD, TX_CMD_CANCEL_PAIR);
            return Err(SourceError::Timeout("ARM_PAIR 被 RTL 拒绝".into()));
        }

        let (Some(new_a), Some(new_b)) = (
            self.wait_dbell(DBELL_A_OFF, old_a),
            self.wait_dbell(DBELL_B_OFF, old_b),
        ) else {
            let _ = wr(cfd, BAR_TX_CMD, TX_CMD_CANCEL_PAIR);
            return Err(SourceError::Timeout(
                "双路门铃超时（FPGA 没有推完一帧）".into(),
            ));
        };

        if let Err(e) = self.dev.read_slot_raw(0, &mut self.raw_a) {
            self.release(cfd, new_a, new_b);
            return Err(slot_err(e));
        }
        if let Err(e) = self.dev.read_slot_raw(1, &mut self.raw_b) {
            self.release(cfd, new_a, new_b);
            return Err(slot_err(e));
        }
        let fa = match decode(&self.raw_a, self.dev.info()) {
            Ok(f) => f,
            Err(why) => {
                self.release(cfd, new_a, new_b);
                return Err(SourceError::Timeout(format!("A 槽 R10F 拒绝：{why}")));
            }
        };
        let fb = match decode(&self.raw_b, self.dev.info()) {
            Ok(f) => f,
            Err(why) => {
                self.release(cfd, new_a, new_b);
                return Err(SourceError::Timeout(format!("B 槽 R10F 拒绝：{why}")));
            }
        };

        self.release(cfd, new_a, new_b);
        self.last_pair += 1;
        self.last_count = new_a.max(new_b);
        Ok((fa, fb))
    }

    fn release(&self, cfd: RawFd, fallback_a: u32, fallback_b: u32) {
        let done_a = rd(cfd, BAR_PUSH_DONE).unwrap_or(fallback_a);
        let done_b = rd(cfd, BAR_PAIR_DONE).unwrap_or(fallback_b);
        let _ = wr(cfd, BAR_TX_ACK_SEQ, done_a);
        let _ = wr(cfd, BAR_PAIR_ACK_SEQ, done_b);
        let _ = wr(cfd, BAR_TX_CMD, TX_CMD_ACK_PAIR);
        // 门铃早于 g_st→HELD 几拍；补 CANCEL 兜底，主机槽内容已拷完，不受影响。
        let _ = wr(cfd, BAR_TX_CMD, TX_CMD_CANCEL_PAIR);
        // CANCEL 已经释放源块；这里只留出状态机回 IDLE 的几拍，不再固定等 20ms。
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    pub fn describe(&self) -> String {
        format!(
            "双路设备源 —— {}，A=槽0 B=槽1，pair 目标 0x{:x}/0x{:x}",
            self.dev.describe(),
            self.dev.info().phys_addr + self.dev.info().hdr_bytes as u64,
            self.dev.info().phys_addr
                + self.dev.info().stride as u64
                + self.dev.info().hdr_bytes as u64
        )
    }

    pub fn status(&self) -> String {
        format!(
            "双路 pair={} 最后门铃={} mailbox seq={} count={}",
            self.last_pair,
            self.last_count,
            self.dev.mailbox_seq(),
            self.dev.mailbox_count()
        )
    }
}

fn slot_err(e: crate::driver::SlotError) -> SourceError {
    SourceError::Io(io::Error::other(e.to_string()))
}

fn decode(raw: &[u8], info: &FrameBufferInfo) -> Result<Frame, String> {
    let f = r10f::parse(raw)?.ok_or_else(|| "不是 R10F".to_string())?;
    if f.width != info.width as usize || f.height != info.height as usize {
        return Err(format!(
            "R10F 几何 {}x{} 与驱动报的 {}x{} 不一致",
            f.width, f.height, info.width, info.height
        ));
    }
    Ok(Frame::new(f.width as u16, f.height as u16, f.raw).with_camera(f.camera_id))
}
