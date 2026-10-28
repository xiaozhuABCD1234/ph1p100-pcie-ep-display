//! display-stream —— RK3588 上「取帧 → JPEG → HTTP 推流」。
//!
//! 数据源三选一（`--source`）：合成源、PGM 文件源、驱动设备源。
//! 帧缓存里是 Bayer 马赛克时用 `--bayer <相位>` 做去马赛克，输出彩色；
//! `--bayer gray` 保持单分量灰度输出。默认 **bggr**，与本板实测 R10F 一致。
//! 推流接口两个：MJPEG 连续流与单帧 JPEG 快照（见 `--help`）。

mod auto;
mod bayer;
mod driver;
mod encode;
mod http;
mod r10f;
mod scale;
mod source;
mod store;

use bayer::BayerPhase;
use source::{
    device::DeviceSource, pgm::PgmFile, synthetic::Synthetic, Frame, FrameSource, SourceError,
};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 退出码：1 用法错、2 数据源起不来、3 服务起不来、4 设备连续 IO 失败
const EX_USAGE: u8 = 1;
const EX_SOURCE: u8 = 2;
const EX_SERVE: u8 = 3;
const EX_DEVICE: u8 = 4;

const DEF_DEV: &str = "/dev/ANLOGIC-PCI0_frame";
/// 端口默认取不常用的高位端口，避开 8080 这类常被占用的
const DEF_PORT: u16 = 8090;
const DEF_FPS: u32 = 15;
const DEF_QUALITY: u8 = 80;
const DEF_WAIT_MS: u32 = 2000;
/// Bayer 相位默认 **`auto`**：当前安装 A/B 均为 RGGB。
///
/// 已确证的部分（不要再从 R10F 头 0x28 抄相位）：
///   * 真实帧 2×2 均值显示绿在**反对角** (0,1)/(1,0)（两条绿对角差 0.07%，
///     主对角差 13.5%），所以只可能是 RGGB 或 BGGR；header 0x28=3(GBRG) 是
///     `SRC_BAYER_PHASE=0` 占位按 ROI 奇偶推导的，必然错。
///   * A/B 两路看同一间办公室：只有 RGGB 让两边告示牌都还原成黄色；BGGR 会把
///     它们都变蓝。故当前两块模组都按 **RGGB**（早期 BGGR 结论来自翻转预览）。
///   * 若换模组/改支架/改 FPGA 像素对齐，必须用已知色卡重测；
///     `--bayer rggb|bggr` 可随时覆盖。
const DEF_BAYER: BayerPhase = BayerPhase::Auto;
/// `--auto` 预设的四个值：
///  * 黑电平 12 = IMX415 的 10bit black pedestal 50 截成 8 位（`50 >> 2`），板上实测；
///  * gamma 2.2 = 常规 sRGB 近似；
///  * AE 目标 100 = 中位数落在中灰附近，不把画面拉满；
///  * 白平衡默认开（只有彩色相位下才有意义）。
const DEF_BLACK: u8 = 12;
const DEF_GAMMA: f32 = 2.2;
const DEF_AE: u8 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SourceKind {
    Synthetic,
    Pgm,
    R10f,
    Device,
}

#[derive(Debug)]
struct Config {
    kind: SourceKind,
    pgm: Option<PathBuf>,
    r10f: Option<PathBuf>,
    device: PathBuf,
    width: u16,
    height: u16,
    fps: u32,
    quality: u8,
    /// 编码前盒式降采样倍率（1 = 不缩；彩色通路先全分辨率去马赛克再缩 RGB）
    scale: u32,
    bayer: BayerPhase,
    /// 一键打开「黑电平 + gamma + AE + AWB」（单项可用下面的选项覆盖）
    auto: bool,
    /// 黑电平（10bit pedestal 截成 8 位的值），0 = 不扣
    black: u8,
    /// gamma 值，1.0 = 不校正
    gamma: f32,
    /// AE 目标（gamma 编码后的中位数亮度），0 = 关
    ae_target: u8,
    /// 灰世界白平衡开关（只在彩色相位下有效）
    awb: bool,
    bind: IpAddr,
    port: u16,
    wait_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            kind: SourceKind::Synthetic,
            pgm: None,
            r10f: None,
            device: PathBuf::from(DEF_DEV),
            width: 640,
            height: 480,
            fps: DEF_FPS,
            quality: DEF_QUALITY,
            scale: 1,
            bayer: DEF_BAYER,
            auto: false,
            black: 0,
            gamma: 1.0,
            ae_target: 0,
            awb: false,
            bind: IpAddr::from([0, 0, 0, 0]),
            port: DEF_PORT,
            wait_ms: DEF_WAIT_MS,
        }
    }
}

