#!/usr/bin/env python3
"""以真实模型前缀激活和 ORT 参考输出核验独立 Rust 内核，并比较热调用耗时。

只使用本地模型与开发依赖，不下载资产、不构建应用。Rust 内核从同一份
src/ppocr/kernels.rs 直接编译；推荐 --kernel-dll 与客户端 ORT DLL 同进程对照，
--binary 用于独立单元测试程序。编译命令见 OCR 性能专题。
"""
from __future__ import annotations

import argparse
import ctypes
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
import time


def imports(project: Path):
    # 可选的局部开发环境，避免修改客户端／应用依赖。
    local = project / "target" / "ocr-kernel-python"
    if local.is_dir():
        sys.path.insert(0, str(local))
    import numpy as np
    import onnxruntime as ort
    if ort.__version__ != "1.24.4":
        raise ValueError(f"需要 ORT 1.24.4，实际 {ort.__version__}")
    return np, ort


def session(path, threads, optimize, ort):
    options = ort.SessionOptions()
    options.intra_op_num_threads = threads
    options.inter_op_num_threads = 1
    options.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
    options.graph_optimization_level = (
        ort.GraphOptimizationLevel.ORT_ENABLE_ALL if optimize else ort.GraphOptimizationLevel.ORT_DISABLE_ALL
    )
    options.add_session_config_entry("session.intra_op.allow_spinning", "0")
    return ort.InferenceSession(str(path), options, providers=["CPUExecutionProvider"])


def source(shape, pattern, np):
    count = int(np.prod(shape))
    if pattern == "zero":
        return np.zeros(shape, dtype=np.float32)
    if pattern == "ramp":
        row = np.arange(shape[3], dtype=np.float32) / np.float32(shape[3]) * np.float32(4) - np.float32(2)
        return np.broadcast_to(row, shape).copy()
    def random_values():
        seed = 0x14151617
        for _ in range(count):
            seed = (seed * 1664525 + 1013904223) & 0xFFFFFFFF
            yield seed >> 8
    data = np.fromiter(random_values(), dtype=np.uint32, count=count).astype(np.float32)
    return (data / np.float32(1 << 22) - np.float32(2)).reshape(shape)


def prepare(root, output, np, ort):
    indexes = json.loads((root / "cases.json").read_text(encoding="utf-8"))
    if len(indexes) != 3:
        raise ValueError("独立 Rust 微基准目前只支持固定 Tiny det 的三组子图")
    prepared = []
    for index, relative in enumerate(indexes):
        directory = (root / relative).parent
        case = json.loads((directory / "case.json").read_text(encoding="utf-8"))
        p = case["layers"][0]["params"]
        q = case["layers"][1]["params"]
        signature = (p["batch"], p["in_channels"], p["height"], p["width"], p["kernel_h"], p["stride_h"], q["out_channels"])
        expected = [(1, 32, 184, 184, 3, 1, 64), (1, 32, 184, 184, 3, 2, 64), (1, 64, 23, 23, 5, 1, 16)][index]
        if signature != expected or p["kernel_w"] != p["kernel_h"] or p["stride_w"] != p["stride_h"]:
            raise ValueError(f"case-{index:02} 不是固定形状，不能使用此独立微基准")
        prefix = session(directory / case["prefix_model"], 1, True, ort)
        reference = session(directory / case["pair_model"], 1, False, ort)
        values = output / f"case-{index:02}"
        values.mkdir(parents=True, exist_ok=True)
        measured_input = None
        shapes = None
        for pattern in ("random", "zero", "ramp"):
            pixels = source(case["original_input_shape"], pattern, np)
            activation = prefix.run(None, {case["prefix_input_name"]: pixels})[0]
            expected_outputs = reference.run(case["output_names"], {case["input_name"]: activation})
            if tuple(activation.shape) != tuple(case["input_shape"]):
                raise ValueError("前缀输出形状不符")
            for label, data in zip(("input", "dw", "pw"), (activation, *expected_outputs)):
                if not np.isfinite(data).all():
                    raise ValueError("参考输入或输出包含非有限值")
                np.asarray(data, dtype="<f4").tofile(values / f"{pattern}-{label}.f32")
            if pattern == "random":
                measured_input = np.ascontiguousarray(activation)
                shapes = [data.shape for data in expected_outputs]
        candidates = []
        for threads in (1, 4):
            engine = session(directory / case["pair_model"], threads, True, ort)
            binding = engine.io_binding()
            binding.bind_cpu_input(case["input_name"], measured_input)
            buffers = [np.empty(shape, dtype=np.float32) for shape in shapes]
            for name, buffer in zip(case["output_names"], buffers):
                binding.bind_output(name, "cpu", 0, np.float32, buffer.shape, buffer.ctypes.data)
            engine.run_with_iobinding(binding)
            expected_outputs = reference.run(case["output_names"], {case["input_name"]: measured_input})
            for actual, expected_output in zip(buffers, expected_outputs):
                if not np.all(np.abs(actual - expected_output) <= 1e-5 + 1e-4 * np.abs(expected_output)):
                    raise ValueError("ORT 优化/预绑定输出与禁优化参考不符")
            # CPU 原地写入的 buffers 与输入必须在所有计时调用期间存活。
            candidates.append((engine, binding, buffers, measured_input))
        prepared.append(candidates)
    return prepared


