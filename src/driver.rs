//! 安路 PH1P100 EP 驱动的用户态契约层 —— 与驱动打交道的一切都集中在本模块。
//!
//! 契约来源：`../driver/include/anlogic_pci_ioctl.h`（驱动与用户态共用的唯一一份 ABI
//! 头文件）。本模块只**只读地镜像**它的编号与结构体布局，不改动驱动任何文件；
//! 驱动契约若增补，只需要改这一个模块。
//!
//! 节点：`/dev/ANLOGIC-PCI<n>_frame`（收帧缓冲专用节点，不绑 BAR），只支持
//! 只读 `mmap` + 帧就绪 `ioctl`。驱动侧实现见 `anlogic_pci_chrdev.c` /
//! `anlogic_pci_frame.c`，参考用户态写法见 `driver/app/frame_grab.c`。
//!
//! 取帧时序（照抄 frame_grab.c 的顺序，不自己发明）：
//!   1. `open(O_RDONLY)`，再 `flock(LOCK_EX|LOCK_NB)` 独占（一次只允许一个取帧进程）；
//!   2. `ANLOGIC_IOCFRAMEINFO` 问缓冲的物理地址/大小/几何/序号；
//!   3. 只读 `mmap` 整块缓冲，核对 mailbox 的 magic；
//!   4. `ANLOGIC_IOCFRAMEWAIT` 轮询等一帧（本 EP 只有 Legacy 中断，这一层不走中断）；
//!   5. 核对槽头 seq/len/flags，拷像素，再复查序号是否在搬运期间绕回。
//!
//! 缓冲内布局（由驱动分配、FPGA 用 MWr 写入）：
//!   每一路内：`stride*slots` 是槽区，槽 n 在 `stride*n`，其前 16 字节是槽头；
//!   本路在整块缓冲里的起点 = `mb_off - stride*slots`（单路 = 0；A/B 两路时 B 路
//!   就是第 1 路的起点）—— mmap 映射的是整块缓冲，算槽地址要加这个起点；
//!   mailbox 在 `mb_off`（页对齐、独占一页），**偏移必须用 ioctl 报的值**，不许按常量猜。

use std::ffi::CString;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{fence, Ordering};

/* ------------------------------------------------------------------ *
 * ioctl 编号编码（asm-generic，x86_64 与 aarch64 一致）
 * ------------------------------------------------------------------ */

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> u64 {
    ((dir << IOC_DIRSHIFT) as u64)
        | ((ty as u64) << IOC_TYPESHIFT)
        | ((nr as u64) << IOC_NRSHIFT)
        | ((size as u64) << IOC_SIZESHIFT)
}

/// `ANLOGIC_IOC_MAGIC`
const ANLOGIC_IOC_MAGIC: u8 = b'x';

/* ------------------------------------------------------------------ *
 * 与头文件逐字段对应的结构体（字段名沿用 C 侧，便于对表）
 * ------------------------------------------------------------------ */

/// `struct anlogic_ioc_frame` —— `ANLOGIC_IOCFRAMEINFO` 的出入参
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AnlogicIocFrame {
    pub phys_addr: u64,
    pub size: u64,
    pub stride: u32,
    pub slot_bytes: u32,
    pub hdr_bytes: u32,
    pub slots: u32,
    pub width: u32,
    pub height: u32,
    pub seq: u32,
    pub count: u32,
    pub mb_off: u32,
}

/// `struct anlogic_ioc_frame_wait` —— `ANLOGIC_IOCFRAMEWAIT` 的出入参
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AnlogicIocFrameWait {
    pub last_seq: u32,
    pub timeout_ms: u32,
    pub seq: u32,
    pub count: u32,
    pub ready: u32,
    pub slot: u32,
}

/// `struct anlogic_frame_mailbox`（64 字节，在缓冲内 `mb_off` 处）
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameMailbox {
    pub magic: u32,
    pub seq: u32,
    pub count: u32,
    pub slot_bytes: u32,
    pub slots: u32,
    pub stride: u32,
    pub width: u32,
    pub height: u32,
    pub reserved: [u32; 8],
}

