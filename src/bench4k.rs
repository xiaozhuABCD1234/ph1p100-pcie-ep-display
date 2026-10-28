//! 4K 单路预览的本地基准（x86 相对值，不是板端数字）。
//!
//! 合成一帧真实的 R10F v1：3840×2160、stride 4800、packed RAW10，整帧
//! 10,371,072 B（128 B 头 + 10,368,000 B 载荷 + 2,944 B 尾零，= 2532 × 4 KiB，
//! 与 cam_r10f_writer 的 DMA_BYTES 公式一致）。然后走
//! 「解析 → 去马赛克 / CFA binning → 后处理 → RGB→NV12 → 软件 JPEG」，逐段计时。
//! 1080p 全尺寸作为对照行 —— 它就是 1080p 双路默认走的那条路。
//!
//! 默认 `#[ignore]`：debug 构建下 4K 全分辨率通路要好几秒，不适合进常规测试。
//!
//! ```sh
//! cargo test --release bench_4k -- --ignored --nocapture
//! ```

use crate::bayer::{self, BayerPhase};
use crate::source::Frame;
use crate::{auto, encode, encode_frame, hwjpeg, r10f, scale, StageTimer, YuvScratch};
use std::time::Instant;

const W: usize = 3840;
const H: usize = 2160;
const STRIDE: usize = 4800;
/// 4K 单路整帧长度：128 B 头 + 10,368,000 B 载荷 + 2,944 B 尾零 = 2532 × 4 KiB。
/// display 侧 R10F 解析只要求「≥ 头 + 载荷」，尾部长度不影响解析；这里按整帧长度构造。
const FRAME4K_BYTES: usize = 10_371_072;
const QUALITY: u8 = 80;
/// 每次配置跑几遍取最快的一遍（第一遍是预热）
const ROUNDS: usize = 3;

/// 合成 Bayer 像素：缓变底图 + ±16 级伪随机噪声。
/// 噪声是为了别让 JPEG 在平场上压得离谱，耗时失去参考意义。
fn synth_plane(w: usize, h: usize) -> Vec<u8> {
    let mut v = vec![0u8; w * h];
    let mut seed = 0x2026_1028u32;
    for y in 0..h {
        let base = 20 + (y * 80) / h;
        for x in 0..w {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = ((seed >> 24) & 31) as i32 - 16;
            v[y * w + x] = (base as i32 + (x * 80 / w) as i32 + n).clamp(0, 255) as u8;
        }
    }
    v
}

/// 按 R10F v1 打包：128 B 头 + packed RAW10（4 像素/5 字节）+ tail 字节零尾。
fn r10f_bytes(plane: &[u8], w: usize, h: usize, stride: usize, tail: usize) -> Vec<u8> {
    let row_bytes = w / 4 * 5;
    assert_eq!(stride, row_bytes, "stride 必须正好装下一行 packed RAW10");
    let payload = stride * h;
    let mut b = vec![0u8; r10f::HDR_BYTES + payload + tail];
    b[..4].copy_from_slice(b"R10F");
    b[0x04] = 1; // version_major
    b[0x06] = 0; // version_minor
    b[0x08..0x0A].copy_from_slice(&(r10f::HDR_BYTES as u16).to_le_bytes());
    b[0x0A] = 0; // camera_id = A
    b[0x0B] = 0; // format = RAW
    b[0x0C] = 0; // codec = 非压缩
    b[0x1C..0x1E].copy_from_slice(&(w as u16).to_le_bytes());
    b[0x1E..0x20].copy_from_slice(&(h as u16).to_le_bytes());
    b[0x20..0x24].copy_from_slice(&(stride as u32).to_le_bytes());
    b[0x28] = 3; // RTL 的占位相位，解析器只当告警
    b[0x2A] = 10;
    b[0x2B] = 0;
    b[0x2C..0x30].copy_from_slice(&(payload as u32).to_le_bytes());
    b[0x30..0x34].copy_from_slice(&(payload as u32).to_le_bytes());
    for y in 0..h {
        let src = &plane[y * w..(y + 1) * w];
        let dst = &mut b[r10f::HDR_BYTES + y * stride..][..row_bytes];
        for (g, px) in dst.chunks_exact_mut(5).zip(src.chunks_exact(4)) {
            g[..4].copy_from_slice(px);
            g[4] = 0; // 10bit 的低 2 位补 0
        }
    }
    b
}

