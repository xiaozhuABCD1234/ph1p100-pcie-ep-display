//! RKNN YOLO11n 检测运行时。
//!
//! 每个检测句柄 = 一个 RKNN context + 一个 worker 线程 + 一个只保留最新任务的
//! mailbox。编码线程只做 letterbox、提交和叠框，NPU 推理在 worker 里，
//! 因此检测慢不会把 JPEG 编码阻塞；跟不上时覆盖旧任务，延迟有界。
//!
//! 支持两种模型契约：
//!   * 单输出 [1,84,N]：旧模型，但整张输出共用一个 int8 scale，类别分数容易被
//!     框坐标的 0..640 尺度压成 0，只建议 FP16 使用；
//!   * 双输出 boxes [1,4,N] + scores [1,80,N]：推荐，分开量化，类别分数保留。

mod labels;
mod overlay;
mod rknn_sys;
mod yolo;

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rknn_sys::{RknnApi, RknnContext, RknnTensorAttr};
use yolo::{
    decode_yolo11, decode_yolo11_split, letterbox_rgb_nhwc, Detection, Letterbox, CHANNELS,
    NUM_CLASSES,
};

pub const CORE_A: u32 = rknn_sys::RKNN_NPU_CORE_0;
pub const CORE_B: u32 = rknn_sys::RKNN_NPU_CORE_1;

const STALE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Both,
    A,
    B,
}

/// 一个待推理任务。RGB 已经 letterbox 成模型输入尺寸的 NHWC uint8。
struct Job {
    input: Vec<u8>,
    lb: Letterbox,
}

struct DetectionSet {
    boxes: Vec<Detection>,
    at: Instant,
}

enum OutputSpec {
    Single {
        dims: Vec<u32>,
        elems: usize,
    },
    Split {
        box_dims: Vec<u32>,
        box_elems: usize,
        score_dims: Vec<u32>,
        score_elems: usize,
    },
}

#[derive(Default)]
struct Stats {
    submitted: AtomicU64,
    ran: AtomicU64,
    dropped: AtomicU64,
    errors: AtomicU64,
    boxes: AtomicU64,
    last_infer_us: AtomicU64,
    disabled: AtomicBool,
}

struct Shared {
    slot: Mutex<Option<Job>>,
    cv: Condvar,
    result: Mutex<Option<DetectionSet>>,
    last_submit: Mutex<Option<Instant>>,
    min_interval: Duration,
    overlay: bool,
    input_w: u32,
    input_h: u32,
    out: OutputSpec,
    conf: f32,
    nms: f32,
    stats: Stats,
}

/// 检测句柄，可 clone 给 A/B 编码线程。内部只有 Arc 与原子量。
#[derive(Clone)]
pub struct DetectHandle {
    shared: Arc<Shared>,
}