/// `struct anlogic_frame_slot_hdr`（16 字节，每个槽的前缀）
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameSlotHdr {
    pub seq: u32,
    pub len: u32,
    pub flags: u32,
    pub reserved: u32,
}

/// `ANLOGIC_FRAME_MAGIC`（小端字节序为 'A','F','R','M'）
pub const ANLOGIC_FRAME_MAGIC: u32 = 0x4D52_4641;
/// `ANLOGIC_FRAME_FLAG_COMPLETE`
pub const ANLOGIC_FRAME_FLAG_COMPLETE: u32 = 0x1;
/// `ANLOGIC_FRAME_MAX_WAIT_MS`
pub const ANLOGIC_FRAME_MAX_WAIT_MS: u32 = 600_000;

const ANLOGIC_IOC_INFO: u8 = 1;
const ANLOGIC_IOCFRAMEINFO: u8 = 10;
const ANLOGIC_IOCFRAMEWAIT: u8 = 11;

/// 与 C 侧 `_Static_assert` 等价：布局一旦对不上就编不过，而不是取到错数据。
const _: () = {
    assert!(size_of::<AnlogicIocFrame>() == 56);
    assert!(size_of::<AnlogicIocFrameWait>() == 24);
    // 字段偏移逐条钉死：字段全是同宽同类型，只钉 sizeof 挡不住换序
    assert!(offset_of!(AnlogicIocFrame, phys_addr) == 0);
    assert!(offset_of!(AnlogicIocFrame, size) == 8);
    assert!(offset_of!(AnlogicIocFrame, stride) == 16);
    assert!(offset_of!(AnlogicIocFrame, slot_bytes) == 20);
    assert!(offset_of!(AnlogicIocFrame, hdr_bytes) == 24);
    assert!(offset_of!(AnlogicIocFrame, slots) == 28);
    assert!(offset_of!(AnlogicIocFrame, width) == 32);
    assert!(offset_of!(AnlogicIocFrame, height) == 36);
    assert!(offset_of!(AnlogicIocFrame, seq) == 40);
    assert!(offset_of!(AnlogicIocFrame, count) == 44);
    assert!(offset_of!(AnlogicIocFrame, mb_off) == 48);
    assert!(size_of::<FrameMailbox>() == 64);
    assert!(offset_of!(FrameMailbox, magic) == 0);
    assert!(offset_of!(FrameMailbox, seq) == 4);
    assert!(offset_of!(FrameMailbox, count) == 8);
    assert!(offset_of!(FrameMailbox, slot_bytes) == 12);
    assert!(offset_of!(FrameMailbox, slots) == 16);
    assert!(offset_of!(FrameMailbox, stride) == 20);
    assert!(offset_of!(FrameMailbox, width) == 24);
    assert!(offset_of!(FrameMailbox, height) == 28);
    assert!(size_of::<FrameSlotHdr>() == 16);
    assert!(offset_of!(FrameSlotHdr, seq) == 0);
    assert!(offset_of!(FrameSlotHdr, len) == 4);
    assert!(offset_of!(FrameSlotHdr, flags) == 8);
    // ioctl 编号照头文件展开值钉死（与 driver/app/rs/src/ioctl.rs 对齐）；
    // `ANLOGIC_IOC_INFO` 量的是 `sizeof(struct anlogic_ioc_info) == 40`。
    assert!(
        ioc(
            IOC_READ | IOC_WRITE,
            ANLOGIC_IOC_MAGIC,
            ANLOGIC_IOC_INFO,
            40
        ) == 0xc028_7801
    );
    assert!(
        ioc(
            IOC_READ,
            ANLOGIC_IOC_MAGIC,
            ANLOGIC_IOCFRAMEINFO,
            size_of::<AnlogicIocFrame>()
        ) == 0x8038_780a
    );
    assert!(
        ioc(
            IOC_READ | IOC_WRITE,
            ANLOGIC_IOC_MAGIC,
            ANLOGIC_IOCFRAMEWAIT,
            size_of::<AnlogicIocFrameWait>()
        ) == 0xc018_780b
    );
};