fn usage() -> String {
    format!(
        "display-stream —— 取帧 → JPEG → HTTP 推流\n\
         \n\
         用法：display-stream [选项]\n\
         \n\
         --source <synthetic|pgm|r10f|device>  数据源，默认 synthetic\n\
         --pgm <路径>                     文件源的 PGM（P5/P2 灰度图）\n\
         --r10f <路径>                    R10F 文件源（.dma，从魔数起；离线回放）\n\
         --device <路径>                  设备源的收帧缓冲节点，默认 {DEF_DEV}\n\
         --width <像素>                   合成源宽度，默认 640\n\
         --height <像素>                  合成源高度，默认 480\n\
         --fps <帧/秒>                    推流节奏，默认 {DEF_FPS}\n\
         --quality <1..100>               JPEG 质量，默认 {DEF_QUALITY}\n\
         --scale <1|2|4|8>               编码前降采样 1/N，默认 1（不缩）。灰度走盒式\n\
                                          均值；彩色走 CFA binning（2×2 块内 R/G/G/B\n\
                                          各一个采样，去马赛克与降采样一步完成）\n\
         --bayer <auto|gray|rggb|bggr|grbg|gbrg>  帧缓存里 Bayer 马赛克的相位，\n\
                                          默认 {DEF_BAYER}：当前安装 A/B 均 RGGB；\n\
                                          gray = 不去马赛克、单分量灰度；\n\
                                          非 gray 时按该相位双线性去马赛克，\n\
                                          输出三分量彩色 JPEG\n\
                                          注意：R10F 头 0x28 的 3(GBRG) 是占位值，\n\
                                          不能直接拿来当相位\n\
         --auto                           一键打开后处理：黑电平 -{DEF_BLACK}、\n\
                                          gamma {DEF_GAMMA}、AE 目标 {DEF_AE}、白平衡开；\n\
                                          默认：device/r10f 开，synthetic/pgm 关；\n\
                                          下面四项可以单独覆盖（与 --auto 的先后无关）\n\
         --black <0..64>                  黑电平（IMX415 的 10bit pedestal 50\n\
                                          截成 8 位就是 12），默认：--auto 时 {DEF_BLACK}，否则 0\n\
         --gamma <1.0..4.0>               gamma，默认：--auto 时 {DEF_GAMMA}，否则 1.0\n\
         --ae <0..250>                    AE 目标（gamma 编码后的中位数亮度），0 = 关；\n\
                                          默认：--auto 时 {DEF_AE}，否则 0\n\
         --awb / --no-awb                 白点白平衡（只在彩色相位下有效），\n\
                                          取最亮且未过曝的像素作白参考；\n\
                                          默认：--auto 时开，否则关\n\
         --port <端口>                    监听端口，默认 {DEF_PORT}\n\
         --bind <地址>                    监听地址，默认 0.0.0.0\n\
         --timeout-ms <毫秒>              设备源等帧超时，默认 {DEF_WAIT_MS}\n\
         -h, --help                       显示本帮助\n\
         \n\
         接口：/             网页（内嵌 MJPEG 流）\n\
               /stream.mjpg   MJPEG 连续流（multipart/x-mixed-replace）\n\
               /snapshot.jpg  单帧 JPEG 快照\n",
        DEF_BAYER = DEF_BAYER.as_str()
    )
}

enum Parsed {
    Help,
    Run(Box<Config>),
}

fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut cfg = Config::default();
    let mut i = 0usize;
    // 后处理四项的“显式给过吗”：None = 跟着 --auto 的预设走
    let (mut o_black, mut o_gamma, mut o_ae, mut o_awb, mut o_auto) = (
        None::<u8>,
        None::<f32>,
        None::<u8>,
        None::<bool>,
        None::<bool>,
    );
    // 取值辅助：拿下一个参数
    fn value(args: &[String], i: &mut usize, name: &str) -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("{name} 后面缺参数"))
    }
    while i < args.len() {
        let name = args[i].clone();
        match name.as_str() {
            "--source" => {
                let v = value(args, &mut i, &name)?;
                cfg.kind = match v.as_str() {
                    "synthetic" => SourceKind::Synthetic,
                    "pgm" => SourceKind::Pgm,
                    "r10f" => SourceKind::R10f,
                    "device" => SourceKind::Device,
                    other => {
                        return Err(format!(
                            "--source 只认 synthetic/pgm/r10f/device，收到 '{other}'"
                        ))
                    }
                };
            }
            "--pgm" => cfg.pgm = Some(PathBuf::from(value(args, &mut i, &name)?)),
            "--r10f" => cfg.r10f = Some(PathBuf::from(value(args, &mut i, &name)?)),
            "--device" => cfg.device = PathBuf::from(value(args, &mut i, &name)?),
            "--width" => cfg.width = num(&value(args, &mut i, &name)?, &name, 1, 65535)? as u16,
            "--height" => cfg.height = num(&value(args, &mut i, &name)?, &name, 1, 65535)? as u16,
            "--fps" => cfg.fps = num(&value(args, &mut i, &name)?, &name, 1, 240)?,
            "--quality" => cfg.quality = num(&value(args, &mut i, &name)?, &name, 1, 100)? as u8,
            "--scale" => {
                // 只允许 2 的幂：彩色通路的 CFA binning 要求块边界与 2×2 相位对齐
                let v = num(&value(args, &mut i, &name)?, &name, 1, 8)?;
                if !matches!(v, 1 | 2 | 4 | 8) {
                    return Err(format!("--scale 只认 1/2/4/8（2 的幂），收到 {v}"));
                }
                cfg.scale = v;
            }
            "--bayer" => {
                let v = value(args, &mut i, &name)?;
                cfg.bayer = BayerPhase::parse(&v)
                    .ok_or_else(|| format!("--bayer 只认 gray/rggb/bggr/grbg/gbrg，收到 '{v}'"))?;
            }
            // 后处理：`--auto` 是预设，四个单项选项可以覆盖它（解析顺序无关，
            // 先在 Option 里记下“显式给过没有”，循环结束后再一次性解算）。
            "--auto" => {
                cfg.auto = true;
                o_auto = Some(true);
            }
            "--no-auto" => {
                cfg.auto = false;
                o_auto = Some(false);
            }
            "--black" => o_black = Some(num(&value(args, &mut i, &name)?, &name, 0, 64)? as u8),
            "--gamma" => o_gamma = Some(fnum(&value(args, &mut i, &name)?, &name, 1.0, 4.0)?),
            "--ae" => o_ae = Some(num(&value(args, &mut i, &name)?, &name, 0, 250)? as u8),
            "--awb" => o_awb = Some(true),
            "--no-awb" => o_awb = Some(false),
            "--port" => cfg.port = num(&value(args, &mut i, &name)?, &name, 1, 65535)? as u16,
            "--timeout-ms" => {
                let v = value(args, &mut i, &name)?;
                cfg.wait_ms = num(&v, &name, 1, driver::ANLOGIC_FRAME_MAX_WAIT_MS)?;
            }
            "--bind" => {
                let v = value(args, &mut i, &name)?;
                cfg.bind = v
                    .parse()
                    .map_err(|_| format!("--bind 不是合法 IP 地址（收到 '{v}'）"))?;
            }
            "-h" | "--help" => return Ok(Parsed::Help),
            other => return Err(format!("不认识的参数 '{other}'")),
        }
        i += 1;
    }
    if cfg.kind == SourceKind::Pgm && cfg.pgm.is_none() {
        return Err("--source pgm 必须配 --pgm <路径>".into());
    }
    if cfg.kind == SourceKind::R10f && cfg.r10f.is_none() {
        return Err("--source r10f 必须配 --r10f <路径>".into());
    }
    // 后处理解算：--auto/--no-auto 显式优先；否则 device/r10f 默认开，
    // synthetic/pgm 默认关（避免影响算法/灰度用例）。
    let preset = o_auto.unwrap_or(matches!(cfg.kind, SourceKind::Device | SourceKind::R10f));
    cfg.auto = preset;
    cfg.black = o_black.unwrap_or(if preset { DEF_BLACK } else { 0 });
    cfg.gamma = o_gamma.unwrap_or(if preset { DEF_GAMMA } else { 1.0 });
    cfg.ae_target = o_ae.unwrap_or(if preset { DEF_AE } else { 0 });
    cfg.awb = o_awb.unwrap_or(preset);
    // 合成源的尺寸闸：乘积也要管，不能只限单个维度（65535×65535 要 4 GiB）
    if cfg.kind == SourceKind::Synthetic
        && cfg.width as u64 * cfg.height as u64 > source::MAX_FRAME_BYTES as u64
    {
        return Err(format!(
            "合成源几何 {}x{} 超过上限 {} 字节/帧（--width × --height 太大）",
            cfg.width,
            cfg.height,
            source::MAX_FRAME_BYTES
        ));
    }
    Ok(Parsed::Run(Box::new(cfg)))
}

