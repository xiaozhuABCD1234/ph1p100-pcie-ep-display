//! RK3588 硬件 JPEG 编码：/dev/video4（hantro VEPU121）的 V4L2 M2M 接口。
//!
//! 只做一件事：吃 NV12（NM12：Y 平面 + UV 平面），吐 JFIF JPEG。1080p 一帧
//! 编码 1 ms 量级，而纯 Rust 软件编码要 60 ms，这是整条推流流水线的关键一环。
//!
//! 几个跟驱动有关的坑（都在板子上实测过，别按通用 V4L2 顺序想当然）：
//!   * 必须先 S_FMT(CAPTURE, JPEG) 再 S_FMT(OUTPUT, NV12)。反过来这个驱动会把
//!     输出队列的格式信息冲掉，REQBUFS 照样成功但随后 QUERYBUF 一直 EINVAL。
//!   * 两个平面是**各自独立映射**的（UN12M 的 plane 1 偏移不等于 plane 0 长度），
//!     不能当连续内存用。
//!   * 宽高会被驱动向上对齐（宽度最小 96、按 16 对齐），多出来的边缘要自己补。
//!
//! 结构体大小按 arm64 ABI 逐字段对齐（V4l2Format=208、V4l2Buffer=88、V4l2Plane=64
//! 等），ioctl 请求号里的 size 必须一致，否则内核返回 ENOTTY。

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- ioctl 请求号

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

const VIDIOC_QUERYCAP: u64 = ioc(IOC_READ, b'V', 0, 104);
const VIDIOC_S_FMT: u64 = ioc(IOC_READ | IOC_WRITE, b'V', 5, 208);
const VIDIOC_REQBUFS: u64 = ioc(IOC_READ | IOC_WRITE, b'V', 8, 20);
const VIDIOC_QUERYBUF: u64 = ioc(IOC_READ | IOC_WRITE, b'V', 9, 88);
const VIDIOC_QBUF: u64 = ioc(IOC_READ | IOC_WRITE, b'V', 15, 88);
const VIDIOC_DQBUF: u64 = ioc(IOC_READ | IOC_WRITE, b'V', 17, 88);
const VIDIOC_STREAMON: u64 = ioc(IOC_WRITE, b'V', 18, 4);
const VIDIOC_STREAMOFF: u64 = ioc(IOC_WRITE, b'V', 19, 4);
const VIDIOC_S_CTRL: u64 = ioc(IOC_READ | IOC_WRITE, b'V', 28, 8);

// ---------------------------------------------------------------- V4L2 常量

const BUF_CAPTURE_MPLANE: u32 = 9;
const BUF_OUTPUT_MPLANE: u32 = 10;
const MEMORY_MMAP: u32 = 1;
const FIELD_NONE: u32 = 1;
const COLORSPACE_JPEG: u32 = 7;
const YCBCR_ENC_601: u32 = 1;
const QUANTIZATION_FULL_RANGE: u32 = 2;
/// V4L2_CAP_VIDEO_M2M_MPLANE
const CAP_M2M_MPLANE: u32 = 0x0000_4000;
/// V4L2_CID_JPEG_COMPRESSION_QUALITY
const CID_JPEG_QUALITY: u32 = 0x009d_0903;
/// 每个队列申请的缓冲数；两个源缓冲才能一边编码一边填下一帧
const NBUFS: usize = 2;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// 'NM12'：Y 平面 + UV 平面（非连续），等价于两平面 NV12。
const fn nv12_fourcc() -> u32 {
    fourcc(b'N', b'M', b'1', b'2')
}
const JPEG_FOURCC: u32 = fourcc(b'J', b'P', b'E', b'G');

// ---------------------------------------------------------------- uapi 结构体

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PlanePixFormat {
    sizeimage: u32,
    bytesperline: u32,
    reserved: [u16; 6],
}

/// struct v4l2_pix_format_mplane：192 字节。
#[repr(C)]
#[derive(Clone, Copy)]
struct PixFormatMplane {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    colorspace: u32,
    plane_fmt: [PlanePixFormat; 8],
    num_planes: u8,
    flags: u8,
    ycbcr_enc: u8,
    quantization: u8,
    xfer_func: u8,
    reserved: [u8; 7],
}

