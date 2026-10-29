# display-stream —— 取帧 → JPEG → HTTP 推流

RK3588 上从 `/dev/ANLOGIC-PCI0_frame` 取帧，做去马赛克/后处理/JPEG 编码，
再通过最小 HTTP 服务输出 MJPEG 与单帧快照；可选 RKNN YOLO11n 检测叠加。

同一二进制适配两种位流，几何一律以驱动 ioctl 报值为准：

| 位流 | 帧内容 | 用法 |
|---|---|---|
| 当前板载 JPEG 位流 | 1920×1088 单路灰度（R10F 或旧 8 bit） | `--source device --bayer gray` |
| 4K 采集位流 | 3840×2160 packed RAW10、整帧 10,371,072 B | `--source device`，默认自动 2×2 binning |

> 硬件 JPEG **压缩码流**（BPR1/JPEG）不由本程序解码；本程序只处理非压缩 R10F
> 与旧 8 bit 灰度。压缩→NPU 链路由根仓 `tools/board/` 的脚本处理。

## 目录与职责

| 路径 | 内容 |
|---|---|
| `src/driver.rs` | 驱动 ABI：ioctl、结构体、只读 mmap、槽读取 |
| `src/r10f.rs` | R10F 头校验 + packed RAW10 解包 |
| `src/source/` | synthetic / pgm / r10f 文件 / 单路设备 / 双路 pair |
| `src/bayer.rs`、`auto.rs`、`scale.rs` | 相位、去马赛克/CFA binning、黑电平/gamma/AE/AWB、缩放 |
| `src/encode.rs`、`hwjpeg.rs` | 软件 JPEG；RK3588 VEPU121 硬件 JPEG（NV12→JPEG） |
| `src/detect/` | RKNN YOLO11n 推理、NMS、点阵字体叠框 |
| `src/http.rs`、`store.rs` | HTTP 路由、MJPEG、最新帧共享区与帧率统计 |
| `web/` | Vue 3 + Vite 上位机；`dist/index.html` 被编进二进制 |
| `models/`、`scripts/` | RKNN 模型说明与可复跑的转换脚本 |

数据流：设备槽 → R10F 校验/解包 → Bayer 或灰度 → 后处理 → JPEG → HTTP。
双路 `device-pair` 在进程内循环 `ARM_PAIR/ACK_PAIR`，不需要外部 `link_ctl`。

## 构建与部署

前端先构建，Rust 用 `include_str!` 嵌入单文件 HTML，板端不需要 Node：

```sh
cd web
pnpm install
pnpm build                 # 产出 web/dist/index.html
cd ..
cargo build --release
cargo build --release --target aarch64-unknown-linux-gnu
cargo test
```

只改 Rust 不必重跑前端；改了 `web/` 必须先 `pnpm build`。交叉链接器见
`.cargo/config.toml`。把 `target/aarch64-unknown-linux-gnu/release/display-stream`
拷到板上即可（常见部署名 `display-pair`）。

## 运行

```sh
# 当前 1920×1088 灰度位流
/root/display-pair --source device --bayer gray --fps 30 --port 8091

# 4K 采集位流：默认自动 2×2 CFA binning 到 1920×1080
/root/display-pair --source device --fps 15 --port 8091

# 双路（1080p ROI 位流）：先同步，再让 display 自己 pump
/root/link_ctl sync start -t 5000
/root/display-pair --source device-pair --fps 30 --scale 2 --port 8091

# 离线回归
./display-stream --source pgm --pgm /tmp/f.pgm --auto --scale 2
./display-stream --source r10f --r10f fa.dma --auto --scale 2
```

浏览器打开 `http://<板子IP>:8091/`；单路模式页面自动隐藏 B 路。

## HTTP 接口

| 路径 | 说明 |
|---|---|
| `/`、`/pair` | Vue 监控页（单路/双路自适应） |
| `/stream_a.mjpg`、`/stream_b.mjpg` | MJPEG 连续流；旧 `/stream.mjpg` 等同 A 路 |
| `/snapshot_a.jpg`、`/snapshot_b.jpg` | 单帧 JPEG；旧 `/snapshot.jpg` 等同 A 路 |
| `/stats` | `pair/a_seq/b_seq/pair_seq/width/height/a_fps/b_fps` |

响应头带 `X-Frame-Seq`、`X-Image-Width/Height`；服务端按 publish 间隔测帧率，
2 s 无新帧归零。界面截图见 [docs/web-console.jpg](docs/web-console.jpg)。

## 关键参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `--source` | synthetic | synthetic/pgm/r10f/device/device-pair |
| `--bayer` | auto | auto 当前解析为 RGGB；灰度位流用 `gray` |
| `--auto` | device/r10f 开 | 黑电平 12、gamma 2.2、AE 100、AWB；单项可覆盖 |
| `--scale` | 1 = 自动 | 显式 1/2/4/8；>4.0 Mpx 默认 2×2，彩色走 CFA binning |
| `--quality` / `--fps` | 80 / 15 | JPEG 质量、推流节奏 |
| `--jpeg-dev` | `/dev/video4` | 存在即启用 VEPU121；`--no-hw-jpeg` 可关 |
| `--detect` | 关 | 打开 YOLO11n；`--rknn`、`--conf`、`--nms`、`--detect-side` |

## 已知限制

- 当前默认 RGGB；换模组/支架/ROI 奇偶后必须用已知色卡重测，R10F 头 0x28 不可信。
- 只取 RAW10 高 8 位，暗部有量化损失；去马赛克是双线性，边缘可能假彩色。
- 没有 CCM/镜头校正/3A；颜色是“白参考 + 灰世界”级别。
- 硬件 JPEG 依赖 `/dev/video4` 与 S_FMT 顺序，打不开会自动退回软件编码。
- 1080p 彩色双路约 11 pair/s；`--scale 2` 约 23~24 pair/s；4K 单路未上板复测。
- 单路 `--source device` 只读当前帧，不自动 ARM/ACK；双路才内置 pump。

## 文档

- 全链路与帧契约：[../docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md)
- 硬件 JPEG 码流：[../docs/CODEC.md](../docs/CODEC.md)
- RKNN 模型与线程模型：[../docs/RKNN.md](../docs/RKNN.md)
- 板测数据：[../docs/TEST_REPORT.md](../docs/TEST_REPORT.md)
- Web 前端：[web/README.md](web/README.md)　模型：[models/README.md](models/README.md)
