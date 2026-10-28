"""OCR 算子实验：直接使用客户端固定的 ORT DLL，不使用 Python wheel 的引擎。"""

from __future__ import annotations

import ctypes as C
import os
from pathlib import Path
from typing import Sequence
import weakref

import numpy as np


class OrtError(RuntimeError):
    pass


# API 24 的 OrtApi 指针表；索引核对自 ort-sys 2.0.0-rc.13 的 repr(C) 布局。
# 只支持 Windows x64，枚举参数/错误码均按 ABI 的 int32 传递。
_API_INDEX = {
    "GetErrorCode": 1,
    "GetErrorMessage": 2,
    "CreateEnv": 3,
    "CreateSession": 7,
    "Run": 9,
    "CreateSessionOptions": 10,
    "SetSessionExecutionMode": 13,
    "SetSessionGraphOptimizationLevel": 23,
    "SetIntraOpNumThreads": 24,
    "SetInterOpNumThreads": 25,
    "CreateTensorWithDataAsOrtValue": 49,
    "CreateCpuMemoryInfo": 69,
    "ReleaseEnv": 92,
    "ReleaseStatus": 93,
    "ReleaseMemoryInfo": 94,
    "ReleaseSession": 95,
    "ReleaseValue": 96,
    "ReleaseSessionOptions": 100,
    "AddSessionConfigEntry": 130,
}
_P = C.c_void_p
_PP = C.POINTER(_P)
_I32 = C.c_int32
_SIZE = C.c_size_t
_I64P = C.POINTER(C.c_int64)
_CHARPP = C.POINTER(C.c_char_p)


class _ApiBase(C.Structure):
    _fields_ = [("GetApi", _P), ("GetVersionString", _P)]