fn num(s: &str, name: &str, lo: u32, hi: u32) -> Result<u32, String> {
    let v: u32 = s
        .parse()
        .map_err(|_| format!("{name} 需要一个 {lo}..{hi} 的整数，收到 '{s}'"))?;
    if !(lo..=hi).contains(&v) {
        return Err(format!("{name} 超出范围（{lo}..{hi}），收到 {v}"));
    }
    Ok(v)
}

fn fnum(s: &str, name: &str, lo: f32, hi: f32) -> Result<f32, String> {
    let v: f32 = s
        .parse()
        .map_err(|_| format!("{name} 需要一个 {lo}..{hi} 的小数，收到 '{s}'"))?;
    if !v.is_finite() || v < lo || v > hi {
        return Err(format!("{name} 超出范围（{lo}..{hi}），收到 {v}"));
    }
    Ok(v)
}

/// 按配置造数据源。失败就如实报错退出，不起半个服务。
fn build_source(cfg: &Config) -> Result<Box<dyn FrameSource>, String> {
    match cfg.kind {
        SourceKind::Synthetic => Ok(Box::new(Synthetic::new(cfg.width, cfg.height))),
        SourceKind::Pgm => {
            let path = cfg.pgm.clone().expect("parse_args 已校验");
            let src = PgmFile::open(&path).map_err(|e| format!("打开 PGM 失败：{e}"))?;
            Ok(Box::new(src))
        }
        SourceKind::R10f => {
            let path = cfg.r10f.clone().expect("parse_args 已校验");
            let src = source::r10f_file::R10fFile::open(&path)
                .map_err(|e| format!("打开 R10F 失败：{e}"))?;
            Ok(Box::new(src))
        }
        SourceKind::Device => {
            let src = DeviceSource::open(&cfg.device, cfg.wait_ms)
                .map_err(|e| format!("打开收帧缓冲节点 {} 失败：{e}", cfg.device.display()))?;
            Ok(Box::new(src))
        }
    }
}

/// 一帧 → JPEG。`gray` 相位走单分量灰度通路；其余相位先按该相位对整帧做
/// 双线性去马赛克，再编三分量彩色 JPEG。返回 `(JPEG 字节, 输出宽, 输出高)` ——
/// 开了 `--scale` 时输出比源图小，调用方必须用返回值而不是源帧几何。
///
/// 后处理（黑电平/gamma/AE/AWB）就地作用在**送去编码的那张图上**：
///   * 灰度通路：先缩（若开），再后处理 —— 后处理逐像素，先缩省 3/4 的统计与查表；
///   * 彩色通路：要缩就**直接 CFA binning**（`bayer::bin_rgb`），不缩才全分辨率去马赛克。
///     实测 4K 彩色 327ms/帧的绝大部分是那条 25MB 的 RGB 中间缓冲，而不是 JPEG；
///     binning 把「去马赛克 + 降采样」合成一步，省掉整条全分辨率通路。
///     **不能先缩 Bayer 再普通去马赛克**：那等于把不同滤色片的采样当邻居插值。
fn encode_frame(
    frame: &mut Frame,
    phase: BayerPhase,
    quality: u8,
    scale: u32,
    fx: &mut auto::AutoLevel,
) -> Result<(Vec<u8>, u16, u16), String> {
    // auto 必须在拿到帧之后解析：A/B 两路的 R/B 顺序不同，R10F header 自己不可信。
    let phase = phase.resolve(frame.camera_id);
    let (w, h) = (frame.width as usize, frame.height as usize);
    let factor = scale as usize;

    if phase.is_gray() {
        let mut scaled: Option<Vec<u8>> = None;
        let (ow, oh) = match scale::out_dims(w, h, factor) {
            Some((ow, oh)) => {
                scaled = Some(scale::box_gray(&frame.gray, w, h, factor));
                (ow, oh)
            }
            None => (w, h),
        };
        let buf: &mut [u8] = match scaled.as_mut() {
            Some(b) => b,
            None => &mut frame.gray,
        };
        fx.apply(buf, ow, 1);
        let jpeg =
            encode::encode_gray(buf, ow as u16, oh as u16, quality).map_err(|e| e.to_string())?;
        return Ok((jpeg, ow as u16, oh as u16));
    }

    let (mut rgb, ow, oh) = match scale::out_dims(w, h, factor) {
        Some((ow, oh)) => (bayer::bin_rgb(&frame.gray, w, h, factor, phase)?, ow, oh),
        None => (bayer::demosaic(&frame.gray, w, h, phase)?, w, h),
    };
    fx.apply(&mut rgb, ow, 3);
    let jpeg =
        encode::encode_rgb(&rgb, ow as u16, oh as u16, quality).map_err(|e| e.to_string())?;
    Ok((jpeg, ow as u16, oh as u16))
}