def measure(run):
    for _ in range(3):
        run()
    samples = []
    for _ in range(15):
        start = time.perf_counter()
        run()
        first = time.perf_counter() - start
        iterations = max(1, min(64, int(0.003 / max(first, 1e-9)) + 1))
        start = time.perf_counter()
        for _ in range(iterations):
            run()
        samples.append((time.perf_counter() - start) * 1000 / iterations)
    return statistics.median(samples)


def in_process(root, output, kernel_dll, runtime_dll, np):
    """两个引擎在同一 Python 进程中运行；ORT 使用客户端实际的 DLL。"""
    from ocr_kernel_native import NativeOrt
    dll = ctypes.CDLL(str(kernel_dll), winmode=0x100 | 0x800)
    invoke = dll.rustshot_ocr_probe_conv
    fp = ctypes.POINTER(ctypes.c_float)
    size = ctypes.c_size_t
    invoke.argtypes = [ctypes.POINTER(size), size, fp, size, fp, size, fp, size, fp, size]
    invoke.restype = ctypes.c_int32
    fields = ("batch", "in_channels", "height", "width", "out_channels", "kernel_h", "kernel_w",
              "stride_h", "stride_w", "pad_top", "pad_left", "pad_bottom", "pad_right", "groups")

    def convolution(layer, directory, source, destination):
        params = (size * 14)(*(layer["params"][key] for key in fields))
        weights = np.fromfile(directory / layer["weights"], dtype="<f4")
        bias = np.fromfile(directory / layer["bias"], dtype="<f4") if layer["bias"] else None
        args = (params, 14, source.ctypes.data_as(fp), source.size,
                weights.ctypes.data_as(fp), weights.size,
                bias.ctypes.data_as(fp) if bias is not None else fp(), 0 if bias is None else bias.size,
                destination.ctypes.data_as(fp), destination.size)
        def run():
            code = invoke(*args)
            if code < 0:
                raise RuntimeError(f"Rust 内核错误 {code}")
        run.keep_alive = (params, weights, bias, source, destination)
        return run

    def one_sample(run):
        start = time.perf_counter()
        run()
        seconds = time.perf_counter() - start
        iterations = max(1, min(64, int(0.003 / max(seconds, 1e-9)) + 1))
        start = time.perf_counter()
        for _ in range(iterations):
            run()
        return (time.perf_counter() - start) * 1000 / iterations

    indexes = json.loads((root / "cases.json").read_text(encoding="utf-8"))
    with NativeOrt(runtime_dll) as native:
        print(f"同进程实测：Rust DLL={kernel_dll.name}, 客户端 ORT DLL={runtime_dll}; Level3/CPU/spin=off，预分配双输出，轮转18轮，前3轮不计", flush=True)
        for index, relative in enumerate(indexes):
            directory = (root / relative).parent
            case = json.loads((directory / "case.json").read_text(encoding="utf-8"))
            values = output / f"case-{index:02}"
            random = np.fromfile(values / "random-input.f32", dtype="<f4").reshape(case["input_shape"])
            dw_params = case["layers"][0]["params"]
            pw_params = case["layers"][1]["params"]
            dw_shape = [dw_params["batch"], dw_params["out_channels"], pw_params["height"], pw_params["width"]]
            pw_shape = [pw_params["batch"], pw_params["out_channels"], pw_params["height"], pw_params["width"]]
            source_buffer = random.copy()
            first, second = np.empty(dw_shape, dtype=np.float32), np.empty(pw_shape, dtype=np.float32)
            dw = convolution(case["layers"][0], directory, source_buffer, first)
            pw = convolution(case["layers"][1], directory, first, second)
            def pair():
                dw()
                pw()
            runners = [native.create_runner(directory / case["pair_model"], threads, source_buffer,
                                             (first, second), case["input_name"], case["output_names"])
                       for threads in (1, 4)]
            try:
                max_abs = 0.0
                for pattern in ("random", "zero", "ramp"):
                    np.copyto(source_buffer, np.fromfile(values / f"{pattern}-input.f32", dtype="<f4").reshape(source_buffer.shape))
                    expected = [np.fromfile(values / f"{pattern}-{name}.f32", dtype="<f4").reshape(shape)
                                for name, shape in zip(("dw", "pw"), (dw_shape, pw_shape))]
                    for run in (pair, *runners):
                        run()
                        for actual, reference in zip((first, second), expected):
                            difference = np.abs(actual - reference)
                            max_abs = max(max_abs, float(difference.max()))
                            if not np.isfinite(actual).all() or not np.all(difference <= 1e-5 + 1e-4 * np.abs(reference)):
                                raise ValueError(f"case-{index:02}/{pattern} 数值对照失败")
                np.copyto(source_buffer, random)
                engines = (pair, *runners, dw, pw)
                samples = [[] for _ in engines]
                for round in range(18):
                    for offset in range(len(engines)):
                        slot = (round + offset) % len(engines)
                        ms = one_sample(engines[slot])
                        if round >= 3:
                            samples[slot].append(ms)
                medians = [statistics.median(sample) for sample in samples]
                rust, one, four, dw_ms, pw_ms = medians
                print(f"native case-{index:02} max_abs={max_abs:.3g} rust_pair_ms={rust:.4f} client_ort1_ms={one:.4f} client_ort4_ms={four:.4f} rust_dw_ms={dw_ms:.4f} rust_pw_ms={pw_ms:.4f} ort1_over_rust={one/rust:.2f}", flush=True)
            finally:
                for runner in runners:
                    runner.close()