/* ------------------------------------------------------------------ *
 * 取帧缓冲描述（ioctl 报出来的几何，定义域内已自检）
 * ------------------------------------------------------------------ */

/// 驱动报出来的收帧缓冲描述，字段与 `struct anlogic_ioc_frame` 同名。
#[derive(Clone, Copy, Debug)]
pub struct FrameBufferInfo {
    pub phys_addr: u64,
    pub size: u64,
    pub stride: u32,
    pub slot_bytes: u32,
    pub hdr_bytes: u32,
    pub slots: u32,
    pub width: u32,
    pub height: u32,
    pub seq: u32,
    pub count: u32,
    pub mb_off: u32,
}

impl FrameBufferInfo {
    /// 一槽内容需要的字节数。
    ///
    /// 不是宽×高：当前正式通路的一槽是 R10F（128 B 头 + packed RAW10 + 尾零），
    /// 驱动报的 `slot_bytes` 才是真正要 mmap/读取的长度。旧 8 bit 灰度通路下
    /// `slot_bytes == width*height`，两种布局共用这一条。
    pub fn frame_bytes(&self) -> usize {
        self.slot_bytes as usize
    }
}

/// 帧就绪轮询的结果
#[derive(Clone, Copy, Debug)]
pub struct WaitResult {
    pub ready: bool,
    pub seq: u32,
    pub count: u32,
    pub slot: u32,
}

/// 取某一槽时遇到的不一致（可跳过重取，不是致命错误）
#[derive(Debug)]
pub enum SlotError {
    /// 槽头序号对不上 / 没标 COMPLETE / 搬运期间序号绕回
    Inconsistent(String),
    /// 系统调用失败
    Io(io::Error),
}

impl std::fmt::Display for SlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlotError::Inconsistent(s) => write!(f, "帧不一致：{s}"),
            SlotError::Io(e) => write!(f, "取帧 IO 错误：{e}"),
        }
    }
}

impl From<io::Error> for SlotError {
    fn from(e: io::Error) -> Self {
        SlotError::Io(e)
    }
}

/// 一次成功取到的槽内容
#[derive(Clone, Copy, Debug)]
pub struct SlotRead {
    pub seq: u32,
    pub count: u32,
    /// 槽头报的有效像素字节数
    pub len: u32,
    /// 实际拷进调用方缓冲的字节数（不足部分已补 0）
    pub copied: usize,
}

/* ------------------------------------------------------------------ *
 * 只读 mmap 的持有者
 * ------------------------------------------------------------------ */

struct Mapping {
    base: *mut libc::c_void,
    len: usize,
}

// 映射只在取帧时被读；跨线程移动是安全的（内容由 FPGA 单写）
unsafe impl Send for Mapping {}

impl Mapping {
    fn as_ptr(&self) -> *const u8 {
        self.base as *const u8
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base, self.len);
        }
    }
}

/* ------------------------------------------------------------------ *
 * 字符设备
 * ------------------------------------------------------------------ */

/// 收帧缓冲节点。open 时做完所有自检（flock / 描述符 / mmap / magic），
/// 用不出来就当场报错，不带半个可用状态往下走。
pub struct FrameDevice {
    file: OwnedFd,
    map: Mapping,
    info: FrameBufferInfo,
}

/// 把系统调用的 errno 包成带上下文的 io::Error
fn ctx(op: &str, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{op}：{e}"))
}