impl Default for PixFormatMplane {
    fn default() -> Self {
        PixFormatMplane {
            width: 0,
            height: 0,
            pixelformat: 0,
            field: FIELD_NONE,
            colorspace: COLORSPACE_JPEG,
            plane_fmt: [PlanePixFormat::default(); 8],
            num_planes: 0,
            flags: 0,
            ycbcr_enc: YCBCR_ENC_601 as u8,
            quantization: QUANTIZATION_FULL_RANGE as u8,
            xfer_func: 0,
            reserved: [0; 7],
        }
    }
}

/// struct v4l2_format 的格式联合体：最大成员是 raw_data[200]，按 8 字节对齐后
/// 仍是 200，加上 type 与对齐填充，整个结构正好 208 字节（板子上用 gcc 核对过）。
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct FormatUnion {
    raw: [u8; 200],
}

impl Default for FormatUnion {
    fn default() -> Self {
        FormatUnion { raw: [0; 200] }
    }
}

#[repr(C)]
struct V4l2Format {
    type_: u32,
    _pad: u32,
    fmt: FormatUnion,
}

impl V4l2Format {
    fn new(buf_type: u32) -> Self {
        V4l2Format {
            type_: buf_type,
            _pad: 0,
            fmt: FormatUnion::default(),
        }
    }

    fn pix_mp_mut(&mut self) -> &mut PixFormatMplane {
        unsafe { &mut *(self.fmt.raw.as_mut_ptr() as *mut PixFormatMplane) }
    }

    fn pix_mp(&self) -> &PixFormatMplane {
        unsafe { &*(self.fmt.raw.as_ptr() as *const PixFormatMplane) }
    }
}

/// struct v4l2_plane：64 字节。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Plane {
    bytesused: u32,
    length: u32,
    /// union { mem_offset, userptr, fd }
    m: u64,
    data_offset: u32,
    reserved: [u32; 11],
}

/// struct v4l2_buffer：88 字节（含 timeval 与 timecode）。
#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    _pad0: u32,
    tv_sec: i64,
    tv_usec: i64,
    timecode: [u8; 16],
    sequence: u32,
    memory: u32,
    /// union { offset, userptr, planes*, fd }
    m: u64,
    length: u32,
    reserved2: u32,
    request_fd: i32,
    _pad1: u32,
}

