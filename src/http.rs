//! 最小 HTTP 服务：把最新帧以 MJPEG（`multipart/x-mixed-replace`）连续推出去，
//! 另给一个单帧 JPEG 快照接口。只用 std 的 `TcpListener`，一个连接一个线程。

use crate::store::{FrameStore, Snapshot};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

/// multipart 的分界串（请求头里 `boundary=frame`，正文分隔行是 `--frame`）
const BOUNDARY: &str = "frame";

/// 一路 MJPEG 客户端在没新帧时最多等多久，然后探一次对端是否断开
const IDLE_PROBE: Duration = Duration::from_secs(5);

/// 请求行最多读多少字节就放弃
const MAX_REQUEST: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Index,
    Stream,
    Snapshot,
}

/// 路由表：查询串（`?…`）已由调用方剥掉
pub fn route(path: &str) -> Option<Route> {
    match path {
        "/" | "/index.html" => Some(Route::Index),
        "/stream.mjpg" | "/stream" | "/mjpeg" => Some(Route::Stream),
        "/snapshot.jpg" | "/snapshot" => Some(Route::Snapshot),
        _ => None,
    }
}

/// 阻塞式服务：绑上端口后不会返回（除非 accept 出错）
pub fn serve(addr: SocketAddr, store: Arc<FrameStore>) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let store = Arc::clone(&store);
                std::thread::Builder::new()
                    .name("http-client".into())
                    .spawn(move || {
                        let peer = stream
                            .peer_addr()
                            .map(|a| a.to_string())
                            .unwrap_or_else(|_| "?".into());
                        if let Err(e) = handle(stream, &store) {
                            // 客户端提前关流是常态，不当错误刷屏
                            if e.kind() != io::ErrorKind::BrokenPipe
                                && e.kind() != io::ErrorKind::ConnectionReset
                                && e.kind() != io::ErrorKind::UnexpectedEof
                            {
                                eprintln!("客户端 {peer}：{e}");
                            }
                        }
                    })
                    .expect("spawn http-client");
            }
            Err(e) => eprintln!("accept 失败：{e}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, store: &FrameStore) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
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
        Some(Route::Index) => respond(w, 200, "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        Some(Route::Snapshot) => match store.latest() {
            Some(snap) => {
                let mut head = format!(
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
                head.clear();
                w.write_all(&snap.jpeg)?;
                w.flush()
            }
            None => respond(
                w,
                503,
                "text/plain; charset=utf-8",
                "还没有帧：数据源还没产出（FPGA 未接上时设备源会一直空转）\n".as_bytes(),
            ),
        },
        Some(Route::Stream) => stream_mjpeg(w, store),
        None => respond(
            w,
            404,
            "text/plain; charset=utf-8",
            "没有这个路径\n".as_bytes(),
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
fn read_request(stream: &TcpStream) -> io::Result<(String, String)> {
    let mut s = stream; // &TcpStream 本身实现了 Read
    let mut buf = Vec::new();
    let mut tmp = [0u8; 512];
    loop {
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

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh"><head><meta charset="utf-8"><title>display-stream</title></head>
<body style="margin:0;background:#111;color:#ccc;font-family:sans-serif">
<h3 style="margin:12px">display-stream —— MJPEG（灰度或 Bayer 彩色，看 --bayer）</h3>
<img src="/stream.mjpg" style="width:100%;image-rendering:pixelated" alt="stream">
<p style="margin:12px">单帧快照：<a style="color:#8cf" href="/snapshot.jpg">/snapshot.jpg</a></p>
</body></html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_and_query_strings() {
        assert_eq!(route("/"), Some(Route::Index));
        assert_eq!(route("/stream.mjpg"), Some(Route::Stream));
        assert_eq!(route("/stream"), Some(Route::Stream));
        assert_eq!(route("/snapshot.jpg"), Some(Route::Snapshot));
        assert_eq!(route("/nope"), None);
    }

    #[test]
    fn strips_query_and_fragment() {
        assert_eq!(normalize_target("/snapshot.jpg?t=123"), "/snapshot.jpg");
        assert_eq!(normalize_target("/stream.mjpg#x"), "/stream.mjpg");
        assert_eq!(normalize_target("/"), "/");
        assert_eq!(
            route(&normalize_target("/snapshot.jpg?t=1")),
            Some(Route::Snapshot)
        );
    }
}
