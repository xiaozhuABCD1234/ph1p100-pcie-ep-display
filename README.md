# display-stream —— 取帧 → JPEG → HTTP 推流

RK3588 上把安路 PH1P100 EP 送来的帧（默认 640×480）取出来，编成 JPEG，由最小
HTTP 服务推出去（MJPEG 连续流 + 单帧快照）。默认编成**单分量灰度 JPEG**；
帧缓存里是 Bayer 马赛克时用 `--bayer <相位>` 做去马赛克，改推**三分量彩色 JPEG**。
`--auto` 一键打开主机侧后处理（黑电平扣除 + gamma + AE + AWB），把「传感器 RAW
直接显示就是又暗又偏色」这个默认状态改掉。
Rust cargo 工程，无 C/C++ 源文件。

## 构建

    cargo build --release                                # 本机原生，产物 target/release/
    cargo build --release --target aarch64-unknown-linux-gnu   # 给板子
    cargo test --release                                 # 单元测试（编码/解析/路由）

交叉编译的链接器见 `.cargo/config.toml`（`aarch64-linux-gnu-gcc`），产物在
`target/aarch64-unknown-linux-gnu/release/display-stream`。

## 运行

**本板推荐用法**（彩色 + 后处理，见下面两节的实测依据）：

    ./display-stream --source device --bayer gbrg --auto --fps 15

其它：

    ./display-stream                                        # 合成源 640×480、15fps、端口 8090
    ./display-stream --source pgm --pgm frame.pgm            # PGM 文件源（P5/P2 灰度图）
    ./display-stream --source device --fps 30                # 设备源，默认节点见下
    ./display-stream --source device --bayer gbrg            # 只去马赛克，不做后处理
    ./display-stream --source device --auto                  # 灰度通路也能用（无 AWB）

参数：`--source synthetic|pgm|device`（默认 synthetic）、`--pgm <路径>`、
`--device <节点>`（默认 `/dev/ANLOGIC-PCI0_frame`，需先加载驱动）、
`--width`/`--height`（合成源几何）、`--fps`（默认 15）、`--quality`（1..100，默认 80）、
`--bayer <gray|rggb|bggr|grbg|gbrg>`（默认 `gray`，实测应为 `gbrg`）、
`--auto` / `--black` / `--gamma` / `--ae` / `--awb` / `--no-awb`（后处理，见下节）、
`--port`（默认 8090）、`--bind`（默认 0.0.0.0）、`--timeout-ms`（设备源等帧超时，默认 2000）、
`-h`。退出码：1 用法错、2 数据源起不来、3 服务起不来、4 设备连续 IO 失败。

### `--bayer`：把帧当 Bayer 马赛克，出去马赛克的彩色图

整帧按该相位做双线性去马赛克（`src/bayer.rs`，纯 Rust、无新依赖），再编三分量
彩色 JPEG；MJPEG 与 `/snapshot.jpg` 两个端点都是彩色。`gray` 是默认值，也是旧版
行为：帧当单分量灰度图，不去马赛克。

**本板实测相位是 `gbrg`**（次选 `grbg`，两者逐像素只差红蓝互换），所以要彩色就写：

    ./display-stream --source device --bayer gbrg --auto --fps 15

> **本节的结论改过两次，记一下免得再绕**：早期写 `bggr`/`rggb`（错），后来改成
> `grbg`（还是错，只对了一半：那一步定对了「绿在哪条对角线」，但红蓝谁上谁下判反了），
> 2026-10-16 用**已知颜色的物体**才定死为 `gbrg`。证据图：
> `/tmp/fpga-rk-img/20261016-2038_*`（同一场景同参数，只换 `--bayer`）。

#### 第一步：绿在哪条对角线上（只靠数据就能定，很稳）

取 2×2 四个格子自身的**原始均值**（静止场景连抓 5 帧完全一致）：

| 格子 | 均值 |
| --- | --- |
| (偶行, 偶列) | 19.11 |
| (奇行, 奇列) | 19.09 |
| (偶行, 奇列) | 15.74 |
| (奇行, 偶列) | 15.28 |

同一个滤色片的两个采样点最接近、不同滤色片（红 vs 蓝）差得多：主对角线差 **0.1%**、
反对角线差 **3%**，所以「两格同通道」的是**主对角线**。马赛克里唯一占两格的通道是绿
→ 绿在主对角线 → 相位只能是 `grbg` 或 `gbrg`。

（**绝对值随场景变，别拿去当判据**；要看的只是「哪条对角线两格几乎相等」这个关系。
2026-10-16 换了个场景重测：主对角线 35.9 / 35.9（等），反对角线 25.5 / 23.3（差 9%）——
关系不变。）

旁证（拿 AWB 增益看很直观）：按 `bggr`/`rggb` 去马赛克时灰世界算出的 R/B 增益
**恰好相等**（0.49 / 1.00 / 0.49）—— 那正是因为它们被接到了同一条对角线上（同一个
物理通道）；按 `grbg`/`gbrg` 则是 1.4 / 1.0 / 1.5 这种「抬红蓝」的常规方向。

#### 第二步：红蓝谁上谁下 —— 只能用已知颜色的物体定

