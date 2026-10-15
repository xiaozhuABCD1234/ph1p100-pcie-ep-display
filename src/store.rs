//! 最新帧共享区：取帧线程往里放编码好的 JPEG，HTTP 线程各自取。
//!
//! MJPEG 客户端只关心「下一帧」——用 `Condvar` 等新序号，不做队列堆积：
//! 客户端慢就自然丢帧，不占内存。

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct Snapshot {
    /// 单调递增的帧序号（不是驱动那个 seq，是推流侧的计数）
    pub seq: u64,
    pub jpeg: Arc<Vec<u8>>,
    pub width: u16,
    pub height: u16,
}

struct Inner {
    latest: Option<Snapshot>,
}

pub struct FrameStore {
    inner: Mutex<Inner>,
    cv: Condvar,
    next_seq: Mutex<u64>,
}

impl FrameStore {
    pub fn new() -> Self {
        FrameStore {
            inner: Mutex::new(Inner { latest: None }),
            cv: Condvar::new(),
            next_seq: Mutex::new(0),
        }
    }

    /// 放一帧，返回分配到的序号
    pub fn publish(&self, jpeg: Vec<u8>, width: u16, height: u16) -> u64 {
        let seq = {
            let mut n = self.next_seq.lock().unwrap();
            *n += 1;
            *n
        };
        let snap = Snapshot {
            seq,
            jpeg: Arc::new(jpeg),
            width,
            height,
        };
        let mut g = self.inner.lock().unwrap();
        g.latest = Some(snap);
        drop(g);
        self.cv.notify_all();
        seq
    }

    /// 当前最新帧（没有就返回 None）
    pub fn latest(&self) -> Option<Snapshot> {
        self.inner.lock().unwrap().latest.clone()
    }

    /// 等一帧序号不等于 `last_seq` 的（最多等 `timeout`）。超时返回 None。
    pub fn wait_next(&self, last_seq: u64, timeout: Duration) -> Option<Snapshot> {
        let g = self.inner.lock().unwrap();
        let (g, _) = self
            .cv
            .wait_timeout_while(g, timeout, |g| match &g.latest {
                Some(s) => s.seq == last_seq,
                None => true,
            })
            .unwrap();
        match &g.latest {
            Some(s) if s.seq != last_seq => Some(s.clone()),
            _ => None,
        }
    }
}

impl Default for FrameStore {
    fn default() -> Self {
        Self::new()
    }
}
