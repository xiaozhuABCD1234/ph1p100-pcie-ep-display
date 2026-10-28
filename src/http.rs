//! 最小 HTTP 服务：把最新帧以 MJPEG（`multipart/x-mixed-replace`）连续推出去，
//! 另给一个单帧 JPEG 快照接口。只用 std 的 `TcpListener`，一个连接一个线程；
//! 连接数有准入上限，请求读与响应写都有超时，`accept` 出错按指数退避重试。

use crate::store::{FrameStore, Snapshot};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// multipart 的分界串（请求头里 `boundary=frame`，正文分隔行是 `--frame`）
const BOUNDARY: &str = "frame";

/// 一路 MJPEG 客户端在没新帧时最多等多久，然后探一次对端是否断开
const IDLE_PROBE: Duration = Duration::from_secs(5);

/// 请求行最多读多少字节就放弃
const MAX_REQUEST: usize = 8 * 1024;

/// 同时最多处理多少条连接（准入上限）。超出直接回 503 并关闭，不 spawn 线程：
/// 只连不发或只连不 recv 的客户端最多占这么多线程栈。
const MAX_CONNECTIONS: usize = 16;

/// 请求读的单次超时：连上不发请求的连接最多静默这么久
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 读完整条请求头的总时限：每几秒滴一个字节的慢客户端也拖不过这个上限
const READ_DEADLINE: Duration = Duration::from_secs(10);
/// 响应写超时：连上不 recv 的客户端最多拖这么久（超时就关连接，不永久阻塞）
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// 超限回 503 用的写超时，比正常路径短，免得拖住 accept 循环
const BUSY_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// `accept` 失败的退避区间（`EMFILE`/`ENFILE`/`ENOBUFS` 等按指数增长）
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_millis(500);
/// `accept` 失败日志降频：错误反复出现也最多每秒一条
const ACCEPT_LOG_INTERVAL: Duration = Duration::from_secs(1);

/// 超过准入上限时回给客户端的正文
const BUSY_BODY: &str = "同时连接数已达上限，请稍后再试\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Index,
    PairPage,
    StreamA,
    StreamB,
    SnapshotA,
    SnapshotB,
    Stats,
}

/// 路由表：查询串（`?…`）已由调用方剥掉。
/// 旧路径全部保留并指向 A 路；双路新增 `_a/_b/pair`。
pub fn route(path: &str) -> Option<Route> {
    match path {
        "/" | "/index.html" => Some(Route::Index),
        "/pair" | "/pair.html" => Some(Route::PairPage),
        "/stream.mjpg" | "/stream" | "/mjpeg" | "/stream_a.mjpg" | "/stream_a" => {
            Some(Route::StreamA)
        }
        "/stream_b.mjpg" | "/stream_b" => Some(Route::StreamB),
        "/snapshot.jpg" | "/snapshot" | "/snapshot_a.jpg" | "/snapshot_a" => Some(Route::SnapshotA),
        "/snapshot_b.jpg" | "/snapshot_b" => Some(Route::SnapshotB),
        "/stats" | "/stats.json" => Some(Route::Stats),
        _ => None,
    }
}

/// 阻塞式服务：绑上端口后不会返回。连接数有准入上限，`accept` 出错退避重试，
/// 不会因为 fd 耗尽而空转烧核或刷爆日志。
pub fn serve(
    addr: SocketAddr,
    store: Arc<FrameStore>,
    b_store: Option<Arc<FrameStore>>,
) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    let live = Arc::new(AtomicUsize::new(0));
    let mut backoff = ACCEPT_BACKOFF_MIN;
    // 初始化为「一段时间以前」，保证第一条失败日志一定打得出来
    let mut last_log = Instant::now() - ACCEPT_LOG_INTERVAL;
    loop {
        match listener.accept() {
            Ok((stream, _peer)) => {
                backoff = ACCEPT_BACKOFF_MIN;
                // 准入：先占名额，超限就回 503 关掉，不进线程
                let n = live.fetch_add(1, Ordering::AcqRel);
                if n >= MAX_CONNECTIONS {
                    live.fetch_sub(1, Ordering::AcqRel);
                    let _ = refuse_busy(stream);
                    continue;
                }
                let guard = ConnGuard(Arc::clone(&live));
                let store = Arc::clone(&store);
                let b_store = b_store.as_ref().map(Arc::clone);
                let spawned = std::thread::Builder::new()
                    .name("http-client".into())
                    .spawn(move || {
                        // 名额随本连接存续，drop（含 spawn 失败丢弃闭包）时归还
                        let _guard = guard;
                        let peer = stream
                            .peer_addr()
                            .map(|a| a.to_string())
                            .unwrap_or_else(|_| "?".into());
                        if let Err(e) = handle(stream, &store, b_store.as_deref()) {
                            // 客户端提前关流/不发数据是常态，不当错误刷屏
                            if !is_quiet(&e) {
                                eprintln!("客户端 {peer}：{e}");
                            }
                        }
                    });
                if let Err(e) = spawned {
                    // 线程建不起来（EAGAIN/内核内存不足）：不让整个进程跟着走，
                    // guard 随失败的闭包一起 drop，名额自动释放
                    eprintln!("连接线程建不起来，本连接关闭：{e}");
                }
            }
            Err(e) => {
                // EINTR / ECONNABORTED 是瞬时的，立刻重试
                if is_accept_retry(&e) {
                    continue;
                }
                // 其余错误（尤其 fd 耗尽后的 EMFILE/ENFILE）退避，别空转烧核
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
                // 降频打印：反复失败也最多每秒一条
                if last_log.elapsed() >= ACCEPT_LOG_INTERVAL {
                    eprintln!("accept 失败（已退避到 {}ms）：{e}", backoff.as_millis());
                    last_log = Instant::now();
                }
            }
        }
    }
}