fn ioctl_ptr<T>(fd: RawFd, req: u64, arg: &mut T) -> io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, req as libc::c_ulong, arg as *mut T) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl FrameDevice {
    /// 打开收帧缓冲节点并做完自检。参数是节点路径，如
    /// `/dev/ANLOGIC-PCI0_frame`。
    pub fn open(path: &Path) -> io::Result<Self> {
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "设备路径里有 NUL 字节"))?;
        // 只读打开：本程序不写设备
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(ctx(
                &format!("打不开收帧缓冲节点 {}", path.display()),
                io::Error::last_os_error(),
            ));
        }
        let file = unsafe { OwnedFd::from_raw_fd(fd) };

        // 与 frame_grab.c 一样独占：两个进程同时取帧会互相抢槽
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(ctx(
                &format!("{} 已被其它进程占用（flock）", path.display()),
                io::Error::last_os_error(),
            ));
        }

        // 问缓冲几何
        let mut raw = AnlogicIocFrame::default();
        ioctl_ptr(
            file.as_raw_fd(),
            ioc(
                IOC_READ,
                ANLOGIC_IOC_MAGIC,
                ANLOGIC_IOCFRAMEINFO,
                size_of::<AnlogicIocFrame>(),
            ),
            &mut raw,
        )
        .map_err(|e| ctx("ANLOGIC_IOCFRAMEINFO 失败", e))?;

        let info = FrameBufferInfo {
            phys_addr: raw.phys_addr,
            size: raw.size,
            stride: raw.stride,
            slot_bytes: raw.slot_bytes,
            hdr_bytes: raw.hdr_bytes,
            slots: raw.slots,
            width: raw.width,
            height: raw.height,
            seq: raw.seq,
            count: raw.count,
            mb_off: raw.mb_off,
        };
        check_descriptor(&info).map_err(|m| io::Error::new(io::ErrorKind::InvalidData, m))?;

        // 只读映射整块缓冲（驱动也只接受 PROT_READ | MAP_SHARED）
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                info.size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(ctx(
                &format!("mmap {} 字节的收帧缓冲失败", info.size),
                io::Error::last_os_error(),
            ));
        }
        let map = Mapping {
            base: ptr,
            len: info.size as usize,
        };
        let dev = FrameDevice { file, map, info };

        // mailbox 定位靠 ioctl 报的 mb_off，不按常量猜
        if dev.mailbox_u32(offset_of!(FrameMailbox, magic)) != ANLOGIC_FRAME_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "mailbox（缓冲内偏移 {}）的 magic 不是 0x{:08x}：驱动还没写好描述符",
                    dev.info.mb_off, ANLOGIC_FRAME_MAGIC
                ),
            ));
        }
        Ok(dev)
    }

    pub fn info(&self) -> &FrameBufferInfo {
        &self.info
    }

    /// 一行式描述，给启动日志用
    pub fn describe(&self) -> String {
        format!(
            "收帧缓冲 phys=0x{:x} size={}KiB {}槽 步长{}B 槽头{}B 每帧{}x{}={}B mailbox偏移{} seq={} count={}",
            self.info.phys_addr,
            self.info.size / 1024,
            self.info.slots,
            self.info.stride,
            self.info.hdr_bytes,
            self.info.width,
            self.info.height,
            self.info.slot_bytes,
            self.info.mb_off,
            self.mailbox_seq(),
            self.mailbox_count(),
        )
    }

    /// 读 mailbox 的一个 u32 字段。FPGA 是这块内存的唯一写者，读侧要 volatile
    /// 加屏障，免得编译器合并/提前取值（与 frame_grab.c 的 mb_field 同义）。
    fn mailbox_u32(&self, field_off: usize) -> u32 {
        let addr = self
            .map
            .as_ptr()
            .wrapping_add(self.info.mb_off as usize + field_off);
        let v = unsafe { std::ptr::read_volatile(addr as *const u32) };
        fence(Ordering::SeqCst);
        v
    }

    /// mailbox.seq：已完成帧的序号（0 = 还没有帧）
    pub fn mailbox_seq(&self) -> u32 {
        self.mailbox_u32(offset_of!(FrameMailbox, seq))
    }

    /// mailbox.count：累计完成帧数（判回绕用）
    pub fn mailbox_count(&self) -> u32 {
        self.mailbox_u32(offset_of!(FrameMailbox, count))
    }

    /// 等一帧就绪。`last_seq` 是调用方已消费到的序号（0 = 缓冲里有帧就返回），
    /// `timeout_ms` 会被驱动按 `ANLOGIC_FRAME_MAX_WAIT_MS` 夹上限。
    pub fn wait_frame(&self, last_seq: u32, timeout_ms: u32) -> io::Result<WaitResult> {
        let mut w = AnlogicIocFrameWait {
            last_seq,
            timeout_ms: timeout_ms.min(ANLOGIC_FRAME_MAX_WAIT_MS),
            ..Default::default()
        };
        ioctl_ptr(
            self.file.as_raw_fd(),
            ioc(
                IOC_READ | IOC_WRITE,
                ANLOGIC_IOC_MAGIC,
                ANLOGIC_IOCFRAMEWAIT,
                size_of::<AnlogicIocFrameWait>(),
            ),
            &mut w,
        )
        .map_err(|e| ctx("ANLOGIC_IOCFRAMEWAIT 失败", e))?;
        Ok(WaitResult {
            ready: w.ready == 1,
            seq: w.seq,
            count: w.count,
            slot: w.slot,
        })
    }

    /// 取一槽的像素到 `out`（长度必须是宽×高）。槽头检查不通过时返回
    /// `SlotError::Inconsistent`，调用方跳过重取即可。
    pub fn read_slot(
        &self,
        slot: u32,
        expect_seq: u32,
        out: &mut [u8],
    ) -> Result<SlotRead, SlotError> {
        if slot >= self.info.slots {
            return Err(SlotError::Inconsistent(format!(
                "驱动报的槽位 {} 越界（共 {} 槽）",
                slot, self.info.slots
            )));
        }
        if out.len() < self.info.frame_bytes() {
            return Err(SlotError::Inconsistent(format!(
                "目标缓冲 {} 字节小于一槽内容 {} 字节",
                out.len(),
                self.info.frame_bytes()
            )));
        }

        // 本路在整块缓冲里的起点（见文件头）：槽区恒在路首、mailbox 紧随其后
        let stream_off =
            self.info.mb_off as usize - self.info.stride as usize * self.info.slots as usize;
        let slot_off = stream_off + slot as usize * self.info.stride as usize;
        let base = unsafe { self.map.as_ptr().add(slot_off) };

        // 顺序：先 seq，再 len/flags（与 frame_grab.c 一致）
        let hdr_seq = volatile_u32(unsafe { base.add(offset_of!(FrameSlotHdr, seq)) });
        let hdr_len = volatile_u32(unsafe { base.add(offset_of!(FrameSlotHdr, len)) });
        let hdr_flags = volatile_u32(unsafe { base.add(offset_of!(FrameSlotHdr, flags)) });

        if hdr_seq != expect_seq {
            return Err(SlotError::Inconsistent(format!(
                "槽头序号 {} 与 mailbox.seq {} 不一致（FPGA 没先写槽头就改了序号？）",
                hdr_seq, expect_seq
            )));
        }
        if hdr_flags & ANLOGIC_FRAME_FLAG_COMPLETE == 0 {
            return Err(SlotError::Inconsistent(format!(
                "槽头 flags=0x{:x} 没标 COMPLETE(bit0)：这一槽可能还在被写",
                hdr_flags
            )));
        }

        // 拷贝：len 夹到 slot_bytes 与一帧大小；不足的补 0（并如实告知调用方）
        let mut copy = hdr_len as usize;
        if copy > self.info.slot_bytes as usize {
            copy = self.info.slot_bytes as usize;
        }
        if copy > out.len() {
            copy = out.len();
        }
        out[copy..].fill(0);
        unsafe {
            std::ptr::copy_nonoverlapping(
                base.add(self.info.hdr_bytes as usize),
                out.as_mut_ptr(),
                copy,
            );
        }

        // 搬运后复查：只有环形缓冲绕回一圈才可能盖掉刚取的这一帧
        let seq_now = self.mailbox_seq();
        if seq_now != expect_seq {
            let advanced = seq_now.wrapping_sub(expect_seq);
            if advanced >= self.info.slots {
                return Err(SlotError::Inconsistent(format!(
                    "搬运期间序号 {} → {}（新出 {} 帧 ≥ {} 槽）：这一槽可能已被覆盖",
                    expect_seq, seq_now, advanced, self.info.slots
                )));
            }
        }

        Ok(SlotRead {
            seq: expect_seq,
            count: self.mailbox_count(),
            len: hdr_len,
            copied: copy,
        })
    }
}

