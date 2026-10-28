#!/usr/bin/env python3
"""从本地 ONNX 导出真实 depthwise → pointwise Conv 对照用例。

开发依赖：python -m pip install onnx numpy
检测模型示例：python Tools/prepare-ocr-kernel-probe.py --model det.onnx \
    --output target/ocr-kernel-probe/det --input-shape 1,3,736,736
识别模型可传 --input-shape 1,3,48,320；没有直接相邻的受支持卷积时会报错。
本工具不下载模型，只接收内嵌权重的模型。产物包含子图、权重和 case.json。
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path


def parse_shape(value: str) -> list[int]:
    try:
        shape = [int(part.strip()) for part in value.split(",")]
    except ValueError as error:
        raise argparse.ArgumentTypeError("输入形状必须是 N,C,H,W 四个正整数") from error
    if len(shape) != 4 or any(part <= 0 for part in shape):
        raise argparse.ArgumentTypeError("输入形状必须是 N,C,H,W 四个正整数")
    return shape


def require_local_file(path: Path) -> None:
    info = path.stat()
    # Windows 的 Offline / RecallOnOpen / RecallOnDataAccess 不能触发按需下载。
    if getattr(info, "st_file_attributes", 0) & (0x1000 | 0x40000 | 0x400000):
        raise ValueError(f"跳过云端占位文件：{path}")
    if not path.is_file():
        raise ValueError(f"不是普通文件：{path}")


def is_link_directory(path: Path) -> bool:
    return path.is_symlink() or (hasattr(path, "is_junction") and path.is_junction()) or (
        path.exists() and bool(getattr(path.lstat(), "st_file_attributes", 0) & 0x400)
    )


def check_output_directory(output: Path, count: int) -> None:
    """只允许空目录或本工具已经生成的目录，避免覆盖用户的其它产物。"""
    if is_link_directory(output):
        raise ValueError(f"输出路径不能是链接或联接点：{output}")
    if not output.exists():
        return
    if not output.is_dir():
        raise ValueError(f"输出路径必须是普通目录：{output}")
    entries = list(output.iterdir())
    if not entries:
        return
    manifest = output / "cases.json"
    if is_link_directory(manifest):
        raise ValueError(f"用例索引不能是链接：{manifest}")
    require_local_file(manifest)
    previous = json.loads(manifest.read_text(encoding="utf-8"))
    if not isinstance(previous, list) or not previous:
        raise ValueError(f"输出目录中没有本工具的有效 cases.json：{output}")
    case_dirs = set()
    for item in previous:
        if not isinstance(item, str) or not re.fullmatch(r"case-\d{2}/case\.json", item):
            raise ValueError(f"输出目录的索引格式不匹配：{output}")
        case_dirs.add(item.split("/")[0])
    if case_dirs != {f"case-{index:02}" for index in range(count)}:
        raise ValueError("本次与已有用例数量不同，请指定空输出目录")
    if {entry.name for entry in entries} != case_dirs | {"cases.json"}:
        raise ValueError(f"输出目录含有其它文件，请指定空目录：{output}")
    allowed = {"case.json", "prefix.onnx", "pair.onnx"}
    allowed |= {f"{layer}-{kind}.f32" for layer in ("dw", "pw") for kind in ("weights", "bias")}
    for name in case_dirs:
        directory = output / name
        if is_link_directory(directory) or not directory.is_dir():
            raise ValueError(f"用例目录不能是链接：{directory}")
        for child in directory.iterdir():
            if is_link_directory(child) or child.name not in allowed:
                raise ValueError(f"用例目录含有其它文件：{child}")
            require_local_file(child)
        case_path = directory / "case.json"
        case = json.loads(case_path.read_text(encoding="utf-8"))
        if case.get("prefix_model") != "prefix.onnx" or case.get("pair_model") != "pair.onnx":
            raise ValueError(f"不是本工具的用例：{case_path}")


def attributes(node, onnx) -> dict:
    return {item.name: onnx.helper.get_attribute_value(item) for item in node.attribute}


def conv_parameters(node, initializers, shapes, onnx):
    if node.op_type != "Conv" or len(node.input) < 2 or len(node.output) != 1:
        return None
    weight = initializers.get(node.input[1])
    shape = shapes.get(node.input[0])
    if weight is None or len(weight.dims) != 4 or shape is None or len(shape) != 4:
        return None
    if weight.data_type != onnx.TensorProto.FLOAT or any(part <= 0 for part in shape):
        return None
    attrs = attributes(node, onnx)
    if attrs.get("auto_pad", b"NOTSET") not in (b"NOTSET", b""):
        return None
    if list(attrs.get("dilations", [1, 1])) != [1, 1]:
        return None
    kernel_h, kernel_w = weight.dims[2:]
    if list(attrs.get("kernel_shape", [kernel_h, kernel_w])) != [kernel_h, kernel_w]:
        return None
    strides = list(attrs.get("strides", [1, 1]))
    pads = list(attrs.get("pads", [0, 0, 0, 0]))
    groups = attrs.get("group", 1)
    if len(strides) != 2 or any(value not in (1, 2) for value in strides):
        return None
    if len(pads) != 4 or any(value < 0 for value in pads) or groups <= 0:
        return None
    batch, channels, height, width = shape
    out_channels = weight.dims[0]
    if channels != weight.dims[1] * groups or out_channels % groups:
        return None
    if len(node.input) > 3 or (len(node.input) == 3 and node.input[2] not in initializers):
        return None
    if len(node.input) == 3:
        bias = initializers[node.input[2]]
        if bias.data_type != onnx.TensorProto.FLOAT or list(bias.dims) != [out_channels]:
            return None
    output_height = (height + pads[0] + pads[2] - kernel_h) // strides[0] + 1
    output_width = (width + pads[1] + pads[3] - kernel_w) // strides[1] + 1
    expected = [batch, out_channels, output_height, output_width]
    if output_height <= 0 or output_width <= 0 or shapes.get(node.output[0]) != expected:
        return None
    return dict(
        batch=batch, in_channels=channels, height=height, width=width,
        out_channels=out_channels, kernel_h=kernel_h, kernel_w=kernel_w,
        stride_h=strides[0], stride_w=strides[1],
        pad_top=pads[0], pad_left=pads[1], pad_bottom=pads[2], pad_right=pads[3],
        groups=groups,
    )


def select_pairs(model, onnx):
    values = list(model.graph.input) + list(model.graph.value_info) + list(model.graph.output)
    shapes = {
        value.name: [dimension.dim_value for dimension in value.type.tensor_type.shape.dim]
        for value in values
        if value.type.HasField("tensor_type")
    }
    initializers = {value.name: value for value in model.graph.initializer}
    consumers = {}
    for node in model.graph.node:
        for name in node.input:
            consumers.setdefault(name, []).append(node)
    representatives = {}
    for dw in model.graph.node:
        dp = conv_parameters(dw, initializers, shapes, onnx)
        if dp is None or dp["groups"] != dp["in_channels"] or dp["out_channels"] != dp["in_channels"]:
            continue
        if dp["kernel_h"] != dp["kernel_w"] or dp["kernel_h"] not in (3, 5):
            continue
        for pw in consumers.get(dw.output[0], []):
            if pw.input[0] != dw.output[0]:
                continue
            pp = conv_parameters(pw, initializers, shapes, onnx)
            if pp is None or pp["groups"] != 1:
                continue
            if (pp["kernel_h"], pp["kernel_w"], pp["stride_h"], pp["stride_w"]) != (1, 1, 1, 1):
                continue
            if any(pp[key] for key in ("pad_top", "pad_left", "pad_bottom", "pad_right")):
                continue
            key = (dp["kernel_h"], dp["stride_h"], dp["stride_w"])
            representatives.setdefault(key, (dw, dp, pw, pp))
    priorities = [(3, 1, 1), (3, 2, 2), (5, 1, 1), (5, 2, 2)]
    keys = [key for key in priorities if key in representatives]
    keys += sorted(set(representatives) - set(keys))
    return [representatives[key] for key in keys[:4]], initializers


def save_subgraph(model, inputs, outputs, path, onnx) -> None:
    extracted = onnx.utils.Extractor(model).extract_model(inputs, outputs)
    extracted.ir_version = model.ir_version
    # Extractor 可能附加 local functions；原节点、权重与 opset 均沿用模型。
    onnx.checker.check_model(extracted)
    onnx.save_model(extracted, path, save_as_external_data=False)


def reject_external_tensors(message, onnx) -> None:
    """覆盖 initializer、Constant 属性、子图与 local function 内的所有张量。"""
    if isinstance(message, onnx.TensorProto):
        if message.data_location == onnx.TensorProto.EXTERNAL or message.external_data:
            raise ValueError("本原型仅支持内嵌权重；拒绝读取 external_data 模型")
        return
    for field, value in message.ListFields():
        if field.message_type is None:
            continue
        if field.is_repeated:
            for nested in value:
                reject_external_tensors(nested, onnx)
        else:
            reject_external_tensors(value, onnx)


def export(args) -> list[str]:
    try:
        import numpy as np
        import onnx
        import onnx.utils
    except ImportError as error:
        raise ValueError("缺少开发依赖；请先 python -m pip install onnx numpy") from error
    model_path = args.model.resolve()
    require_local_file(model_path)
    model = onnx.load_model(model_path, load_external_data=False)
    reject_external_tensors(model, onnx)
    original_ir_version = model.ir_version
    initializer_names = {tensor.name for tensor in model.graph.initializer}
    inputs = [value for value in model.graph.input if value.name not in initializer_names]
    if len(inputs) != 1 or inputs[0].type.tensor_type.elem_type != onnx.TensorProto.FLOAT:
        raise ValueError("仅支持单个 float32 NCHW 输入的模型")
    if len(inputs[0].type.tensor_type.shape.dim) != 4:
        raise ValueError("模型输入必须是四维 NCHW")
    for dimension, size in zip(inputs[0].type.tensor_type.shape.dim, args.input_shape):
        if dimension.HasField("dim_value") and dimension.dim_value > 0 and dimension.dim_value != size:
            raise ValueError(f"--input-shape 与模型固定维度 {dimension.dim_value} 不符")
        dimension.dim_value = size
    # 避免旧的动态或其它尺寸 value_info 污染本次固定形状推断。
    del model.graph.value_info[:]
    for output in model.graph.output:
        output.type.tensor_type.ClearField("shape")
    model = onnx.shape_inference.infer_shapes(model, strict_mode=True, data_prop=True)
    pairs, initializers = select_pairs(model, onnx)
    if not pairs:
        raise ValueError("没有直接相邻且受支持的 depthwise 3×3/5×5 → pointwise 1×1 Conv；未写入输出")
    check_output_directory(args.output, len(pairs))
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    manifest = []
    for index, (dw, dp, pw, pp) in enumerate(pairs):
        directory = output / f"case-{index:02}"
        directory.mkdir(exist_ok=True)
        save_subgraph(model, [inputs[0].name], [dw.input[0]], directory / "prefix.onnx", onnx)
        save_subgraph(model, [dw.input[0]], [dw.output[0], pw.output[0]], directory / "pair.onnx", onnx)
        layers = []
        for label, node, params in (("dw", dw, dp), ("pw", pw, pp)):
            weight_file = f"{label}-weights.f32"
            weights = onnx.numpy_helper.to_array(initializers[node.input[1]])
            np.asarray(weights, dtype="<f4").tofile(directory / weight_file)
            bias_file = None
            if len(node.input) == 3:
                bias_file = f"{label}-bias.f32"
                bias = onnx.numpy_helper.to_array(initializers[node.input[2]])
                np.asarray(bias, dtype="<f4").tofile(directory / bias_file)
            layers.append(dict(name=node.name or node.output[0], params=params, weights=weight_file, bias=bias_file))
        case = dict(
            prefix_model="prefix.onnx", pair_model="pair.onnx",
            original_input_shape=args.input_shape, prefix_input_name=inputs[0].name,
            input_name=dw.input[0], output_names=[dw.output[0], pw.output[0]],
            input_shape=[dp[key] for key in ("batch", "in_channels", "height", "width")],
            layers=layers,
        )
        (directory / "case.json").write_text(json.dumps(case, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        manifest.append(f"{directory.name}/case.json")
        print(f"{directory.name}: {layers[0]['name']} → {layers[1]['name']}; "
              f"input={case['input_shape']}, kernel={dp['kernel_h']}×{dp['kernel_w']}, "
              f"stride={dp['stride_h']}×{dp['stride_w']}, PW channels={pp['out_channels']}")
    (output / "cases.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    assert model.ir_version == original_ir_version
    return manifest


def main() -> int:
    # Windows 在重定向终端时可能采用 ANSI 编码，中文摘要也统一用 UTF-8。
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--model", type=Path, required=True, help="已有的本地 ONNX 模型")
    parser.add_argument("--output", type=Path, required=True, help="空目录或本工具已有的输出目录")
    parser.add_argument("--input-shape", type=parse_shape, default=[1, 3, 736, 736], help="固定 N,C,H,W；默认 1,3,736,736")
    args = parser.parse_args()
    try:
        manifest = export(args)
    except (ValueError, OSError, RuntimeError) as error:
        print(f"导出失败：{error}", file=sys.stderr)
        return 1
    print(f"已导出 {len(manifest)} 个用例：{args.output / 'cases.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