/// `accept` 的瞬时错误：立刻重试即可（不用退避）
fn is_accept_retry(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    )
}

/// 客户端侧的正常断开/超时：不当作服务端错误刷日志
fn is_quiet(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
    )
}

/// 超限时回 503 并关闭（用较短的写超时，避免拖住 accept 循环）
fn refuse_busy(mut stream: TcpStream) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(BUSY_WRITE_TIMEOUT));
    respond(
        &mut stream,
        503,
        "text/plain; charset=utf-8",
        BUSY_BODY.as_bytes(),
    )
}

/// 连接名额守卫：drop 时把「在处理连接数」归还
struct ConnGuard(Arc<AtomicUsize>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle(
    mut stream: TcpStream,
    store: &FrameStore,
    b_store: Option<&FrameStore>,
) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    // 读请求与写响应都设超时：连上不发请求、或连上不 recv 的客户端
    // 都不能永久占着线程（超时按 WouldBlock 返回，连接随即关闭）
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let (method, path) = read_request(&stream)?;
    let route = route(&path);
    let w = &mut stream;

    if method != "GET" {
        return respond(
            w,
            405,
            "text/plain; charset=utf-8",
            "只支持 GET\n".as_bytes(),
        );
    }
    match route {
        // 单路/双路共用同一套前端：页面按 /stats 的 pair 字段决定是否显示 B 路。
        Some(Route::Index) | Some(Route::PairPage) => {
            respond(w, 200, "text/html; charset=utf-8", APP_HTML.as_bytes())
        }
        Some(Route::StreamA) => stream_mjpeg(w, store),
        Some(Route::StreamB) => match b_store {
            Some(b) => stream_mjpeg(w, b),
            None => no_b(w),
        },
        Some(Route::SnapshotA) => snapshot(w, store),
        Some(Route::SnapshotB) => match b_store {
            Some(b) => snapshot(w, b),
            None => no_b(w),
        },
        Some(Route::Stats) => {
            // 页面每秒轮询一次：序号用于计算帧率，几何用于 HUD，
            // pair 决定是否显示 B 路。两路每对同时 publish，所以最小序号
            // 就是已完成的采集对数。
            let a_snap = store.latest();
            let b_snap = b_store.and_then(|s| s.latest());
            let a = a_snap.as_ref().map(|s| s.seq).unwrap_or(0);
            let b = b_snap.as_ref().map(|s| s.seq).unwrap_or(0);
            let pair = b_store.is_some();
            let pair_seq = if pair { a.min(b) } else { 0 };
            let (width, height) = a_snap
                .as_ref()
                .or(b_snap.as_ref())
                .map(|s| (s.width, s.height))
                .unwrap_or((0, 0));
            let a_fps = store.fps();
            let b_fps = b_store.map(|s| s.fps()).unwrap_or(0.0);
            let body = format!(
                "{{\"pair\":{pair},\"a_seq\":{a},\"b_seq\":{b},\"pair_seq\":{pair_seq},\
                  \"width\":{width},\"height\":{height},\
                  \"a_fps\":{a_fps:.1},\"b_fps\":{b_fps:.1}}}\n"
            );
            respond(w, 200, "application/json", body.as_bytes())
        }
        None => respond(
            w,
            404,
            "text/plain; charset=utf-8",
            "没有这个路径\n".as_bytes(),
        ),
    }
}

fn no_b(w: &mut TcpStream) -> io::Result<()> {
    respond(
        w,
        503,
        "text/plain; charset=utf-8",
        "当前不是双路模式（用 --source device-pair 启动）\n".as_bytes(),
    )
}