class NativeOrt:
    """持有固定 DLL 与环境；close() 会先关闭仍存活的 Runner。"""

    def __init__(self, runtime_path: Path):
        if os.name != "nt" or C.sizeof(_P) != 8:
            raise RuntimeError("本实验入口仅支持 Windows x64")
        self.runtime_path = Path(runtime_path).resolve(strict=True)
        self._env = _P()
        self._runners = weakref.WeakSet()
        # LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32。
        self._dll = C.WinDLL(str(self.runtime_path), winmode=0x100 | 0x800)
        self._dll.OrtGetApiBase.argtypes = []
        self._dll.OrtGetApiBase.restype = C.POINTER(_ApiBase)
        base = self._dll.OrtGetApiBase()
        if not base:
            raise OrtError("OrtGetApiBase 返回空指针")
        get_version = C.WINFUNCTYPE(C.c_char_p)(base.contents.GetVersionString)
        self.version = get_version().decode("utf-8")
        if self.version != "1.24.4":
            raise OrtError(f"需要客户端 ORT 1.24.4，实际 {self.version}")
        get_api = C.WINFUNCTYPE(_P, C.c_uint32)(base.contents.GetApi)
        api = get_api(24)
        if not api:
            raise OrtError("客户端 ORT 不支持 API 24")
        self._api = C.cast(api, _PP)
        self._functions = {}
        self._error_code = self._bind("GetErrorCode", _I32, _P)
        self._error_message = self._bind("GetErrorMessage", C.c_char_p, _P)
        self._release_status = self._bind("ReleaseStatus", None, _P)
        self._release_env = self._bind("ReleaseEnv", None, _P)
        self._release_options = self._bind("ReleaseSessionOptions", None, _P)
        self._release_session = self._bind("ReleaseSession", None, _P)
        self._release_value = self._bind("ReleaseValue", None, _P)
        self._release_memory = self._bind("ReleaseMemoryInfo", None, _P)
        self._create_options = self._bind("CreateSessionOptions", _P, _PP)
        self._execution = self._bind("SetSessionExecutionMode", _P, _P, _I32)
        self._optimization = self._bind("SetSessionGraphOptimizationLevel", _P, _P, _I32)
        self._intra = self._bind("SetIntraOpNumThreads", _P, _P, _I32)
        self._inter = self._bind("SetInterOpNumThreads", _P, _P, _I32)
        self._config = self._bind("AddSessionConfigEntry", _P, _P, C.c_char_p, C.c_char_p)
        self._create_session = self._bind("CreateSession", _P, _P, C.c_wchar_p, _P, _PP)
        self._create_memory = self._bind("CreateCpuMemoryInfo", _P, _I32, _I32, _PP)
        self._create_tensor = self._bind(
            "CreateTensorWithDataAsOrtValue", _P, _P, _P, _SIZE, _I64P, _SIZE, _I32, _PP
        )
        self._run = self._bind("Run", _P, _P, _P, _CHARPP, _PP, _SIZE, _CHARPP, _SIZE, _PP)
        create_env = self._bind("CreateEnv", _P, _I32, C.c_char_p, _PP)
        try:
            self._check(create_env(3, b"rustshot-kernel-probe", C.byref(self._env)))
        except BaseException:
            self.close()
            raise

    def _bind(self, name, result, *arguments):
        address = self._api[_API_INDEX[name]]
        if not address:
            raise OrtError(f"ORT API 24 字段为空：{name}")
        function = C.WINFUNCTYPE(result, *arguments)(address)
        self._functions[name] = function
        return function

    def _check(self, status):
        if status:
            try:
                code = self._error_code(status)
                message = self._error_message(status)
                text = message.decode("utf-8", errors="replace") if message else "未知错误"
            finally:
                self._release_status(status)
            raise OrtError(f"ORT {code}: {text}")

    def create_runner(
        self,
        model_path: Path,
        threads: int,
        input_array: np.ndarray,
        output_arrays: Sequence[np.ndarray],
        input_name: str,
        output_names: Sequence[str],
    ) -> Runner:
        if not self._env:
            raise OrtError("NativeOrt 已关闭")
        return Runner(self, model_path, threads, input_array, output_arrays, input_name, output_names)

    def close(self):
        for runner in list(getattr(self, "_runners", ())):
            runner.close()
        if getattr(self, "_env", None):
            self._release_env(self._env)
            self._env = _P()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class Runner:
    """预分配输入/输出；可用 np.copyto 修改输入内容，禁止 resize 数组。"""

    def __init__(self, native, model_path, threads, input_array, output_arrays, input_name, output_names):
        self._native = native
        self._session = _P()
        self._options = _P()
        self._memory = _P()
        self._values = []
        self._shapes = []
        self.input_array = self._array(input_array, output=False)
        self.output_arrays = tuple(self._array(array, output=True) for array in output_arrays)
        if not self.output_arrays or len(self.output_arrays) != len(output_names):
            raise ValueError("输出数组和名称必须非空且一一对应")
        if not isinstance(threads, int) or not 1 <= threads <= 2**31 - 1:
            raise ValueError("threads 必须为正 int32")
        self.model_path = Path(model_path).resolve(strict=True)
        self._name_bytes = tuple(self._name(name) for name in (input_name, *output_names))
        self._input_names = (C.c_char_p * 1)(self._name_bytes[0])
        self._output_names = (C.c_char_p * len(output_names))(*self._name_bytes[1:])
        try:
            native._check(native._create_options(C.byref(self._options)))
            native._check(native._execution(self._options, 0))  # ORT_SEQUENTIAL
            native._check(native._optimization(self._options, 99))  # ORT_ENABLE_ALL / Level3
            native._check(native._intra(self._options, threads))
            native._check(native._inter(self._options, 1))
            native._check(native._config(self._options, b"session.intra_op.allow_spinning", b"0"))
            native._check(native._config(self._options, b"session.inter_op.allow_spinning", b"0"))
            native._check(
                native._create_session(native._env, str(self.model_path), self._options, C.byref(self._session))
            )
            native._check(native._create_memory(1, 0, C.byref(self._memory)))
            for array in (self.input_array, *self.output_arrays):
                shape = (C.c_int64 * array.ndim)(*array.shape)
                self._shapes.append(shape)
                value = _P()
                native._check(
                    native._create_tensor(
                        self._memory, array.ctypes.data, array.nbytes, shape, array.ndim, 1, C.byref(value)
                    )
                )
                self._values.append(value)
            self._input_values = (_P * 1)(self._values[0].value)
            self._expected_outputs = tuple(value.value for value in self._values[1:])
            self._output_values = (_P * len(self.output_arrays))(*self._expected_outputs)
            native._runners.add(self)
        except BaseException:
            self.close()
            raise

    @staticmethod
    def _array(array, *, output):
        if not isinstance(array, np.ndarray) or array.dtype != np.dtype(np.float32):
            raise ValueError("张量必须为已分配的 float32 NumPy 数组")
        if not array.flags.c_contiguous or not array.flags.aligned:
            raise ValueError("张量必须连续并按 float32 对齐")
        if output and not array.flags.writeable:
            raise ValueError("输出数组必须可写")
        if array.size == 0:
            raise ValueError("实验入口不支持空张量")
        return array

    @staticmethod
    def _name(name):
        encoded = name.encode("utf-8")
        if not encoded or b"\0" in encoded:
            raise ValueError("张量名称必须非空且不含 NUL")
        return encoded

    def __call__(self):
        if not self._session:
            raise OrtError("Runner 已关闭")
        native = self._native
        status = native._run(
            self._session, None, self._input_names, self._input_values, 1,
            self._output_names, len(self.output_arrays), self._output_values,
        )
        if tuple(self._output_values) != self._expected_outputs:
            # Run 必须复用非空输出；如 ABI/模型发生异常，先释放意外返回的值，
            # 恢复原指针，保留数组与原 OrtValue 的所有权，禁止后续静默使用新分配。
            extras = set(self._output_values) - set(self._expected_outputs) - {None}
            for value in extras:
                native._release_value(value)
            for index, value in enumerate(self._expected_outputs):
                self._output_values[index] = value
            native._check(status)
            raise OrtError("ORT 替换了预分配输出 OrtValue，已停止本轮实验")
        native._check(status)
        return self.output_arrays

    def close(self):
        native = self._native
        for value in self._values:
            native._release_value(value)
        self._values.clear()
        for attribute, release in (
            ("_memory", native._release_memory),
            ("_session", native._release_session),
            ("_options", native._release_options),
        ):
            handle = getattr(self, attribute)
            if handle:
                release(handle)
                setattr(self, attribute, _P())
        native._runners.discard(self)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