impl Default for V4l2Buffer {
    fn default() -> Self {
        V4l2Buffer {
            index: 0,
            type_: 0,
            bytesused: 0,
            flags: 0,
            field: FIELD_NONE,
            _pad0: 0,
            tv_sec: 0,
            tv_usec: 0,
            timecode: [0; 16],
            sequence: 0,
            memory: MEMORY_MMAP,
            m: 0,
            length: 0,
            reserved2: 0,
            request_fd: 0,
            _pad1: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    reserved: [u32; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Control {
    id: u32,
    value: i32,
}

// ---------------------------------------------------------------- 映射缓冲

struct Map {
    ptr: *mut u8,
    len: usize,
}

// 缓冲由驱动 mmap 出来，跟着 HwJpeg 走；HwJpeg 放在 Mutex 后面跨线程用。
unsafe impl Send for Map {}

impl Map {
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

/// 一个 NV12 输入缓冲：两个平面各自 mmap，必须分开拷。
struct SrcBuf {
    y: Map,
    uv: Map,
}

// ---------------------------------------------------------------- 编码器

/// 给 ioctl 失败补上「哪一步」，否则只有一句 os error 没法查。
fn step<T>(name: &str, r: io::Result<T>) -> io::Result<T> {
    r.map_err(|e| io::Error::new(e.kind(), format!("{name}: {e}")))
}

fn debug_on() -> bool {
    std::env::var_os("DISPLAY_DEBUG_V4L2").is_some()
}

/// 一个 VEPU 实例。不是线程安全的：调用方用 Mutex 串起来。
pub struct HwJpeg {
    fd: OwnedFd,
    /// 调用方喂进来的尺寸
    src_w: u32,
    src_h: u32,
    /// 驱动实际接受的尺寸（会向上对齐）
    width: u32,
    height: u32,
    y_stride: u32,
    uv_stride: u32,
    src: Vec<SrcBuf>,
    cap: Vec<Map>,
    src_idx: usize,
    cap_idx: usize,
    cap_queued: usize,
    timeout_ms: u64,
}

impl HwJpeg {
    /// 打开并把编码器配置到 width × height，quality 走 V4L2_CID_JPEG_COMPRESSION_QUALITY。
    pub fn open(path: &Path, width: u32, height: u32, quality: u8) -> io::Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("硬件编码要求偶数尺寸，收到 {width}x{height}"),
            ));
        }
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "设备路径里有 NUL"))?;
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut me = HwJpeg {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            src_w: width,
            src_h: height,
            width,
            height,
            y_stride: width,
            uv_stride: width / 2,
            src: Vec::new(),
            cap: Vec::new(),
            src_idx: 0,
            cap_idx: 0,
            cap_queued: 0,
            timeout_ms: 1000,
        };
        step("QUERYCAP", me.check_caps())?;
        step("S_FMT", me.set_formats())?;
        step("S_CTRL", me.set_quality(quality))?;
        step("REQBUFS", me.alloc_bufs())?;
        step("STREAMON", me.stream_on())?;
        // capture 侧先排满，第一次编码才有缓冲可写
        while me.cap_queued < me.cap.len() {
            let i = me.cap_idx;
            step("QBUF(cap 预排)", me.queue(BUF_CAPTURE_MPLANE, i, &[]))?;
            me.cap_idx = (me.cap_idx + 1) % me.cap.len();
            me.cap_queued += 1;
        }
        Ok(me)
    }

    /// 驱动实际编码出来的宽度
    pub fn drv_width(&self) -> u32 {
        self.width
    }

    /// 驱动实际编码出来的高度（1080 会被对齐成 1088）
    pub fn drv_height(&self) -> u32 {
        self.height
    }

    /// 调用方喂进来的宽度（不是驱动对齐后的宽度）
    pub fn src_width(&self) -> u32 {
        self.src_w
    }

    /// 调用方喂进来的高度（1080 会被驱动对齐成 1088，这里仍是 1080）
    pub fn src_height(&self) -> u32 {
        self.src_h
    }

    /// NV12 尺寸要求：y.len() >= width*height，uv.len() >= width*height/2。
    pub fn encode(&mut self, y: &[u8], uv: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        let (w, h) = (self.src_w as usize, self.src_h as usize);
        let (dw, dh) = (self.width as usize, self.height as usize);
        if y.len() < w * h || uv.len() < w * h / 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "NV12 长度不对：Y={}（要 {}），UV={}（要 {}）",
                    y.len(),
                    w * h,
                    uv.len(),
                    w * h / 2
                ),
            ));
        }

        let si = self.src_idx;
        let (ys, uvs) = (self.y_stride as usize, self.uv_stride as usize);
        {
            let buf = &mut self.src[si];
            copy_plane(buf.y.bytes_mut(), ys, w, h, dw, dh, y);
            copy_plane(
                buf.uv.bytes_mut(),
                uvs,
                w,
                h / 2,
                dw,
                dh / 2,
                &uv[..w * h / 2],
            );
        }

        let used = [(ys * dh) as u32, (uvs * (dh / 2)) as u32];
        step("QBUF(源)", self.queue(BUF_OUTPUT_MPLANE, si, &used))?;

        let (ci, len) = match self.dequeue(BUF_CAPTURE_MPLANE) {
            Ok(v) => v,
            Err(e) => {
                let _ = self.dequeue(BUF_OUTPUT_MPLANE);
                return Err(e);
            }
        };
        self.cap_queued -= 1;
        if len == 0 {
            self.requeue(BUF_CAPTURE_MPLANE, ci);
            return Err(io::Error::new(io::ErrorKind::Other, "硬件编码返回空帧"));
        }
        out.clear();
        out.extend_from_slice(&self.cap[ci].bytes()[..len]);
        self.requeue(BUF_CAPTURE_MPLANE, ci);

        // 源缓冲处理完也回到应用手里，收回来才能下次再用
        let _ = self.dequeue(BUF_OUTPUT_MPLANE);
        self.src_idx = (si + 1) % self.src.len();
        Ok(())
    }

    // ------------------------------------------------------------ 内部步骤

    fn check_caps(&mut self) -> io::Result<()> {
        let mut cap = V4l2Capability::default();
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), VIDIOC_QUERYCAP, &mut cap) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if (cap.capabilities | cap.device_caps) & CAP_M2M_MPLANE == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "这个节点不是多平面 M2M 编码器",
            ));
        }
        Ok(())
    }

    fn set_fmt_one(&mut self, f: V4l2Format) -> io::Result<PixFormatMplane> {
        let mut f = f;
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), VIDIOC_S_FMT, &mut f) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let got = *f.pix_mp();
        if debug_on() {
            eprintln!(
                "  [v4l2] type={} {}x{} fmt={:#x} planes={} p0(size={} bpl={}) p1(size={} bpl={})",
                f.type_,
                got.width,
                got.height,
                got.pixelformat,
                got.num_planes,
                got.plane_fmt[0].sizeimage,
                got.plane_fmt[0].bytesperline,
                got.plane_fmt[1].sizeimage,
                got.plane_fmt[1].bytesperline
            );
        }
        Ok(got)
    }

    /// 顺序很重要：先 CAPTURE 后 OUTPUT，否则输出队列的 QUERYBUF 会一直 EINVAL。
    fn set_formats(&mut self) -> io::Result<()> {
        let (w, h) = (self.src_w, self.src_h);

        let mut cap = V4l2Format::new(BUF_CAPTURE_MPLANE);
        {
            let p = cap.pix_mp_mut();
            p.width = w;
            p.height = h;
            p.pixelformat = JPEG_FOURCC;
            p.field = FIELD_NONE;
            p.colorspace = COLORSPACE_JPEG;
            p.num_planes = 1;
            p.plane_fmt[0].sizeimage = w * h; // 驱动会自己放大到够用
        }
        let got = self.set_fmt_one(cap)?;
        if got.pixelformat != JPEG_FOURCC {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("硬件编码器不出 JPEG（回的是 {:#x}）", got.pixelformat),
            ));
        }

        let mut out = V4l2Format::new(BUF_OUTPUT_MPLANE);
        {
            let p = out.pix_mp_mut();
            p.width = w;
            p.height = h;
            p.pixelformat = nv12_fourcc();
            p.field = FIELD_NONE;
            p.colorspace = COLORSPACE_JPEG;
            p.ycbcr_enc = YCBCR_ENC_601 as u8;
            p.quantization = QUANTIZATION_FULL_RANGE as u8;
            p.num_planes = 2;
            p.plane_fmt[0].bytesperline = w;
            p.plane_fmt[0].sizeimage = w * h;
            p.plane_fmt[1].bytesperline = w;
            p.plane_fmt[1].sizeimage = w * h / 2;
        }
        let got = self.set_fmt_one(out)?;
        if got.pixelformat != nv12_fourcc() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("硬件编码器不接受 NV12（回的是 {:#x}）", got.pixelformat),
            ));
        }
        // 驱动会把宽高向上对齐（宽度最小 96、按 16 对齐）；只要求装得下。
        if got.width < w || got.height < h {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "硬件编码器缩到了 {}x{}，装不下 {w}x{h}",
                    got.width, got.height
                ),
            ));
        }
        self.width = got.width;
        self.height = got.height;
        self.y_stride = got.plane_fmt[0].bytesperline.max(got.width);
        self.uv_stride = got.plane_fmt[1].bytesperline.max(got.width / 2);
        Ok(())
    }

    fn set_quality(&mut self, quality: u8) -> io::Result<()> {
        let mut c = V4l2Control {
            id: CID_JPEG_QUALITY,
            value: quality.clamp(5, 100) as i32,
        };
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), VIDIOC_S_CTRL, &mut c) };
        if rc < 0 {
            // 质量设不上不算致命：驱动会用默认质量
            return Ok(());
        }
        Ok(())
    }

    fn reqbufs(&mut self, buf_type: u32, count: u32) -> io::Result<u32> {
        let mut rb = V4l2RequestBuffers {
            count,
            type_: buf_type,
            memory: MEMORY_MMAP,
            capabilities: 0,
            reserved: [0],
        };
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), VIDIOC_REQBUFS, &mut rb) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(rb.count)
    }

    fn alloc_bufs(&mut self) -> io::Result<()> {
        for (buf_type, tag) in [(BUF_OUTPUT_MPLANE, "out"), (BUF_CAPTURE_MPLANE, "cap")] {
            let n = step(
                &format!("REQBUFS({tag})"),
                self.reqbufs(buf_type, NBUFS as u32),
            )?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "驱动没给缓冲（REQBUFS 返回 0）",
                ));
            }
            for i in 0..n {
                let planes = step(&format!("QUERYBUF({tag} #{i})"), self.query(buf_type, i))?;
                if buf_type == BUF_OUTPUT_MPLANE {
                    let y = map_plane(self.fd.as_raw_fd(), &planes[0])?;
                    let uv = map_plane(self.fd.as_raw_fd(), &planes[1])?;
                    self.src.push(SrcBuf { y, uv });
                } else {
                    self.cap.push(map_plane(self.fd.as_raw_fd(), &planes[0])?);
                }
            }
        }
        Ok(())
    }

    fn query(&mut self, buf_type: u32, index: u32) -> io::Result<[V4l2Plane; 2]> {
        let mut planes = [V4l2Plane::default(); 2];
        let mut b = V4l2Buffer::default();
        b.type_ = buf_type;
        b.memory = MEMORY_MMAP;
        b.index = index;
        b.length = if buf_type == BUF_OUTPUT_MPLANE { 2 } else { 1 };
        b.m = planes.as_mut_ptr() as u64;
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), VIDIOC_QUERYBUF, &mut b) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(planes)
    }

    fn queue(&mut self, buf_type: u32, index: usize, used: &[u32]) -> io::Result<()> {
        let n = if buf_type == BUF_OUTPUT_MPLANE { 2 } else { 1 };
        let mut planes = [V4l2Plane::default(); 2];
        for (i, plane) in planes.iter_mut().enumerate().take(n) {
            plane.bytesused = used.get(i).copied().unwrap_or(0);
            plane.length = self.plane_len(buf_type, index, i) as u32;
        }
        let mut b = V4l2Buffer::default();
        b.type_ = buf_type;
        b.memory = MEMORY_MMAP;
        b.index = index as u32;
        b.length = n as u32;
        b.m = planes.as_mut_ptr() as u64;
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), VIDIOC_QBUF, &mut b) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn requeue(&mut self, buf_type: u32, index: usize) {
        if self.queue(buf_type, index, &[]).is_ok() {
            self.cap_queued += 1;
        }
    }

    fn plane_len(&self, buf_type: u32, index: usize, plane: usize) -> usize {
        if buf_type == BUF_OUTPUT_MPLANE {
            match self.src.get(index) {
                Some(b) if plane == 0 => b.y.len,
                Some(b) => b.uv.len,
                None => 0,
            }
        } else {
            self.cap.get(index).map(|m| m.len).unwrap_or(0)
        }
    }

    fn dequeue(&mut self, buf_type: u32) -> io::Result<(usize, usize)> {
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms);
        loop {
            let mut planes = [V4l2Plane::default(); 2];
            let mut b = V4l2Buffer::default();
            b.type_ = buf_type;
            b.memory = MEMORY_MMAP;
            b.length = if buf_type == BUF_OUTPUT_MPLANE { 2 } else { 1 };
            b.m = planes.as_mut_ptr() as u64;
            let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), VIDIOC_DQBUF, &mut b) };
            if rc == 0 {
                let idx = b.index as usize;
                let used = planes[0].bytesused as usize + planes[0].data_offset as usize;
                let used = used.min(self.plane_len(buf_type, idx, 0));
                return Ok((idx, used));
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EINTR) => {}
                _ => return Err(e),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "硬件编码器 1 s 没回帧",
                ));
            }
            self.wait_readable(20)?;
        }
    }

    fn wait_readable(&self, ms: i32) -> io::Result<()> {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(());
            }
            return Err(e);
        }
        Ok(())
    }

    fn stream_on(&mut self) -> io::Result<()> {
        for buf_type in [BUF_CAPTURE_MPLANE, BUF_OUTPUT_MPLANE] {
            let mut t: i32 = buf_type as i32;
            let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMON, &mut t) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn stream_off(&mut self) {
        for buf_type in [BUF_OUTPUT_MPLANE, BUF_CAPTURE_MPLANE] {
            let mut t: i32 = buf_type as i32;
            unsafe { libc::ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMOFF, &mut t) };
        }
    }
}

