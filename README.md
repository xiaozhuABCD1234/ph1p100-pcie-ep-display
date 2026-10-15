# display-stream —— 取帧 → JPEG → HTTP 推流

RK3588 上把安路 PH1P100 EP 送来的帧（默认 640×480）取出来，编成 JPEG，由最小
HTTP 服务推出去（MJPEG 连续流 + 单帧快照）。默认编成**单分量灰度 JPEG**；
帧缓存里是 Bayer 马赛克时用 `--bayer <相位>` 做去马赛克，改推**三分量彩色 JPEG**。
Rust cargo 工程，无 C/C++ 源文件。

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
    ./display-stream --source device --bayer bggr            # 设备源 + Bayer 去马赛克 → 彩色

参数：`--source synthetic|pgm|device`（默认 synthetic）、`--pgm <路径>`、
`--device <节点>`（默认 `/dev/ANLOGIC-PCI0_frame`，需先加载驱动）、
`--width`/`--height`（合成源几何）、`--fps`（默认 15）、`--quality`（1..100，默认 80）、
`--bayer <gray|rggb|bggr|grbg|gbrg>`（默认 `gray`）、
`--port`（默认 8090）、`--bind`（默认 0.0.0.0）、`--timeout-ms`（设备源等帧超时，默认 2000）、
`-h`。退出码：1 用法错、2 数据源起不来、3 服务起不来、4 设备连续 IO 失败。

### `--bayer`：把帧当 Bayer 马赛克，出去马赛克的彩色图

整帧按该相位做双线性去马赛克（`src/bayer.rs`，纯 Rust、无新依赖），再编三分量
彩色 JPEG；MJPEG 与 `/snapshot.jpg` 两个端点都是彩色。`gray` 是默认值，也是旧版
行为：帧当单分量灰度图，不去马赛克。

相位首选 **`bggr`**（次选 `rggb`，两者只差红蓝互换）。依据是一帧真实抓帧（640×480）
上四种相位各出一张 JPEG 后的统计：`bggr`/`rggb` 的绿通道均值最高（24.2，另两相位
17.8）、约八成像素判为绿主导（另两相位 0.8%）、色度假彩色能量最低（12.0 对 15.3）；
而原图上同属一色的两条对角线
（偶行奇列与奇行偶列）的块均值差只有 0.17，另一条对角线是 2.69（亮 15.9 倍）。
绿在采样密度上是马赛克里唯一占两格的通道，故绿必须落在对称的那条对角线上，
于是相位只能是 `bggr` 或 `rggb`。选 `bggr` 属**推论**（典型 CMOS 原始响应
绿 > 红 > 蓝，IMX415 蓝通道量子效率最低），不是实测：单帧数据里 `bggr` 与 `rggb`
逐像素只差红蓝互换，任何纯幅度指标都区分不了，要用已知颜色的物体或色卡才能定死。

已知限制：**不做白平衡**，所以彩色图在暗光下整体偏绿；相位错了则呈品红假彩色。
去马赛克只有双线性，没有边缘自适应，高频边缘会有假彩色锯齿。

## 接口

    /             网页（内嵌 MJPEG 流）
    /stream.mjpg  MJPEG 连续流（multipart/x-mixed-replace）
    /snapshot.jpg 单帧 JPEG

`curl -o f.jpg http://<板子IP>:8090/snapshot.jpg` 抓一帧；`X-Frame-Seq` 等头带帧序号与几何。
同时最多 16 条连接，超出直接回 503 并关闭；请求读 5s、请求头总限 10s、响应写 5s 超时，慢/不读的客户端会被关连接。

## 代码位置

    src/driver.rs        驱动契约唯一落点：ioctl 编号、结构体布局、只读 mmap 与取帧时序
    src/source/          synthetic / pgm / device 三种数据源（device 只是适配）
    src/bayer.rs         Bayer 相位与双线性去马赛克（纯 Rust，`--bayer` 走这里）
    src/encode.rs        JPEG 编码（jpeg-encoder：灰度用 ColorType::Luma，彩色用 Rgb）
    src/http.rs store.rs 最小 HTTP 服务、最新帧共享区（MJPEG 靠共享区拿下一帧）