`grbg` 与 `gbrg` 逐像素只差 R/B 互换，所以**任何纯幅度指标对这两者给出完全相同的
值**（「绿均值最高」「假彩色能量最低」都不行 —— 前两次结论反复就是栽在这儿）。
唯一可靠的办法是让画面里出现**已知颜色的东西**，两个相位各出一张快照对比：

    for ph in grbg gbrg; do
      ./display-stream --source device --bayer $ph --auto --fps 2 --port 8097 &
      sleep 11; curl -s -o /tmp/cmp_$ph.jpg http://127.0.0.1:8097/snapshot.jpg
      kill %1; sleep 2
    done

本板 2026-10-16 的对照结果：

| 画面里的物体 | 实际颜色 | 按 `grbg` | 按 `gbrg` |
| --- | --- | --- | --- |
| 展示墙的白色海报/白纸 | 白 | 米黄（假） | **白（对）** |
| 设备上的黄色机柜 | 黄 | 蓝（假） | **黄（对）** |
| HMI 触摸屏（白底界面） | 白 | 米色 | **白（对）** |

通道互换会把白色以外的颜色整体翻过来，所以「白纸仍旧白、黄柜仍旧黄」的那一相位才对。

#### 关于「相机转了个方向要不要换相位」

**不用换。** 相位是**传感器 + 该路像素怎么进 FPGA** 的属性（CFA 长在传感器上、
读出顺序固定），把相机（或整台设备）转 90°/180° 只是**画面内容**转了，帧里的
马赛克排布一点没变 —— 换了朝向照样用 `--bayer gbrg`。

只有下面这些情况才需要重新验相位：换了相机模组/子板、MIPI 走线或 lane 顺序改了、
FPGA 侧的像素对齐/裁剪改动（例如 x 方向平移一个像素、或整帧镜像/转置），
以及任何改动过 `csi_rx_*` 的场合。判据还是上面两步。

已知限制：去马赛克只有双线性，没有边缘自适应，高频边缘会有假彩色锯齿；暗光下
R/B 增益拉得很大，彩色噪点会明显（那属于信号太弱，不是算法问题）。白平衡与亮度
由下面的 `--auto` 负责。

### `--auto`：主机侧后处理（黑电平 + gamma + AE + AWB）

FPGA 送过来的是**传感器 RAW**（10bit 只取高 8 位、传感器侧没有 AE/AWB/gamma），
所以不做后处理就是「很暗 + 偏色」。`--auto` 在主机侧一键补上（`src/auto.rs`）：

| 项 | `--auto` 预设 | 单项选项 | 作用 |
| --- | --- | --- | --- |
| 黑电平 | 12 | `--black <0..64>` | IMX415 的 10bit pedestal 50 截成 8 位 = 12；不扣掉 gamma 会把这段台阶整体抬起来 |
| gamma | 2.2 | `--gamma <1.0..4.0>` | 线性 RAW → 显示域 |
| AE | 目标 100 | `--ae <0..250>`（0 = 关） | 定数字增益，把 gamma 编码后的**中位数**打到目标亮度 |
| AWB | 开 | `--awb` / `--no-awb` | 灰世界白平衡，以绿为参考（只在彩色相位下有效） |

单项选项**优先于 `--auto`**，且与两者在命令行上的先后无关；不开 `--auto` 时全部
默认关（逐字节原样透传，与旧版一致）。启动日志与每 5 秒的周期日志都会打出当前生效的
黑电平/gamma/增益/白平衡系数。

实现上三个要点：

* 增益都在**线性域**算，最后合成成 `通道×16相位` 的 256 项查找表，逐像素只剩查表。
* **抖动**：8 位输入配大增益必然丢灰度级（暗帧线性值只有 0..50 级），不抖就是明显色带；
  所以查表带 4×4 有序抖动。反过来，若这张表恰好是一一映射（没丢级），就不抖。
* 增益按帧做对数域 EMA 平滑（系数 0.35）抑制闪烁，第一帧直接采用不等收敛。

注意：后处理跑在**已经截成 8 位**的数据上，大增益会成上限地放大台阶。真正想要好的
暗光彩色，得让 FPGA 把 RAW10 传过来（那是另一件事，见仓库根目录的讨论）。

### 4K（相机整帧直通 DDR3）

FPGA 侧现在能把相机整幅 3840×2160 直通到 DDR3（见 `fpga/README.md`），
主机侧只是把驱动按 4K 分配（`insmod anlogic_pci.ko frame_width=3840 frame_height=2160`）：

    ./display-stream --source device --bayer gbrg --auto --fps 2

设备源的几何是从驱动 ioctl 读的，所以**不用改本工具**；但要知道代价：
一帧 829 万像素，本工具是单线程纯 Rust，`--auto` + 去马赛克 + JPEG 编码在 RK3588 上
大概只有几 fps（`--fps` 只是上限，编不过来就以实际速度走）。想在 4K 下流畅推流，
要么加一级降采样（例如先缩到 1080p 再编码），要么接受低帧率看快照
（`curl -o f.jpg http://<板子IP>:8090/snapshot.jpg`）。
只取原始帧不走这个工具时用 `frame_grab -o f.pgm`（8.29MB/帧）。

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
    src/auto.rs          主机侧后处理：黑电平 / AWB / AE / gamma / 有序抖动（`--auto` 走这里）
    src/encode.rs        JPEG 编码（jpeg-encoder：灰度用 ColorType::Luma，彩色用 Rgb）
    src/http.rs store.rs 最小 HTTP 服务、最新帧共享区（MJPEG 靠共享区拿下一帧）
