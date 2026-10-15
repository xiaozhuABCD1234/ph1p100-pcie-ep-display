//! display-stream —— RK3588 上「取帧 → JPEG → HTTP 推流」。
//!
//! 数据源三选一（`--source`）：合成源、PGM 文件源、驱动设备源。
//! 帧缓存里是 Bayer 马赛克时用 `--bayer <相位>` 做去马赛克，输出彩色；
//! `--bayer gray`（默认）保持原有的单分量灰度输出。
//! 推流接口两个：MJPEG 连续流与单帧 JPEG 快照（见 `--help`）。

mod bayer;
mod driver;
mod encode;
mod http;
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
/// Bayer 相位默认 `gray`：与旧版行为一致（单分量灰度），要彩色得显式指定相位。
/// 实测（`--bayer` 四相位对比，见 README）首选 `bggr`。
const DEF_BAYER: BayerPhase = BayerPhase::Gray;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SourceKind {
    Synthetic,
    Pgm,
    Device,
}

#[derive(Debug)]
struct Config {
    kind: SourceKind,
    pgm: Option<PathBuf>,
    device: PathBuf,
    width: u16,
    height: u16,
    fps: u32,
    quality: u8,
    bayer: BayerPhase,
    bind: IpAddr,
    port: u16,
    wait_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            kind: SourceKind::Synthetic,
            pgm: None,
            device: PathBuf::from(DEF_DEV),
            width: 640,
            height: 480,
            fps: DEF_FPS,
            quality: DEF_QUALITY,
            bayer: DEF_BAYER,
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
         --source <synthetic|pgm|device>  数据源，默认 synthetic\n\
         --pgm <路径>                     文件源的 PGM（P5/P2 灰度图）\n\
         --device <路径>                  设备源的收帧缓冲节点，默认 {DEF_DEV}\n\
         --width <像素>                   合成源宽度，默认 640\n\
         --height <像素>                  合成源高度，默认 480\n\
         --fps <帧/秒>                    推流节奏，默认 {DEF_FPS}\n\
         --quality <1..100>               JPEG 质量，默认 {DEF_QUALITY}\n\
         --bayer <gray|rggb|bggr|grbg|gbrg>  帧缓存里 Bayer 马赛克的相位，\n\
                                          默认 {DEF_BAYER}（不去马赛克，单分量灰度）；\n\
                                          非 gray 时按该相位双线性去马赛克，\n\
                                          输出三分量彩色 JPEG\n\
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
                    "device" => SourceKind::Device,
                    other => {
                        return Err(format!(
                            "--source 只认 synthetic/pgm/device，收到 '{other}'"
                        ))
                    }
                };
            }
            "--pgm" => cfg.pgm = Some(PathBuf::from(value(args, &mut i, &name)?)),
            "--device" => cfg.device = PathBuf::from(value(args, &mut i, &name)?),
            "--width" => cfg.width = num(&value(args, &mut i, &name)?, &name, 1, 65535)? as u16,
            "--height" => cfg.height = num(&value(args, &mut i, &name)?, &name, 1, 65535)? as u16,
            "--fps" => cfg.fps = num(&value(args, &mut i, &name)?, &name, 1, 240)?,
            "--quality" => cfg.quality = num(&value(args, &mut i, &name)?, &name, 1, 100)? as u8,
            "--bayer" => {
                let v = value(args, &mut i, &name)?;
                cfg.bayer = BayerPhase::parse(&v)
                    .ok_or_else(|| format!("--bayer 只认 gray/rggb/bggr/grbg/gbrg，收到 '{v}'"))?;
            }
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

