# display-stream —— 取帧 → 灰度 JPEG → HTTP 推流

RK3588 上把安路 PH1P100 EP 送来的灰度帧（默认 640×480）取出来，编成**单分量灰度
JPEG**，由最小 HTTP 服务推出去（MJPEG 连续流 + 单帧快照）。Rust cargo 工程，无 C/C++ 源文件。

## 构建

    cargo build --release                                # 本机原生，产物 target/release/
    cargo build --release --target aarch64-unknown-linux-gnu   # 给板子
    cargo test --release                                 # 单元测试（编码/解析/路由）

交叉编译的链接器见 `.cargo/config.toml`（`aarch64-linux-gnu-gcc`），产物在
`target/aarch64-unknown-linux-gnu/release/display-stream`。

## 运行

    ./display-stream                                        # 合成源 640×480、15fps、端口 8090
    ./display-stream --source pgm --pgm frame.pgm            # PGM 文件源（P5/P2 灰度图）
    ./display-stream --source device --fps 30                # 设备源，默认节点见下

参数：`--source synthetic|pgm|device`（默认 synthetic）、`--pgm <路径>`、
`--device <节点>`（默认 `/dev/ANLOGIC-PCI0_frame`，需先加载驱动）、
`--width`/`--height`（合成源几何）、`--fps`（默认 15）、`--quality`（1..100，默认 80）、
`--port`（默认 8090）、`--bind`（默认 0.0.0.0）、`--timeout-ms`（设备源等帧超时，默认 2000）、
`-h`。退出码：1 用法错、2 数据源起不来、3 服务起不来、4 设备连续 IO 失败。

## 接口

    /             网页（内嵌 MJPEG 流）
    /stream.mjpg  MJPEG 连续流（multipart/x-mixed-replace）
    /snapshot.jpg 单帧 JPEG

`curl -o f.jpg http://<板子IP>:8090/snapshot.jpg` 抓一帧；`X-Frame-Seq` 等头带帧序号与几何。

## 代码位置

    src/driver.rs        驱动契约唯一落点：ioctl 编号、结构体布局、只读 mmap 与取帧时序
    src/source/          synthetic / pgm / device 三种数据源（device 只是适配）
    src/encode.rs        灰度 JPEG 编码（jpeg-encoder 的 ColorType::Luma）
    src/http.rs store.rs 最小 HTTP 服务、最新帧共享区（MJPEG 靠共享区拿下一帧）