impl Drop for HwJpeg {
    fn drop(&mut self) {
        self.stream_off();
        let all = self
            .src
            .iter()
            .flat_map(|b| [&b.y, &b.uv])
            .chain(self.cap.iter());
        for m in all {
            unsafe { libc::munmap(m.ptr as *mut libc::c_void, m.len) };
        }
    }
}

fn map_plane(fd: i32, plane: &V4l2Plane) -> io::Result<Map> {
    let len = plane.length as usize;
    if len == 0 {
        return Err(io::Error::new(io::ErrorKind::Other, "驱动给了零长度平面"));
    }
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            plane.m as i64,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(Map {
        ptr: ptr as *mut u8,
        len,
    })
}

/// 把 w×h 的平面搬进驱动缓冲（行距 stride，驱动尺寸 dw×dh），
/// 溢出部分用最后一列/最后一行补齐，免得编码器读到脏数据。
fn copy_plane(buf: &mut [u8], stride: usize, w: usize, h: usize, dw: usize, dh: usize, src: &[u8]) {
    let rows = h.min(dh);
    let cols = w.min(dw).min(stride).min(buf.len());
    for row in 0..rows {
        let dst = row * stride;
        if dst + cols > buf.len() {
            break;
        }
        let s = row * w;
        let n = cols.min(w);
        buf[dst..dst + n].copy_from_slice(&src[s..s + n]);
        let last = src[s + n - 1];
        for i in n..cols {
            buf[dst + i] = last;
        }
    }
    let lastrow = rows.saturating_sub(1) * stride;
    for row in rows..dh {
        let dst = row * stride;
        if dst + cols > buf.len() {
            break;
        }
        buf.copy_within(lastrow..lastrow + cols, dst);
    }
}