def main():
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=Path, default=Path("target/ocr-kernel-probe/det"))
    parser.add_argument("--values", type=Path, default=Path("target/ocr-kernel-values"))
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument("--binary", type=Path)
    target.add_argument("--kernel-dll", type=Path, help="推荐：与客户端 ORT DLL 同进程轮转计时")
    parser.add_argument("--runtime-dll", type=Path, help="客户端 ORT DLL，默认使用已存在的标准 Tiny 缓存")
    parser.add_argument("--cpu-mask", type=lambda value: int(value, 0), help="仅限制当前基准进程的 Windows CPU 亲和性，如 0x55")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--time-python-ort", action="store_true", help="另外测 PyPI 内嵌 ORT；它与客户端 DLL 是不同构建，不能直接替代客户端基线")
    args = parser.parse_args()
    if not 1 <= args.rounds <= 9:
        parser.error("--rounds 必须在 1..9")
    project = Path(__file__).resolve().parent.parent
    if args.cpu_mask is not None:
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.GetCurrentProcess.restype = ctypes.c_void_p
        kernel32.SetProcessAffinityMask.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
        if not kernel32.SetProcessAffinityMask(kernel32.GetCurrentProcess(), args.cpu_mask):
            raise ctypes.WinError(ctypes.get_last_error())
        print(f"基准进程 CPU affinity={args.cpu_mask:#x}", flush=True)
    np, ort = imports(project)
    root, output = args.cases.resolve(), args.values.resolve()
    prepared = prepare(root, output, np, ort)
    if args.kernel_dll:
        runtime = args.runtime_dll or (Path(os.environ["LOCALAPPDATA"]) / "RaiShot/ocr/cpu-api24-tiny-1/onnxruntime-cpu-1.24.4-win-x64/onnxruntime.dll")
        in_process(root, output, args.kernel_dll.resolve(), runtime.resolve(), np)
        return
    binary = args.binary.resolve()
    print("ORT=1.24.4 CPU oracle=Disable；真实前缀三种输入；Rust计时不含加载、前缀、校验、文件及进程创建。PyPI内嵌ORT与客户端DLL是不同构建，默认只作数值oracle。", flush=True)
    env = os.environ.copy()
    env["RUSTSHOT_OCR_KERNEL_DIR"] = str(root)
    env["RUSTSHOT_OCR_KERNEL_VALUES"] = str(output)
    rust_samples = [[] for _ in range(3)]
    ort_samples = [[[], []] for _ in range(3)]
    flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    def native():
        process = subprocess.run([str(binary), "--ignored", "--nocapture", "--test-threads=1", "kernels_microbench_real_weights"],
                                 cwd=project, env=env, capture_output=True, text=True, encoding="utf-8", creationflags=flags)
        print(process.stdout, end="", flush=True)
        if process.returncode != 0:
            print(process.stderr, file=sys.stderr)
            raise RuntimeError("Rust 数值检查或微基准失败")
        if len(re.findall(r"oracle_patterns=3", process.stdout)) != 3:
            raise ValueError("Rust 程序未执行三组 ORT 数值校验，请重新编译当前源码")
        matches = re.findall(r"case-(\d+) dw_ms=([\d.]+) pw_ms=([\d.]+) pair_ms=([\d.]+)", process.stdout)
        if len(matches) != 3:
            raise ValueError("Rust 程序未输出三组计时")
        for index, dw, pw, pair in matches:
            rust_samples[int(index)].append(tuple(map(float, (dw, pw, pair))))
    def reference_timing():
        for index, candidates in enumerate(prepared):
            for slot, (engine, binding, _, _) in enumerate(candidates):
                ort_samples[index][slot].append(measure(lambda: engine.run_with_iobinding(binding)))
    for round in range(args.rounds):
        if round % 2 == 0:
            native()
            if args.time_python_ort:
                reference_timing()
        else:
            if args.time_python_ort:
                reference_timing()
            native()
    for index, samples in enumerate(rust_samples):
        dw, pw, pair = (statistics.median(row[column] for row in samples) for column in range(3))
        extra = ""
        if args.time_python_ort:
            one, four = (statistics.median(samples) for samples in ort_samples[index])
            extra = f" pypi_ort1_bound_ms={one:.4f} pypi_ort4_bound_ms={four:.4f} pypi_ort1_over_rust={one/pair:.2f}"
        print(f"summary case-{index:02} rust_dw_ms={dw:.4f} rust_pw_ms={pw:.4f} rust_pair_ms={pair:.4f}{extra}")


if __name__ == "__main__":
    main()
