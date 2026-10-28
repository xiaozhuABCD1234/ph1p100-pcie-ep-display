# display-stream —— R10F 取帧 → 彩色 JPEG → HTTP 推流

RK3588 上把安路 PH1P100 EP 送来的相机帧取出、去马赛克/后处理、编成 JPEG，由最小
HTTP 服务推出去（MJPEG 连续流 + 单帧快照）。

> 2026-10-28 状态：当前正式通路是 **1920×1080 ROI、packed RAW10、R10F v1**，
> 一帧传输 **2,592,768 B**（128 B 头 + 2,592,000 B 载荷 + 640 B 尾零）。
> 驱动只创建一个 <code>/dev/ANLOGIC-PCI0_frame</code> 节点；B 路靠 <code>CTRL[8]</code> 选源，
> 没有 <code>_frame_b</code> 节点。旧 README 里的 4K / 8.29 MB / 7.85 fps / <code>_frame_b</code>
> 是 8bit 双路时代的历史，不适用于当前代码。

## 快速开始

板上（二进制已交叉编译到 aarch64）：

~~~sh
# 1) 设备单帧：读缓冲里现有/已 ARM 的一帧，彩色 + 后处理
/root/display-stream-new --source device --scale 2 --port 18090
# device/r10f 默认已开 --auto（黑电平/gamma/AWB/AE），相位默认 auto→RGGB
# 浏览器/本机：
curl -o /tmp/f.jpg http://<板子IP>:18090/snapshot.jpg

# 2) 静态 PGM（frame_grab 已解包 R10F；最稳的离线路径）
/root/frame_grab -n -o /tmp/f.pgm
./display-stream --source pgm --pgm /tmp/f.pgm --auto --scale 2

# 3) 离线回放一帧 R10F（.dma 从 R10F 魔数开始）
./display-stream --source r10f --r10f fa.dma --auto --scale 2
~~~

一帧推完后不会自动再来一帧：当前设备源没有常驻 RKA1 ARM/ACK pump。
要连续采集，需要外部循环：

~~~sh
link_ctl arm -r A
/root/display-stream-new --source device --auto --scale 2 --port 18090 &
# 取到一帧后：
link_ctl ack
~~~

每次 ARM 只给一帧额度；不 ACK 会停在 HELD。详见 <code>docs/DATAFLOW.md</code> §9。

## 数据链路（当前）

~~~text
IMX415 A/B 4K RAW10
  -> checked CSI-2（ECC/VC/DT/WC/CRC/行数/FS-FE 裁决）
  -> 偶数 ROI 1920x1080（X0=960, Y0=540, SKIP_ROWS=37）
  -> cam_r10f_writer（128B 头 + packed RAW10 + 4KiB 尾零）
  -> DDR3 双缓冲 -> ddr_frame_pusher
  -> PCIe Gen2 x2 Posted MWr -> 主机一致性 DMA 槽
  -> /dev/ANLOGIC-PCI0_frame（单槽、one-shot ARM/ACK）
  -> display：R10F 解析 -> 5B/4px 解包 -> 去马赛克 -> 后处理 -> JPEG -> HTTP
~~~

槽内布局：16 B 槽头 + 112 B pad + R10F（128 B 头 + 2,592,000 B packed + 640 B 尾零）。
槽步长 2,596,864 B；单槽总分配 2,600,960 B。

## R10F 支持（2026-10-28 修复）

<code>src/r10f.rs</code> 校验 magic/version/header_bytes/format/codec/valid_bits/packing/
width/height/stride/raw_bytes/payload_bytes，并解包 5B/4px：

~~~text
b0=P0[9:2] b1=P1[9:2] b2=P2[9:2] b3=P3[9:2]
b4={P3[1:0],P2[1:0],P1[1:0],P0[1:0]}
8bit 预览 = 每个采样的高 8 位 = b0..b3
~~~

设备源按驱动报的 <code>slot_bytes</code> 读满一槽，再解析；不是 R10F 时才回落旧 8bit 路径。
文件源 <code>--source r10f --r10f f.dma</code> 走同一解析器，用于离线回归。

## 彩色相位：当前安装 A/B 均 RGGB

### 已确证

真实帧 2×2 四个格子的均值（frame A）：

~~~text
(0,0)=31.32  (0,1)=56.72
(1,0)=56.68  (1,1)=35.85
~~~

两条绿对角只差 0.07%，主对角差 13.5% —— 绿在**反对角** (0,1)/(1,0)。
马赛克里只有绿占两格，所以相位只可能是 **RGGB 或 BGGR**；
<code>GBRG</code>/<code>GRBG</code>（绿在主对角）会出品红/绿棋盘，已排除。

### 为什么取 RGGB

- A/B 两路看同一间办公室；只有 RGGB 让两边告示牌都还原成**黄色**，
  BGGR 会把它们都变蓝。同一块牌子不可能既黄又蓝，这是当前最强的一组同场景对照。
- 用户对 <code>--bayer rggb</code> 的 A 路结果确认为“有点对”。
- R10F 头 0x28 写 3(GBRG) 是 <code>SRC_BAYER_PHASE=0</code> 占位按 ROI 奇偶推导的，
  <strong>不是测量值</strong>；不要拿它当相位。