/// 按配置造数据源。失败就如实报错退出，不起半个服务。
fn build_source(cfg: &Config) -> Result<Box<dyn FrameSource>, String> {
    match cfg.kind {
        SourceKind::Synthetic => Ok(Box::new(Synthetic::new(cfg.width, cfg.height))),
        SourceKind::Pgm => {
            let path = cfg.pgm.clone().expect("parse_args 已校验");
            let src = PgmFile::open(&path).map_err(|e| format!("打开 PGM 失败：{e}"))?;
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
/// 双线性去马赛克，再编三分量彩色 JPEG。
fn encode_frame(frame: &Frame, phase: BayerPhase, quality: u8) -> Result<Vec<u8>, String> {
    if phase.is_gray() {
        return encode::encode_gray(&frame.gray, frame.width, frame.height, quality)
            .map_err(|e| e.to_string());
    }
    let rgb = bayer::demosaic(
        &frame.gray,
        frame.width as usize,
        frame.height as usize,
        phase,
    )?;
    encode::encode_rgb(&rgb, frame.width, frame.height, quality).map_err(|e| e.to_string())
}

/// 取帧线程：取一帧 → 编码 → 放进共享区，按 fps 控节奏；
/// 每 5 秒往标准错误打一条状态（无新帧、编码耗时、驱动序号）。
fn produce(mut src: Box<dyn FrameSource>, store: Arc<store::FrameStore>, cfg: Config) {
    let period = Duration::from_micros(1_000_000 / cfg.fps as u64);
    let mut published: u64 = 0;
    let mut encode_us: u64 = 0;
    let mut last_log = Instant::now();
    let mut io_fails: u32 = 0;

    loop {
        let t0 = Instant::now();
        match src.next_frame() {
            Ok(frame) => {
                io_fails = 0;
                let t_enc = Instant::now();
                let jpeg = match encode_frame(&frame, cfg.bayer, cfg.quality) {
                    Ok(j) => j,
                    Err(e) => {
                        eprintln!("编码失败（{e}），跳过这一帧");
                        continue;
                    }
                };
                encode_us += t_enc.elapsed().as_micros() as u64;
                let stream_seq = store.publish(jpeg, frame.width, frame.height);
                published += 1;

                if last_log.elapsed() >= Duration::from_secs(5) {
                    let extra = match src.status() {
                        Some(s) => format!("，{s}"),
                        None => String::new(),
                    };
                    eprintln!(
                        "已推 {published} 帧（推流序号 {stream_seq}）；最近一帧 {}x{}，\
                         平均编码 {:.2}ms{extra}",
                        frame.width,
                        frame.height,
                        encode_us as f64 / 1000.0 / published.max(1) as f64,
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
    let probe = Frame::new(4, 1, vec![0, 64, 128, 255]);
    match encode_frame(&probe, cfg.bayer, cfg.quality).and_then(|j| {
        encode::jpeg_component_count(&j).ok_or_else(|| "输出里找不到 SOF 段".to_string())
    }) {
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
        println!(
            "  编码  ：Bayer 去马赛克（相位 {}）→ 三分量彩色 JPEG，质量 {}",
            cfg.bayer.as_str(),
            cfg.quality
        );
    }
    println!("  节奏  ：{} fps", cfg.fps);
    println!("  监听  ：http://{addr}/  与  http://{addr}/snapshot.jpg");

    let store = Arc::new(store::FrameStore::new());
    {
        let store = Arc::clone(&store);
        std::thread::Builder::new()
            .name("produce".into())
            .spawn(move || produce(src, store, cfg))
            .expect("spawn produce");
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
    fn bayer_defaults_to_gray() {
        assert_eq!(run(&[]).unwrap().bayer, BayerPhase::Gray);
        assert_eq!(
            run(&["--source", "pgm", "--pgm", "f.pgm"]).unwrap().bayer,
            BayerPhase::Gray
        );
    }

    #[test]
    fn bayer_accepts_all_five_values() {
        for (arg, want) in [
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
        let frame = Frame::new(8, 8, (0..64).map(|i| ((i * 37 + 11) % 256) as u8).collect());
        let count = |p: BayerPhase| {
            let j = encode_frame(&frame, p, 80).expect("编码应当成功");
            encode::jpeg_component_count(&j).expect("应当能找到 SOF")
        };
        assert_eq!(count(BayerPhase::Gray), 1);
        for p in [
            BayerPhase::Rggb,
            BayerPhase::Bggr,
            BayerPhase::Grbg,
            BayerPhase::Gbrg,
        ] {
            assert_eq!(count(p), 3, "相位 {}", p.as_str());
            assert_ne!(count(p), count(BayerPhase::Gray));
        }
        // 相位必须真的改变像素，而不只是换个标签：RGGB 与 GRBG 的结果应当不同
        let a = encode_frame(&frame, BayerPhase::Rggb, 80).unwrap();
        let b = encode_frame(&frame, BayerPhase::Grbg, 80).unwrap();
        assert_ne!(a, b, "不同相位应当产生不同的 JPEG");
    }

    #[test]
    fn help_mentions_bayer_option() {
        assert!(usage().contains("--bayer"));
        assert!(parse_args(&["--help".into()]).is_ok());
    }
}