fn volatile_u32(p: *const u8) -> u32 {
    let v = unsafe { std::ptr::read_volatile(p as *const u32) };
    fence(Ordering::SeqCst);
    v
}

/// 描述符自检：与 frame_grab.c 的 descriptor_check 同口径，宁可启动就报错，
/// 也不要拿着对不上的地址去算偏移。
fn check_descriptor(i: &FrameBufferInfo) -> Result<(), String> {
    if i.phys_addr == 0
        || i.size == 0
        || i.slots == 0
        || i.slot_bytes == 0
        || i.stride == 0
        || i.hdr_bytes == 0
        || i.width == 0
        || i.height == 0
        || i.mb_off == 0
    {
        return Err(format!("驱动报的收帧缓冲描述符不完整：{i:?}"));
    }
    if i.stride < i.hdr_bytes + i.slot_bytes {
        return Err(format!(
            "步长 {} 装不下槽头 {} + 一帧 {}",
            i.stride, i.hdr_bytes, i.slot_bytes
        ));
    }
    if i.size < i.hdr_bytes as u64 + i.stride as u64 * i.slots as u64 {
        return Err(format!(
            "缓冲 {} 字节装不下 {} 槽 × 步长 {}",
            i.size, i.slots, i.stride
        ));
    }
    if i.mb_off & 0xFFF != 0 {
        return Err(format!(
            "mb_off {} 不是页对齐（mailbox 得独占一页）",
            i.mb_off
        ));
    }
    if (i.mb_off as u64) < i.stride as u64 * i.slots as u64 {
        return Err(format!(
            "mb_off {} 落在槽区（{} 槽 × 步长 {}）里，会互相覆盖",
            i.mb_off, i.slots, i.stride
        ));
    }
    if i.mb_off as u64 + size_of::<FrameMailbox>() as u64 > i.size {
        return Err(format!(
            "mailbox 偏移 {} + 64 字节超出缓冲 {}",
            i.mb_off, i.size
        ));
    }
    if i.slot_bytes < i.width * i.height {
        // 不致命：小于一帧时像素会补 0
        eprintln!(
            "警告：一帧只给了 {} 字节，小于 {}x{} 灰度需要的 {} 字节",
            i.slot_bytes,
            i.width,
            i.height,
            i.width * i.height
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_header() {
        // 照抄 driver/app/rs/src/ioctl.rs 的同名用例：命令字一旦算错就当场响
        assert_eq!(
            ioc(
                IOC_READ | IOC_WRITE,
                ANLOGIC_IOC_MAGIC,
                ANLOGIC_IOC_INFO,
                40
            ),
            0xc028_7801
        );
        assert_eq!(
            ioc(
                IOC_READ,
                ANLOGIC_IOC_MAGIC,
                ANLOGIC_IOCFRAMEINFO,
                size_of::<AnlogicIocFrame>()
            ),
            0x8038_780a
        );
        assert_eq!(
            ioc(
                IOC_READ | IOC_WRITE,
                ANLOGIC_IOC_MAGIC,
                ANLOGIC_IOCFRAMEWAIT,
                size_of::<AnlogicIocFrameWait>()
            ),
            0xc018_780b
        );
    }
}