### 仍然存在的风险

早期 BGGR 结论来自 180° 翻转的旧预览/另一套安装；如果当前模组实际是倒装，R/B 会互换。
换模组、改支架、改 MIPI 走线、改 ROI 奇偶（X0/Y0/SKIP_ROWS）后必须用**已知色卡**重测。
命令行可随时覆盖：

~~~sh
display-stream --source r10f --r10f fa.dma --bayer rggb --auto
display-stream --source r10f --r10f fb.dma --bayer bggr --auto
~~~

<code>--bayer auto</code> 当前解析为 RGGB；它保留成独立名字，是为了以后换模组只改一处。

## 后处理（<code>--auto</code>，device/r10f 默认开）

| 项 | 预设 | 选项 | 说明 |
| --- | --- | --- | --- |
| 黑电平 | 12 | <code>--black 0..64</code> | IMX415 10bit pedestal 50 截成 8 位 = 12 |
| gamma | 2.2 | <code>--gamma 1.0..4.0</code> | 线性 RAW -> 显示域 |
| AE | 目标 100 | <code>--ae 0..250</code> | 只提亮（gain >= 1），不压暗正常曝光帧 |
| AWB | 灰世界 | <code>--awb</code>/<code>--no-awb</code> | 扣黑后、排除 max>=250 的像素，再算 R/B 增益 |

<code>--source device</code> / <code>--source r10f</code> 不给 <code>--auto</code> 也默认按上表开；
<code>--no-auto</code> 可退回原样透传。synthetic/pgm 默认关，仍要显式 <code>--auto</code>。

实现顺序：扣黑 -> 灰世界 AWB -> AE -> gamma LUT -> 高光去饱和 -> 4×4 有序抖动。
高光去饱和只把输出域 max>220 的像素往亮度拉，避免灯管边缘品红/绿边。

**关于“绿衣服不绿”**：这两帧里前景织物与白墙的相机色度几乎相同（差异 <2%）。
任何让白墙变白全局 AWB 都会把它还原成中性；要强行保留绿色，只能把白墙也染绿，
或做按区域/语义的白平衡。这是当前像素的事实，不是相位错。真要验证请拿已知色卡/
已知颜色物体重拍。

## 构建

~~~sh
cargo build --release
cargo build --release --target aarch64-unknown-linux-gnu
cargo test
~~~

交叉链接器见 <code>.cargo/config.toml</code>（<code>aarch64-linux-gnu-gcc</code>），
产物在 <code>target/aarch64-unknown-linux-gnu/release/display-stream</code>。

## 命令行

~~~text
--source synthetic|pgm|r10f|device   数据源，默认 synthetic
--pgm <路径>                         PGM 文件源（P5/P2 灰度）
--r10f <路径>                        R10F 文件源（.dma，从魔数开始）
--device <路径>                      设备节点，默认 /dev/ANLOGIC-PCI0_frame
--width/--height                     合成源几何
--fps <1..240>                       推流节奏，默认 15
--quality <1..100>                   JPEG 质量，默认 80
--scale <1|2|4|8>                    编码前降采样；彩色走 CFA binning
--bayer <auto|gray|rggb|bggr|grbg|gbrg>  相位，默认 auto（当前 RGGB）
--auto / --black / --gamma / --ae / --awb / --no-awb  后处理
--port <端口>                        默认 8090
--bind <地址>                        默认 0.0.0.0
--timeout-ms <毫秒>                  设备等帧超时，默认 2000
~~~

## HTTP

~~~text
/              内嵌网页（MJPEG）
/stream.mjpg   MJPEG 连续流
/snapshot.jpg  单帧 JPEG
~~~

<code>curl -o f.jpg http://<板子IP>:8090/snapshot.jpg</code> 抓一帧；响应头带帧序号/几何。

## 代码位置

~~~text
src/driver.rs        驱动 ABI：ioctl/结构体/mmap/槽读取
src/r10f.rs          R10F 头校验 + packed RAW10 解包
src/source/          synthetic / pgm / r10f_file / device
src/bayer.rs         相位枚举 + 双线性去马赛克 + CFA binning
src/auto.rs          黑电平 / 灰世界 AWB / AE / gamma / 高光去饱和 / 抖动
src/encode.rs        JPEG（灰度 Luma / 彩色 RGB）
src/http.rs store.rs 最小 HTTP 与最新帧共享区
~~~

## 已知限制

- 设备源没有常驻 ARM/ACK pump，单帧快照可用，连续流要外部循环或后续补 pump。
- 没有 CCM/镜头校正/3A 标定；颜色是“白参考 + 灰世界”级别，不是色彩管理级。
- 只取 RAW10 高 8 位，暗部有量化损失；需要 10/16bit 通路要另改 Frame 类型。
- 去马赛克是双线性，边缘有假彩色；需要 Malvar/边缘自适应可替换 <code>src/bayer.rs</code>。
- R10F 头的 bayer_phase 不可信；换硬件后必须重测相位。