fn snapshot(w: &mut TcpStream, store: &FrameStore) -> io::Result<()> {
    match store.latest() {
        Some(snap) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\
                 Cache-Control: no-store, no-cache, must-revalidate\r\nPragma: no-cache\r\n\
                 X-Frame-Seq: {}\r\nX-Image-Width: {}\r\nX-Image-Height: {}\r\n\
                 Connection: close\r\n\r\n",
                snap.jpeg.len(),
                snap.seq,
                snap.width,
                snap.height
            );
            w.write_all(head.as_bytes())?;
            w.write_all(&snap.jpeg)?;
            w.flush()
        }
        None => respond(
            w,
            503,
            "text/plain; charset=utf-8",
            "还没有帧：数据源还没产出（FPGA 未接上时设备源会一直空转）\n".as_bytes(),
        ),
    }
}

fn stream_mjpeg(w: &mut TcpStream, store: &FrameStore) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary={BOUNDARY}\r\n\
         Cache-Control: no-store, no-cache, must-revalidate\r\nPragma: no-cache\r\n\
         Connection: close\r\n\r\n"
    );
    w.write_all(head.as_bytes())?;

    let mut last = 0u64;
    // 先把手上最新的一帧发出去，客户端立刻有画面
    if let Some(snap) = store.latest() {
        write_part(w, &snap)?;
        last = snap.seq;
    }
    loop {
        match store.wait_next(last, IDLE_PROBE) {
            Some(snap) => {
                write_part(w, &snap)?;
                last = snap.seq;
            }
            None => {
                // 一直没新帧：探一次对端是否已经断开，免得线程空挂
                if peer_gone(w.as_raw_fd()) {
                    return Ok(());
                }
            }
        }
    }
}

fn write_part(w: &mut TcpStream, snap: &Snapshot) -> io::Result<()> {
    let head = format!(
        "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\
         X-Frame-Seq: {}\r\n\r\n",
        snap.jpeg.len(),
        snap.seq
    );
    w.write_all(head.as_bytes())?;
    w.write_all(&snap.jpeg)?;
    w.write_all(b"\r\n")?;
    w.flush()
}

fn respond(w: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        reason(code),
        body.len()
    );
    w.write_all(head.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// 读请求行。只解析首行的方法与路径，剩下的头一律不看（不使用 keep-alive）。
/// 单次读有 5s 超时，整条请求头另有总时限，慢客户端拖不住线程。
fn read_request(stream: &TcpStream) -> io::Result<(String, String)> {
    let mut s = stream; // &TcpStream 本身实现了 Read
    let mut buf = Vec::new();
    let mut tmp = [0u8; 512];
    let started = Instant::now();
    loop {
        if started.elapsed() > READ_DEADLINE {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "请求头读取超过总时限",
            ));
        }
        let n = s.read(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "对端在请求行读完前就关了",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_REQUEST {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "请求头过长"));
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let line = head.lines().next().unwrap_or_default();
    let mut it = line.split_whitespace();
    let method = it.next().unwrap_or_default().to_string();
    let target = it.next().unwrap_or_default();
    if method.is_empty() || target.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "请求行不完整"));
    }
    Ok((method, normalize_target(target)))
}

/// 剥掉查询串/锚点，只留路径
fn normalize_target(target: &str) -> String {
    target
        .split(['?', '#'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("/")
        .to_string()
}

/// 对端是否已断开（MSG_PEEK + 非阻塞：返回 0 就是收到了 FIN）
fn peer_gone(fd: RawFd) -> bool {
    let mut b = [0u8; 1];
    let n = unsafe {
        libc::recv(
            fd,
            b.as_mut_ptr() as *mut libc::c_void,
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    n == 0
}

/// 前端构建产物（`cd web && npm run build`）：Vite 打成单个 HTML，
/// JS/CSS 全部内联，板端不需要 Node，也不依赖外部 CDN。
const APP_HTML: &str = include_str!("../web/dist/index.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_and_query_strings() {
        assert_eq!(route("/"), Some(Route::Index));
        assert_eq!(route("/stream.mjpg"), Some(Route::StreamA));
        assert_eq!(route("/stream"), Some(Route::StreamA));
        assert_eq!(route("/snapshot.jpg"), Some(Route::SnapshotA));
        assert_eq!(route("/stream_b.mjpg"), Some(Route::StreamB));
        assert_eq!(route("/snapshot_b.jpg"), Some(Route::SnapshotB));
        assert_eq!(route("/pair"), Some(Route::PairPage));
        assert_eq!(route("/stats"), Some(Route::Stats));
        assert_eq!(route("/nope"), None);
    }

    #[test]
    fn strips_query_and_fragment() {
        assert_eq!(normalize_target("/snapshot.jpg?t=123"), "/snapshot.jpg");
        assert_eq!(normalize_target("/stream.mjpg#x"), "/stream.mjpg");
        assert_eq!(normalize_target("/"), "/");
        assert_eq!(
            route(&normalize_target("/snapshot.jpg?t=1")),
            Some(Route::SnapshotA)
        );
    }
}
