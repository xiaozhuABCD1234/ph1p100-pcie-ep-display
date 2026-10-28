# YOLO11n RKNN 模型

## 当前使用

| 项 | 值 |
|---|---|
| 文件 | models/yolo11n-640-int8.rknn |
| 大小 | 4,833,217 B |
| SHA-256 | d105e9f4765723123d9a5f1250e6e7ff6f9ca45eca418447322c68e168ae00b5 |
| 输入 | images [1,3,640,640] NCHW float32，mean=0 / std=255，RGB |
| 输出 | boxes [1,4,8400] + scores [1,80,8400]（两个输出，分开 int8 量化） |
| 类别 | COCO 80 类 |
| 转换 | Ultralytics YOLO11n → ONNX（opset 12，simplify）→ rknn-toolkit2 2.3.2，rk3588，w8a8 |

## 为什么是拆分输出的模型

Ultralytics 导出的 ONNX 默认把框坐标（0..640）和 80 类分数（0..1）拼成一个
[1,84,8400] 输出。RKNN 对整个输出做一层 int8 量化时，scale 被框坐标的尺度
主导，类别分数几乎全部被量化成 0——模型能跑、框坐标也在，但永远检测不到目标。

正确做法是在 ONNX 里把最后一层 Concat 拆开，暴露：

    boxes   [1,4,8400]
    scores  [1,80,8400]

RKNN 会为两个输出分别算量化 scale，类别分数得以保留。本目录的
models/yolo11n-640-int8.rknn 就是这种双输出模型；display-stream 的检测模块
同时支持单输出和双输出，但正式部署使用双输出。

## 重新生成

    cd display
    ./scripts/convert_yolo11n.sh /path/to/yolo11n.pt

脚本依赖：

- 宿主机 Python 3.12，ultralytics（导出 ONNX，建议 8.4.x）
- rknn-toolkit2==2.3.2（onnx==1.16.1、setuptools<80）
- COCO128 校准集（32 张即可；脚本默认用仓库外的 dataset.txt）

转换完成后把 models/yolo11n-640-int8.rknn 拷到板上 /root/models/，
并用 --rknn 指定。

## 许可

权重来自 Ultralytics YOLO11n（AGPL-3.0）。本目录不提交 .rknn/.onnx 二进制，
只保留说明和转换脚本；部署时由使用者自行生成或从训练资产复制。
