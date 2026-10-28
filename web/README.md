# display web —— 双路视觉上位机

Vue 3 + Vite 的单页监控界面，构建成**一个** `dist/index.html`（JS/CSS 全内联），
由 `../src/http.rs` 用 `include_str!` 嵌入 Rust 二进制。板端不需要 Node、
不访问外网 CDN。

## 开发

~~~sh
pnpm install
DISPLAY_BOARD=http://<板子IP>:8091 pnpm dev
~~~

Vite 会把 `/stats`、`/stream_*.mjpg`、`/snapshot_*.jpg` 代理到板子。
只改前端时不需要重编 Rust。

## 构建

~~~sh
pnpm build          # 产出 dist/index.html
cd .. && cargo build --release --target aarch64-unknown-linux-gnu
~~~

`dist/index.html` 要随源码提交：Rust 编译时直接读这个文件，CI 或板上不跑 Node。

## 数据来源

页面只依赖现有接口：

- `/stats`：`pair / a_seq / b_seq / pair_seq / width / height / a_fps / b_fps`
- `/stream_a.mjpg`、`/stream_b.mjpg`
- `/snapshot_a.jpg`、`/snapshot_b.jpg`

帧率由服务端测量，页面 1 s 轮询一次；MJPEG 解码完全交给浏览器原生 `<img>`。
