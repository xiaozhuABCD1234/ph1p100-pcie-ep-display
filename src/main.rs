//! display-stream —— RK3588 上「取帧 → 灰度 JPEG → HTTP 推流」。
//!
//! 数据源三选一（`--source`）：合成源、PGM 文件源、驱动设备源。
//! 推流接口两个：MJPEG 连续流与单帧 JPEG 快照（见 `--help`）。

mod driver;
mod encode;
mod http;
mod source;
mod store;

use source::{device::DeviceSource, pgm::PgmFile, synthetic::Synthetic, FrameSource, SourceError};
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SourceKind {
    Synthetic,
    Pgm,
    Device,
}

struct Config {
    kind: SourceKind,
    pgm: Option<PathBuf>,
    device: PathBuf,
    width: u16,
    height: u16,
    fps: u32,
    quality: u8,
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
            bind: IpAddr::from([0, 0, 0, 0]),
            port: DEF_PORT,
            wait_ms: DEF_WAIT_MS,
        }
    }
}

fn usage() -> String {
    format!(
        "display-stream —— 取帧 → 灰度 JPEG → HTTP 推流\n\
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
         --port <端口>                    监听端口，默认 {DEF_PORT}\n\
         --bind <地址>                    监听地址，默认 0.0.0.0\n\
         --timeout-ms <毫秒>              设备源等帧超时，默认 {DEF_WAIT_MS}\n\
         -h, --help                       显示本帮助\n\
         \n\
         接口：/             网页（内嵌 MJPEG 流）\n\
               /stream.mjpg   MJPEG 连续流（multipart/x-mixed-replace）\n\
               /snapshot.jpg  单帧 JPEG 快照\n"
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
                let jpeg = match encode::encode_gray(
                    &frame.gray,
                    frame.width,
                    frame.height,
                    cfg.quality,
                ) {
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
    // 启动自检：确认编码器真的输出单分量（灰度）JPEG，而不是彩色
    match encode::encode_gray(&[0, 64, 128, 255], 4, 1, cfg.quality)
        .map_err(|e| e.to_string())
        .and_then(|j| {
            encode::jpeg_component_count(&j).ok_or_else(|| "输出里找不到 SOF 段".to_string())
        }) {
        Ok(1) => println!("  自检  ：编码器输出单分量（灰度）JPEG，符合预期"),
        Ok(n) => {
            eprintln!("数据源自检不过：JPEG 有 {n} 个分量，不是灰度");
            return ExitCode::from(EX_SOURCE);
        }
        Err(e) => {
            eprintln!("编码自检失败：{e}");
            return ExitCode::from(EX_SOURCE);
        }
    }
    let addr = SocketAddr::new(cfg.bind, cfg.port);
    println!("  数据源：{}", src.describe());
    println!("  编码  ：单分量灰度 JPEG，质量 {}", cfg.quality);
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
