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

    ./display-stream --source device --bayer grbg --auto --fps 15

其它：

    ./display-stream                                        # 合成源 640×480、15fps、端口 8090
    ./display-stream --source pgm --pgm frame.pgm            # PGM 文件源（P5/P2 灰度图）
    ./display-stream --source device --fps 30                # 设备源，默认节点见下
    ./display-stream --source device --bayer grbg            # 只去马赛克，不做后处理
    ./display-stream --source device --auto                  # 灰度通路也能用（无 AWB）

参数：`--source synthetic|pgm|device`（默认 synthetic）、`--pgm <路径>`、
`--device <节点>`（默认 `/dev/ANLOGIC-PCI0_frame`，需先加载驱动）、
`--width`/`--height`（合成源几何）、`--fps`（默认 15）、`--quality`（1..100，默认 80）、
`--bayer <gray|rggb|bggr|grbg|gbrg>`（默认 `gray`，实测应为 `grbg`）、
`--auto` / `--black` / `--gamma` / `--ae` / `--awb` / `--no-awb`（后处理，见下节）、
`--port`（默认 8090）、`--bind`（默认 0.0.0.0）、`--timeout-ms`（设备源等帧超时，默认 2000）、
`-h`。退出码：1 用法错、2 数据源起不来、3 服务起不来、4 设备连续 IO 失败。

### `--bayer`：把帧当 Bayer 马赛克，出去马赛克的彩色图

整帧按该相位做双线性去马赛克（`src/bayer.rs`，纯 Rust、无新依赖），再编三分量
彩色 JPEG；MJPEG 与 `/snapshot.jpg` 两个端点都是彩色。`gray` 是默认值，也是旧版
行为：帧当单分量灰度图，不去马赛克。

相位实测是 **`grbg`**（次选 `gbrg`，两者只差红蓝互换），所以要彩色就写
`--bayer grbg`。

> **修正**：早期版本的 README 写的是 `bggr`/`rggb`，那次结论是**错的**，已按下面的
> 重新测量改掉。原来用的指标是「色度假彩色能量最低」，但**选错相位会把通道模糊掉，
> 反而让高频能量变低**，所以那个指标偏向错误答案（另，「绿通道均值最高」那一栏测的
> 其实是「哪个相位下绿通道的数字最大」，同样可以被错误相位刷高）。

现在的依据是 2×2 四个格子自身的**原始均值**（连抓 5 帧完全一致，静态场景）：

| 格子 | 均值 |
| --- | --- |
| (偶行, 偶列) | 19.11 |
| (奇行, 奇列) | 19.09 |
| (偶行, 奇列) | 15.74 |
| (奇行, 偶列) | 15.28 |

两条对角线的含义完全不同，而**哪条是「同一个物理通道」一目了然**：

* 主对角线 (偶,偶)+(奇,奇)：19.11 vs 19.09，差 **0.1%**；
* 反对角线 (偶,奇)+(奇,偶)：15.74 vs 15.28，差 **3%**。

同一个滤色片的两个采样点应当最接近，不同滤色片（红 vs 蓝）才会差得多。所以两个格子
属于**同一通道**的是主对角线；而马赛克里唯一占两格的通道就是绿，于是绿在**主对角线**上
—— 相位只能是 `grbg` 或 `gbrg`（这两个的绿都在主对角线，区别只是另一条对角线上
红蓝谁上谁下）。

旁证（拿 AWB 算出来的增益看，很直观）：按 `bggr`/`rggb` 去马赛克时灰世界算出的
R/B 增益**恰好相等**（0.49 / 1.00 / 0.49）—— 那正是因为它们被接到了同一条对角线上
（同一个物理通道）；按 `grbg` 则是 1.84 / 1.00 / **2.09** 这种「抬红蓝」的常规方向。

`grbg` 与 `gbrg` 逐像素只差红蓝互换，任何纯幅度指标都区分不了，要定死仍需要用
**已知颜色的物体或色卡**拍一帧。目前先取 `grbg`，依据是两条反对角均值里较大的那个
在 (偶,奇) 格上，按 `grbg` 它是红 —— 而典型 CMOS 原始响应是绿 > 红 > 蓝。

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