impl DetectHandle {
    /// 从当前 RGB 生成 letterbox 输入并提交。按 detect_fps 限频；
    /// mailbox 已有未处理任务时覆盖它并记 dropped。
    pub fn submit(&self, rgb: &[u8], w: u32, h: u32) {
        if self.shared.stats.disabled.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        {
            let mut last = self
                .shared
                .last_submit
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(t) = *last {
                if now.duration_since(t) < self.shared.min_interval {
                    return;
                }
            }
            *last = Some(now);
        }
        let (input, lb) =
            match letterbox_rgb_nhwc(rgb, w, h, self.shared.input_w, self.shared.input_h) {
                Ok(v) => v,
                Err(e) => {
                    self.shared.stats.errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("检测预处理失败：{e}");
                    return;
                }
            };
        let mut g = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some() {
            self.shared.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
        *g = Some(Job { input, lb });
        self.shared.stats.submitted.fetch_add(1, Ordering::Relaxed);
        drop(g);
        self.shared.cv.notify_one();
    }

    /// 把最新检测结果画到当前 RGB 上；超过 500ms 的旧结果不画。
    pub fn overlay(&self, rgb: &mut [u8], w: usize, h: usize) {
        if !self.shared.overlay {
            return;
        }
        let boxes = {
            let g = self.shared.result.lock().unwrap_or_else(|e| e.into_inner());
            match g.as_ref() {
                Some(r) if r.at.elapsed() <= STALE => Some(r.boxes.clone()),
                _ => None,
            }
        };
        if let Some(b) = boxes {
            overlay::draw_detections(rgb, w, h, &b);
        }
    }

    pub fn status(&self) -> String {
        let s = &self.shared.stats;
        if s.disabled.load(Ordering::Relaxed) {
            return "检测已禁用（运行时连续失败）".into();
        }
        format!(
            "提交 {} / 推理 {} / 丢弃 {} / 错误 {} / 最近 {:.1}ms / 框 {}",
            s.submitted.load(Ordering::Relaxed),
            s.ran.load(Ordering::Relaxed),
            s.dropped.load(Ordering::Relaxed),
            s.errors.load(Ordering::Relaxed),
            s.last_infer_us.load(Ordering::Relaxed) as f64 / 1000.0,
            s.boxes.load(Ordering::Relaxed),
        )
    }
}

/// 启动一个检测 worker。模型加载/契约检查在调用线程完成，失败直接返回。
pub fn start(
    model_path: &Path,
    conf: f32,
    nms: f32,
    detect_fps: u32,
    core_mask: u32,
    draw_overlay: bool,
) -> Result<DetectHandle, String> {
    if !(0.0..=1.0).contains(&conf) || !(0.0..=1.0).contains(&nms) {
        return Err("conf/nms 必须在 0..1".into());
    }
    let model = fs::read(model_path)
        .map_err(|e| format!("读 RKNN 模型 {} 失败：{e}", model_path.display()))?;
    let api = RknnApi::load()?;
    let ctx = RknnContext::init(api, &model, core_mask)?;
    let io = ctx.io_num()?;
    if io.n_input != 1 || !(1..=2).contains(&io.n_output) {
        return Err(format!(
            "模型要求 1 输入/1..2 输出，实际 {} 输入/{} 输出",
            io.n_input, io.n_output
        ));
    }
    let in_attr = ctx.input_attr(0)?;
    let (input_w, input_h, input_c) = parse_input_hw(&in_attr)?;
    if input_c != 3 {
        return Err(format!("模型输入通道 {input_c}，只支持 RGB 3 通道"));
    }

    let (out, out_desc) = match io.n_output {
        1 => {
            let a = ctx.output_attr(0)?;
            let (dims, elems) = parse_channels(&a, CHANNELS, "output0")?;
            let desc = format!("单输出 {:?}（{} 元素）", dims, elems);
            (OutputSpec::Single { dims, elems }, desc)
        }
        2 => {
            let b = ctx.output_attr(0)?;
            let s = ctx.output_attr(1)?;
            let (box_dims, box_elems) = parse_channels(&b, 4, "boxes")?;
            let (score_dims, score_elems) = parse_channels(&s, NUM_CLASSES, "scores")?;
            let desc = format!(
                "双输出 boxes {:?}（{} 元素）+ scores {:?}（{} 元素）",
                box_dims, box_elems, score_dims, score_elems
            );
            (
                OutputSpec::Split {
                    box_dims,
                    box_elems,
                    score_dims,
                    score_elems,
                },
                desc,
            )
        }
        _ => unreachable!(),
    };
    let core_name = match core_mask {
        CORE_A => "0".to_string(),
        CORE_B => "1".to_string(),
        other => format!("0x{other:x}"),
    };
    println!(
        "  检测  ：{}，{}x{} 输入，{}，NPU core {}",
        model_path.display(),
        input_w,
        input_h,
        out_desc,
        core_name
    );

    let shared = Arc::new(Shared {
        slot: Mutex::new(None),
        cv: Condvar::new(),
        result: Mutex::new(None),
        last_submit: Mutex::new(None),
        min_interval: Duration::from_micros(1_000_000 / detect_fps.max(1) as u64),
        overlay: draw_overlay,
        input_w,
        input_h,
        out,
        conf,
        nms,
        stats: Stats::default(),
    });
    let worker_shared = Arc::clone(&shared);
    thread::Builder::new()
        .name(format!("detect-core{core_name}"))
        .spawn(move || worker(ctx, worker_shared))
        .map_err(|e| format!("起检测 worker 失败：{e}"))?;
    Ok(DetectHandle { shared })
}

fn worker(ctx: RknnContext, shared: Arc<Shared>) {
    let (mut out0, mut out1) = match &shared.out {
        OutputSpec::Single { elems, .. } => (Vec::with_capacity(*elems), Vec::new()),
        OutputSpec::Split {
            box_elems,
            score_elems,
            ..
        } => (
            Vec::with_capacity(*box_elems),
            Vec::with_capacity(*score_elems),
        ),
    };
    let mut consecutive_errors = 0u32;
    loop {
        let job = {
            let mut g = shared.slot.lock().unwrap_or_else(|e| e.into_inner());
            while g.is_none() {
                let (ng, _) = shared
                    .cv
                    .wait_timeout(g, Duration::from_millis(200))
                    .unwrap_or_else(|e| e.into_inner());
                g = ng;
            }
            g.take().expect("循环条件保证有任务")
        };
        let t = Instant::now();
        let run = match &shared.out {
            OutputSpec::Single { elems, .. } => ctx.run(&job.input, &mut out0, *elems),
            OutputSpec::Split {
                box_elems,
                score_elems,
                ..
            } => ctx.run2(
                &job.input,
                &mut out0,
                *box_elems,
                &mut out1,
                *score_elems,
            ),
        };
        match run {
            Ok(()) => {
                let decoded = match &shared.out {
                    OutputSpec::Single { dims, .. } => {
                        decode_yolo11(&out0, dims, &job.lb, shared.conf, shared.nms)
                    }
                    OutputSpec::Split {
                        box_dims,
                        score_dims,
                        ..
                    } => decode_yolo11_split(
                        &out0,
                        &out1,
                        box_dims,
                        score_dims,
                        &job.lb,
                        shared.conf,
                        shared.nms,
                    ),
                };
                match decoded {
                    Ok(boxes) => {
                        consecutive_errors = 0;
                        shared
                            .stats
                            .last_infer_us
                            .store(t.elapsed().as_micros() as u64, Ordering::Relaxed);
                        shared.stats.ran.fetch_add(1, Ordering::Relaxed);
                        shared
                            .stats
                            .boxes
                            .store(boxes.len() as u64, Ordering::Relaxed);
                        let mut g = shared.result.lock().unwrap_or_else(|e| e.into_inner());
                        *g = Some(DetectionSet {
                            boxes,
                            at: Instant::now(),
                        });
                    }
                    Err(e) => {
                        shared.stats.errors.fetch_add(1, Ordering::Relaxed);
                        eprintln!("检测后处理失败：{e}");
                    }
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                shared.stats.errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("检测推理失败（第 {consecutive_errors} 次）：{e}");
                if consecutive_errors >= 5 {
                    shared.stats.disabled.store(true, Ordering::Relaxed);
                    eprintln!("检测连续失败，已禁用（视频继续，不再叠框）");
                    return;
                }
            }
        }
    }
}

fn dims_product(dims: &[u32]) -> usize {
    dims.iter()
        .fold(1usize, |a, &b| a.saturating_mul(b as usize))
}

fn parse_input_hw(a: &RknnTensorAttr) -> Result<(u32, u32, u32), String> {
    let d = a.dims();
    if d.len() != 4 {
        return Err(format!("输入 dims 不是 4 维：{:?}", d));
    }
    let (c, h, w) = match a.fmt {
        rknn_sys::RKNN_TENSOR_NCHW => (d[1], d[2], d[3]),
        rknn_sys::RKNN_TENSOR_NHWC => (d[3], d[1], d[2]),
        _ => {
            if d[1] == 3 {
                (d[1], d[2], d[3])
            } else if d[3] == 3 {
                (d[3], d[1], d[2])
            } else {
                return Err(format!("无法判断输入通道维：dims={d:?}, fmt={}", a.fmt));
            }
        }
    };
    Ok((w, h, c))
}

fn parse_channels(
    a: &RknnTensorAttr,
    channels: usize,
    name: &str,
) -> Result<(Vec<u32>, usize), String> {
    let dims = a.dims().to_vec();
    let shape: Vec<u32> = dims.iter().copied().filter(|&d| d != 1).collect();
    let ok = matches!(shape.as_slice(), [c, _] if *c as usize == channels)
        || matches!(shape.as_slice(), [_, c] if *c as usize == channels);
    if !ok {
        return Err(format!(
            "{name} shape 只支持 [{channels},N] 或 [N,{channels}]，实际 dims={dims:?}"
        ));
    }
    let n = a.n_elems as usize;
    let n = if n == 0 { dims_product(&dims) } else { n };
    Ok((dims, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(dims: &[u32], fmt: u32) -> RknnTensorAttr {
        let mut a = RknnTensorAttr::default();
        a.n_dims = dims.len() as u32;
        a.fmt = fmt;
        a.n_elems = dims_product(dims) as u32;
        a.dims[..dims.len()].copy_from_slice(dims);
        a
    }

    #[test]
    fn parses_nchw_input() {
        let a = attr(&[1, 3, 640, 640], rknn_sys::RKNN_TENSOR_NCHW);
        assert_eq!(parse_input_hw(&a).unwrap(), (640, 640, 3));
    }

    #[test]
    fn parses_nhwc_input() {
        let a = attr(&[1, 640, 640, 3], rknn_sys::RKNN_TENSOR_NHWC);
        assert_eq!(parse_input_hw(&a).unwrap(), (640, 640, 3));
    }

    #[test]
    fn parses_single_yolo_output() {
        let a = attr(&[1, 84, 8400], rknn_sys::RKNN_TENSOR_NCHW);
        let (d, n) = parse_channels(&a, CHANNELS, "output0").unwrap();
        assert_eq!(d, vec![1, 84, 8400]);
        assert_eq!(n, 84 * 8400);
    }

    #[test]
    fn parses_split_outputs() {
        let b = attr(&[1, 4, 8400], rknn_sys::RKNN_TENSOR_NCHW);
        let s = attr(&[1, 80, 8400], rknn_sys::RKNN_TENSOR_NCHW);
        assert_eq!(parse_channels(&b, 4, "boxes").unwrap().1, 4 * 8400);
        assert_eq!(
            parse_channels(&s, NUM_CLASSES, "scores").unwrap().1,
            NUM_CLASSES * 8400
        );
    }

    #[test]
    fn rejects_unsupported_output() {
        let a = attr(&[1, 85, 8400], rknn_sys::RKNN_TENSOR_NCHW);
        assert!(parse_channels(&a, CHANNELS, "output0").is_err());
    }
}