/// 各段耗时（毫秒）。同一遍迭代里的数值，不跨迭代拼。
#[derive(Default, Clone, Copy)]
struct Stages {
    parse: f64,
    pixel: f64,
    post: f64,
    nv12: f64,
    jpeg: f64,
}

impl Stages {
    fn total(&self) -> f64 {
        self.parse + self.pixel + self.post + self.nv12 + self.jpeg
    }
}

fn ms(t: &Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// 跑一整帧：解析 → 去马赛克（factor=1）或 CFA binning（factor>1）→ 后处理
/// → RGB→NV12 → 软件 JPEG。返回 (分段耗时, 输出宽, 输出高, JPEG 字节数)。
fn run_once(frame_bytes: &[u8], phase: BayerPhase, factor: usize) -> (Stages, usize, usize, usize) {
    let t = Instant::now();
    let f = r10f::parse(frame_bytes)
        .expect("R10F 解析失败")
        .expect("合成帧应当是 R10F");
    let mut s = Stages {
        parse: ms(&t),
        ..Default::default()
    };
    let (w, h) = (f.width, f.height);

    let t = Instant::now();
    let (mut rgb, ow, oh) = if factor > 1 {
        (
            bayer::bin_rgb(&f.raw, w, h, factor, phase).expect("CFA binning"),
            w / factor,
            h / factor,
        )
    } else {
        (
            bayer::demosaic(&f.raw, w, h, phase).expect("去马赛克"),
            w,
            h,
        )
    };
    s.pixel = ms(&t);

    // 板上 device/r10f 默认开 --auto：黑电平 12 / gamma 2.2 / AE 100 / AWB
    let mut fx = auto::AutoLevel::new(12, 2.2, 100, true);
    let t = Instant::now();
    fx.apply(&mut rgb, ow, 3);
    s.post = ms(&t);

    let (mut y, mut uv) = (vec![0u8; ow * oh], vec![0u8; ow * oh / 2]);
    let t = Instant::now();
    hwjpeg::rgb_to_nv12(&rgb, ow, oh, &mut y, &mut uv);
    s.nv12 = ms(&t);

    let t = Instant::now();
    let jpeg = encode::encode_rgb(&rgb, ow as u16, oh as u16, QUALITY).expect("JPEG 编码");
    s.jpeg = ms(&t);
    (s, ow, oh, jpeg.len())
}

/// 同一配置跑 1 遍预热 + ROUNDS 遍，取总耗时最短的那遍。
fn bench(frame_bytes: &[u8], phase: BayerPhase, factor: usize) -> (Stages, usize, usize, usize) {
    let mut best = run_once(frame_bytes, phase, factor);
    for _ in 0..ROUNDS {
        let r = run_once(frame_bytes, phase, factor);
        if r.0.total() < best.0.total() {
            best = r;
        }
    }
    best
}

/// 终端里的显示宽度（CJK 占两格），否则中文行名会把数字列顶歪
fn dwidth(s: &str) -> usize {
    s.chars()
        .map(|c| if (c as u32) >= 0x2E80 { 2 } else { 1 })
        .sum()
}

fn ljust(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(dwidth(s))))
}

fn rjust(s: &str, w: usize) -> String {
    format!("{}{s}", " ".repeat(w.saturating_sub(dwidth(s))))
}

fn row(name: &str, a: f64, b: f64, c: f64) -> String {
    format!(
        "{}{}{}{}",
        ljust(name, 30),
        rjust(&format!("{a:.2}"), 12),
        rjust(&format!("{b:.2}"), 13),
        rjust(&format!("{c:.2}"), 16)
    )
}

fn head(name: &str, a: &str, b: &str, c: &str) -> String {
    format!(
        "{}{}{}{}",
        ljust(name, 30),
        rjust(a, 12),
        rjust(b, 13),
        rjust(c, 16)
    )
}