/// 取帧线程：取一帧 → 编码 → 放进共享区，按 fps 控节奏；
/// 每 5 秒往标准错误打一条状态（无新帧、编码耗时、驱动序号）。
fn produce(mut src: Box<dyn FrameSource>, store: Arc<store::FrameStore>, cfg: Config) {
    let period = Duration::from_micros(1_000_000 / cfg.fps as u64);
    let mut published: u64 = 0;
    let mut encode_us: u64 = 0;
    let mut last_log = Instant::now();
    let mut io_fails: u32 = 0;
    // 后处理器带着时域状态（AE/AWB 的滑动平均）跨帧活着
    let mut fx = auto::AutoLevel::new(cfg.black, cfg.gamma, cfg.ae_target, cfg.awb);

    loop {
        let t0 = Instant::now();
        match src.next_frame() {
            Ok(mut frame) => {
                io_fails = 0;
                let t_enc = Instant::now();
                let (jpeg, out_w, out_h) =
                    match encode_frame(&mut frame, cfg.bayer, cfg.quality, cfg.scale, &mut fx) {
                        Ok(j) => j,
                        Err(e) => {
                            eprintln!("编码失败（{e}），跳过这一帧");
                            continue;
                        }
                    };
                encode_us += t_enc.elapsed().as_micros() as u64;
                let stream_seq = store.publish(jpeg, out_w, out_h);
                published += 1;

                if last_log.elapsed() >= Duration::from_secs(5) {
                    let extra = match src.status() {
                        Some(s) => format!("，{s}"),
                        None => String::new(),
                    };
                    eprintln!(
                        "已推 {published} 帧（推流序号 {stream_seq}）；最近一帧 {}x{}，\
                         平均编码 {:.2}ms；{}{extra}",
                        out_w,
                        out_h,
                        encode_us as f64 / 1000.0 / published.max(1) as f64,
                        fx.status(),
                    );
                    last_log = Instant::now();
                }
            }
            Err(SourceError::Timeout(why)) => {
                io_fails = 0;
                if last_log.elapsed() >= Duration::from_secs(5) {
                    eprintln!("暂时没有新帧：{why}");
                    last_log = Instant::now();
                }
            }
            Err(SourceError::Io(e)) => {
                io_fails += 1;
                eprintln!("取帧 IO 失败（第 {io_fails} 次）：{e}");
                if io_fails >= 10 {
                    eprintln!("设备连续 {io_fails} 次 IO 失败，退出（不静默空转）");
                    std::process::exit(EX_DEVICE as i32);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }

        let spent = t0.elapsed();
        if spent < period {
            std::thread::sleep(period - spent);
        }
    }
}

/// 后处理参数的一行式描述，给启动日志用
fn postfx_desc(cfg: &Config) -> String {
    if cfg.black == 0 && (cfg.gamma - 1.0).abs() < 1e-6 && cfg.ae_target == 0 && !cfg.awb {
        return "关（原样透传）".to_string();
    }
    format!(
        "黑电平 -{}，gamma {:.2}，AE 目标 {}，白平衡 {}",
        cfg.black,
        cfg.gamma,
        if cfg.ae_target == 0 {
            "关".to_string()
        } else {
            cfg.ae_target.to_string()
        },
        if cfg.awb { "开" } else { "关" }
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match parse_args(&args) {
        Ok(Parsed::Help) => {
            print!("{}", usage());
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Run(c)) => *c,
        Err(e) => {
            eprintln!("参数错误：{e}\n");
            eprint!("{}", usage());
            return ExitCode::from(EX_USAGE);
        }
    };

    println!("display-stream 启动");
    let src = match build_source(&cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("数据源起不来：{e}");
            return ExitCode::from(EX_SOURCE);
        }
    };
    // 启动自检：拿一帧定值小图走一遍真实编码路径，确认分量数与相位设置一致
    // （gray → 1 个分量，彩色相位 → 3 个分量），避免设置与输出对不上。
    let want = if cfg.bayer.is_gray() { 1 } else { 3 };
    // 自检只看编码器：后处理器这里用恒等参数，免得自检图被 gamma/AE 改写
    let mut probe = Frame::new(4, 1, vec![0, 64, 128, 255]);
    let mut probe_fx = auto::AutoLevel::new(0, 1.0, 0, false);
    match encode_frame(&mut probe, cfg.bayer, cfg.quality, cfg.scale, &mut probe_fx).and_then(
        |(j, _, _)| {
            encode::jpeg_component_count(&j).ok_or_else(|| "输出里找不到 SOF 段".to_string())
        },
    ) {
        Ok(n) if n == want => {
            if cfg.bayer.is_gray() {
                println!("  自检  ：编码器输出单分量（灰度）JPEG，符合预期");
            } else {
                println!(
                    "  自检  ：编码器输出三分量（彩色）JPEG，相位 {}，符合预期",
                    cfg.bayer.as_str()
                );
            }
        }
        Ok(n) => {
            eprintln!("数据源自检不过：JPEG 有 {n} 个分量，期望 {want} 个");
            return ExitCode::from(EX_SOURCE);
        }
        Err(e) => {
            eprintln!("编码自检失败：{e}");
            return ExitCode::from(EX_SOURCE);
        }
    }
    let addr = SocketAddr::new(cfg.bind, cfg.port);
    println!("  数据源：{}", src.describe());
    if cfg.bayer.is_gray() {
        println!("  编码  ：单分量灰度 JPEG，质量 {}", cfg.quality);
    } else {
        let how = if cfg.scale > 1 {
            "CFA binning（去马赛克+降采样一步）"
        } else {
            "Bayer 去马赛克"
        };
        println!(
            "  编码  ：{}（相位 {}）→ 三分量彩色 JPEG，质量 {}",
            how,
            cfg.bayer.as_str(),
            cfg.quality
        );
    }
    println!("  节奏  ：{} fps", cfg.fps);
    if cfg.scale > 1 {
        println!(
            "  缩放  ：编码前 1/{}（灰度：盒式均值；彩色：CFA binning）",
            cfg.scale
        );
    }
    println!("  后处理：{}", postfx_desc(&cfg));
    // gray 相位根本没有马赛克可解，AWB 无从生效 —— 显式提醒，别静默什么都不做
    if cfg.awb && cfg.bayer.is_gray() {
        println!(
            "  注意  ：--bayer gray 不做去马赛克，白平衡无从生效；\
             要彩色请给相位（本板实测 --bayer bggr）"
        );
    }
    println!("  监听  ：http://{addr}/  与  http://{addr}/snapshot.jpg");

    let store = Arc::new(store::FrameStore::new());
    {
        let store = Arc::clone(&store);
        if let Err(e) = std::thread::Builder::new()
            .name("produce".into())
            .spawn(move || produce(src, store, cfg))
        {
            // 线程建不起来：如实报错退出，不 panic 把整个进程带走
            eprintln!("取帧线程起不来：{e}");
            return ExitCode::from(EX_SOURCE);
        }
    }

    if let Err(e) = http::serve(addr, Arc::clone(&store)) {
        eprintln!("HTTP 服务起不来（绑定 {addr}）：{e}");
        return ExitCode::from(EX_SERVE);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Config, String> {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse_args(&v)? {
            Parsed::Run(c) => Ok(*c),
            Parsed::Help => Err("该调用应当跑起来而不是出帮助".into()),
        }
    }

    #[test]
    fn bayer_defaults_to_auto() {
        assert_eq!(run(&[]).unwrap().bayer, BayerPhase::Auto);
        assert_eq!(
            run(&["--source", "pgm", "--pgm", "f.pgm"]).unwrap().bayer,
            BayerPhase::Auto
        );
    }

    #[test]
    fn device_and_r10f_default_to_auto_postfx() {
        let dev = run(&["--source", "device"]).unwrap();
        assert_eq!(
            (dev.black, dev.gamma, dev.ae_target, dev.awb),
            (12, 2.2, 100, true)
        );
        let r10f = run(&["--source", "r10f", "--r10f", "f.dma"]).unwrap();
        assert_eq!(
            (r10f.black, r10f.gamma, r10f.ae_target, r10f.awb),
            (12, 2.2, 100, true)
        );
        let off = run(&["--source", "device", "--no-auto"]).unwrap();
        assert_eq!(
            (off.black, off.gamma, off.ae_target, off.awb),
            (0, 1.0, 0, false)
        );
        let pgm = run(&["--source", "pgm", "--pgm", "f.pgm"]).unwrap();
        assert_eq!(
            (pgm.black, pgm.gamma, pgm.ae_target, pgm.awb),
            (0, 1.0, 0, false)
        );
    }

    #[test]
    fn bayer_accepts_all_five_values() {
        for (arg, want) in [
            ("auto", BayerPhase::Auto),
            ("gray", BayerPhase::Gray),
            ("rggb", BayerPhase::Rggb),
            ("bggr", BayerPhase::Bggr),
            ("grbg", BayerPhase::Grbg),
            ("gbrg", BayerPhase::Gbrg),
        ] {
            let cfg = run(&["--bayer", arg]).unwrap();
            assert_eq!(cfg.bayer, want, "--bayer {arg}");
            assert_eq!(cfg.bayer.as_str(), arg);
        }
    }

    #[test]
    fn bayer_rejects_unknown_value() {
        let e = run(&["--bayer", "rgbg"]).unwrap_err();
        assert!(e.contains("rgbg"), "{e}");
        assert!(run(&["--bayer"]).is_err(), "--bayer 后面缺参数应当报错");
    }

    #[test]
    fn encoded_components_follow_phase() {
        // 同一帧：gray 出 1 个分量，四个彩色相位都出 3 个分量，且四者结果不完全相同
        let mut frame = Frame::new(8, 8, (0..64).map(|i| ((i * 37 + 11) % 256) as u8).collect());
        let mut fx = auto::AutoLevel::new(0, 1.0, 0, false);
        let count = |frame: &mut Frame, p: BayerPhase, fx: &mut auto::AutoLevel| {
            let (j, _, _) = encode_frame(frame, p, 80, 1, fx).expect("编码应当成功");
            encode::jpeg_component_count(&j).expect("应当能找到 SOF")
        };
        assert_eq!(count(&mut frame, BayerPhase::Gray, &mut fx), 1);
        for p in [
            BayerPhase::Rggb,
            BayerPhase::Bggr,
            BayerPhase::Grbg,
            BayerPhase::Gbrg,
        ] {
            assert_eq!(count(&mut frame, p, &mut fx), 3, "相位 {}", p.as_str());
            assert_ne!(count(&mut frame, p, &mut fx), 1);
        }
        // 相位必须真的改变像素，而不只是换个标签：RGGB 与 GRBG 的结果应当不同
        let (a, _, _) = encode_frame(&mut frame, BayerPhase::Rggb, 80, 1, &mut fx).unwrap();
        let (b, _, _) = encode_frame(&mut frame, BayerPhase::Grbg, 80, 1, &mut fx).unwrap();
        assert_ne!(a, b, "不同相位应当产生不同的 JPEG");
    }

    #[test]
    fn postfx_defaults_to_identity_and_auto_preset_fills_in() {
        let c = run(&[]).unwrap();
        assert_eq!((c.black, c.gamma, c.ae_target, c.awb), (0, 1.0, 0, false));
        assert!(postfx_desc(&c).contains("透传"), "{}", postfx_desc(&c));

        let c = run(&["--auto"]).unwrap();
        assert_eq!(
            (c.black, c.gamma, c.ae_target, c.awb),
            (DEF_BLACK, DEF_GAMMA, DEF_AE, true)
        );
        assert!(
            postfx_desc(&c).contains("黑电平 -12"),
            "{}",
            postfx_desc(&c)
        );
    }

    #[test]
    fn postfx_overrides_beat_auto_regardless_of_order() {
        for args in [
            vec!["--auto", "--gamma", "1.8", "--no-awb"],
            vec!["--gamma", "1.8", "--no-awb", "--auto"],
        ] {
            let c = run(&args).unwrap();
            assert_eq!(c.gamma, 1.8, "{args:?}");
            assert!(!c.awb, "{args:?}");
            // 没被覆盖的仍走 --auto 预设
            assert_eq!(c.black, DEF_BLACK, "{args:?}");
            assert_eq!(c.ae_target, DEF_AE, "{args:?}");
        }
        // 单独给一项、不开 --auto：其余保持恒等
        let c = run(&["--ae", "150"]).unwrap();
        assert_eq!((c.ae_target, c.black, c.awb), (150, 0, false));
        let c = run(&["--ae", "0", "--auto"]).unwrap();
        assert_eq!(c.ae_target, 0, "--ae 0 应当能关掉 --auto 里打开的 AE");
    }

    #[test]
    fn postfx_rejects_out_of_range_values() {
        assert!(run(&["--black", "65"]).is_err());
        assert!(run(&["--black", "-1"]).is_err());
        assert!(run(&["--gamma", "0.9"]).is_err());
        assert!(run(&["--gamma", "4.1"]).is_err());
        assert!(run(&["--gamma", "abc"]).is_err());
        assert!(run(&["--ae", "251"]).is_err());
        assert!(run(&["--black", "64"]).is_ok());
        assert!(run(&["--gamma", "1.0"]).is_ok());
    }

    #[test]
    fn help_mentions_postfx_options() {
        for opt in ["--auto", "--black", "--gamma", "--ae", "--awb", "--scale"] {
            assert!(usage().contains(opt), "帮助里应当有 {opt}");
        }
    }

    #[test]
    fn encode_frame_applies_postfx_to_gray_path() {
        // 一帧压在 pedestal 上的灰图：开了黑电平+gamma 之后必须不再是原来的字节，
        // 但分量数仍是 1（不因为后处理就跑偏到彩色通路）
        let mut frame = Frame::new(4, 4, vec![12u8; 16]);
        let mut fx = auto::AutoLevel::new(12, 2.2, 0, false);
        let (j, _, _) = encode_frame(&mut frame, BayerPhase::Gray, 80, 1, &mut fx).unwrap();
        assert_eq!(encode::jpeg_component_count(&j), Some(1));
        assert!(frame.gray.iter().all(|&v| v == 0), "pedestal 应当已被扣掉");
    }

    #[test]
    fn help_mentions_bayer_option() {
        assert!(usage().contains("--bayer"));
        assert!(parse_args(&["--help".into()]).is_ok());
    }

    #[test]
    fn scale_option_parses_and_is_bounded() {
        assert_eq!(run(&[]).unwrap().scale, 1);
        for v in ["1", "2", "4", "8"] {
            assert_eq!(run(&["--scale", v]).unwrap().scale, v.parse().unwrap());
        }
        // 非 2 的幂（彩色 binning 要求块边界对齐）一律拒绝
        for v in ["0", "3", "5", "6", "7", "9"] {
            assert!(run(&["--scale", v]).is_err(), "--scale {v}");
        }
        assert!(run(&["--scale"]).is_err());
    }

    #[test]
    fn scale_shrinks_output_dims_on_both_paths() {
        let src: Vec<u8> = (0..64).map(|i| i as u8).collect();
        let mut fx = auto::AutoLevel::new(0, 1.0, 0, false);
        for phase in [BayerPhase::Gray, BayerPhase::Gbrg] {
            let mut frame = Frame::new(8, 8, src.clone());
            let (j, w, h) = encode_frame(&mut frame, phase, 80, 2, &mut fx).unwrap();
            assert_eq!((w, h), (4, 4), "相位 {}", phase.as_str());
            assert_eq!(
                encode::jpeg_component_count(&j),
                Some(if phase.is_gray() { 1 } else { 3 })
            );
            // 缩过之后源帧本身不该被改动（灰度通路用的是拷贝）
            if phase.is_gray() {
                assert_eq!(frame.gray, src);
            }
        }
        // 源图比缩放倍数还小时退回原图，不 panic、不产生 0 维
        let mut tiny = Frame::new(2, 2, vec![7u8; 4]);
        let (_, w, h) = encode_frame(&mut tiny, BayerPhase::Gray, 80, 8, &mut fx).unwrap();
        assert_eq!((w, h), (2, 2));
    }

    #[test]
    fn synthetic_geometry_over_limit_rejected() {
        let e = run(&["--width", "65535", "--height", "65535"]).unwrap_err();
        assert!(e.contains("上限"), "{e}");
        // 上限之内仍应正常通过
        assert!(run(&["--width", "4096", "--height", "4096"]).is_ok());
    }
}
