#!/bin/sh
# 生成 YOLO11n 640 的双输出 INT8 RKNN 模型。
#
# 用法：
#   PY_EXPORT=/path/to/ultralytics/venv/bin/python \
#   PY_RKNN=/path/to/rknn-toolkit2/venv/bin/python \
#   DATASET=/path/to/dataset.txt \
#   ./scripts/convert_yolo11n.sh /path/to/yolo11n.pt
#
# 产物：models/yolo11n-640-int8.rknn
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
PT=${1:?用法: convert_yolo11n.sh <yolo11n.pt>}
PY_EXPORT=${PY_EXPORT:-python3}
PY_RKNN=${PY_RKNN:-python3}
DATASET=${DATASET:-}
WORK="$ROOT/models/.build"
mkdir -p "$WORK"

echo "[1/3] 导出 ONNX"
"$PY_EXPORT" - "$PT" "$WORK/yolo11n-640.onnx" <<'PY'
import sys
from ultralytics import YOLO
src, out = sys.argv[1], sys.argv[2]
model = YOLO(src)
p = model.export(format='onnx', imgsz=640, opset=12, simplify=True, dynamic=False, batch=1)
import os, shutil
shutil.move(p, out)
print('onnx:', out)
PY

echo "[2/3] 拆分 boxes/scores 两个输出"
"$PY_RKNN" - "$WORK/yolo11n-640.onnx" "$WORK/yolo11n-640-split.onnx" <<'PY'
import sys, onnx
from onnx import helper, TensorProto
src, dst = sys.argv[1], sys.argv[2]
m = onnx.load(src)
last = m.graph.node[-1]
assert last.op_type == 'Concat' and last.output[0] == 'output0', (last.op_type, last.output)
boxes_src, scores_src = last.input[0], last.input[1]
m.graph.node.remove(last)
m.graph.node.extend([
    helper.make_node('Identity', [boxes_src], ['boxes'], name='boxes_out'),
    helper.make_node('Identity', [scores_src], ['scores'], name='scores_out'),
])
while len(m.graph.output):
    m.graph.output.pop()
m.graph.output.extend([
    helper.make_tensor_value_info('boxes', TensorProto.FLOAT, [1, 4, 8400]),
    helper.make_tensor_value_info('scores', TensorProto.FLOAT, [1, 80, 8400]),
])
m = onnx.shape_inference.infer_shapes(m)
onnx.checker.check_model(m)
onnx.save(m, dst)
print('split onnx:', dst)
PY

if [ -z "$DATASET" ] || [ ! -f "$DATASET" ]; then
  echo "需要 DATASET=<量化校准集 dataset.txt>" >&2
  exit 2
fi

echo "[3/3] 转 RKNN（w8a8，双输出）"
"$PY_RKNN" - "$WORK/yolo11n-640-split.onnx" "$ROOT/models/yolo11n-640-int8.rknn" "$DATASET" <<'PY'
import sys
from rknn.api import RKNN
src, dst, dataset = sys.argv[1], sys.argv[2], sys.argv[3]
r = RKNN(verbose=False)
assert r.config(mean_values=[[0,0,0]], std_values=[[255,255,255]],
                target_platform='rk3588', quantized_dtype='w8a8',
                quantized_algorithm='normal', optimization_level=3) == 0
assert r.load_onnx(model=src) == 0
assert r.build(do_quantization=True, dataset=dataset) == 0
assert r.export_rknn(dst) == 0
r.release()
print('rknn:', dst)
PY

sha256sum "$ROOT/models/yolo11n-640-int8.rknn"