#[test]
#[ignore]
fn bench_4k_pipeline() {
    assert!(
        !cfg!(debug_assertions),
        "这是性能基准，请用：cargo test --release bench_4k -- --ignored --nocapture"
    );
    let phase = BayerPhase::Rggb;

    let plane4k = synth_plane(W, H);
    let tail4k = FRAME4K_BYTES - r10f::HDR_BYTES - STRIDE * H;
    let bytes4k = r10f_bytes(&plane4k, W, H, STRIDE, tail4k);
    assert_eq!(
        bytes4k.len(),
        FRAME4K_BYTES,
        "128 B 头 + 10,368,000 B 载荷 + 2,944 B 尾 = 2532 × 4 KiB"
    );

    // 1080p 对照：与 README 记录的整帧 2,592,768 B 一致（128 + 2,592,000 + 640）
    let (pw, ph, pstride) = (1920usize, 1080usize, 2400usize);
    let bytes1080 = r10f_bytes(&synth_plane(pw, ph), pw, ph, pstride, 640);

    let (s1080, ow1080, oh1080, n1080) = bench(&bytes1080, phase, 1);
    let (sfull, owfull, ohfull, nfull) = bench(&bytes4k, phase, 1);
    let (sbin, owbin, ohbin, nbin) = bench(&bytes4k, phase, 2);

    println!();
    println!("4K 单路本地基准（x86_64，release，单线程；合成 R10F Bayer 噪声图）");
    println!(
        "{}",
        head("阶段 (ms)", "1080p 默认", "4K 全尺寸", "4K 2×2 binning")
    );
    for (name, a, b, c) in [
        ("R10F 解析 + 解包", s1080.parse, sfull.parse, sbin.parse),
        (
            "去马赛克 / CFA binning",
            s1080.pixel,
            sfull.pixel,
            sbin.pixel,
        ),
        (
            "后处理（黑/gamma/AE/AWB）",
            s1080.post,
            sfull.post,
            sbin.post,
        ),
        ("RGB→NV12", s1080.nv12, sfull.nv12, sbin.nv12),
        ("JPEG（软件，质量 80）", s1080.jpeg, sfull.jpeg, sbin.jpeg),
        (
            "合计（不含取帧/推流）",
            s1080.total(),
            sfull.total(),
            sbin.total(),
        ),
    ] {
        println!("{}", row(name, a, b, c));
    }
    println!(
        "{}",
        head(
            "输出尺寸",
            &format!("{ow1080}x{oh1080}"),
            &format!("{owfull}x{ohfull}"),
            &format!("{owbin}x{ohbin}")
        )
    );
    println!(
        "{}",
        head(
            "估算单路 fps (1000/合计)",
            &format!("{:.1}", 1000.0 / s1080.total()),
            &format!("{:.1}", 1000.0 / sfull.total()),
            &format!("{:.1}", 1000.0 / sbin.total())
        )
    );
    println!(
        "{}",
        head(
            "JPEG 字节数",
            &n1080.to_string(),
            &nfull.to_string(),
            &nbin.to_string()
        )
    );
    println!(
        "倍率比：4K 全尺寸 / binning = {:.2}×；binning / 1080p = {:.2}×",
        sfull.total() / sbin.total(),
        sbin.total() / s1080.total()
    );

    // 端到端验证「一条规则」：--scale 1（默认）下 4K 必须自动选 binning，
    // 1080p 必须保持全分辨率去马赛克。
    assert_eq!(scale::effective_factor(W, H, 1), 2);
    assert_eq!(scale::effective_factor(pw, ph, 1), 1);
    let f = r10f::parse(&bytes4k).unwrap().unwrap();
    let mut frame = Frame::new(f.width as u16, f.height as u16, f.raw);
    let mut fx = auto::AutoLevel::new(12, 2.2, 100, true);
    let t = Instant::now();
    let (jpeg, ow, oh) = encode_frame(
        &mut frame,
        phase,
        QUALITY,
        1,
        &mut fx,
        None,
        &mut YuvScratch::default(),
        &StageTimer::default(),
        None,
    )
    .expect("4K 默认倍率编码");
    println!(
        "encode_frame(4K, --scale 1) → {ow}x{oh}，{:.1} ms，JPEG {} B（含软件 JPEG）",
        ms(&t),
        jpeg.len()
    );
    assert_eq!(
        (ow as usize, oh as usize),
        (W / 2, H / 2),
        "4K 默认必须自动 2×2 binning"
    );
    println!();
}