/// RGB888 → NV12（JFIF 全范围 BT.601）。色度按 2×2 取均值。
pub fn rgb_to_nv12(rgb: &[u8], w: usize, h: usize, y: &mut [u8], uv: &mut [u8]) {
    debug_assert!(rgb.len() >= w * h * 3);
    debug_assert!(y.len() >= w * h);
    debug_assert!(uv.len() >= w * h / 2);
    for row in 0..h {
        let src = &rgb[row * w * 3..(row + 1) * w * 3];
        let dst = &mut y[row * w..(row + 1) * w];
        for x in 0..w {
            let r = src[x * 3] as i32;
            let g = src[x * 3 + 1] as i32;
            let b = src[x * 3 + 2] as i32;
            dst[x] = clamp_u8((77 * r + 150 * g + 29 * b) >> 8);
        }
    }
    for row in (0..h).step_by(2) {
        let out = &mut uv[(row / 2) * w..(row / 2) * w + w];
        for x in (0..w).step_by(2) {
            let (mut rs, mut gs, mut bs) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                let base = ((row + dy) * w + x) * 3;
                for dx in 0..2 {
                    let p = base + dx * 3;
                    rs += rgb[p] as i32;
                    gs += rgb[p + 1] as i32;
                    bs += rgb[p + 2] as i32;
                }
            }
            let (r, g, b) = (rs / 4, gs / 4, bs / 4);
            out[x] = clamp_u8(((-43 * r - 85 * g + 128 * b) >> 8) + 128);
            out[x + 1] = clamp_u8(((128 * r - 107 * g - 21 * b) >> 8) + 128);
        }
    }
}

#[inline]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}
