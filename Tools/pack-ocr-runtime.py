#!/usr/bin/env python3
"""从官方 ONNX Runtime 压缩包裁出客户端需要的成员，生成可复现的小运行库包。

用法：pack-ocr-runtime.py --manifest dependencies/ppocr-cpu.json --upstream <官方zip> --output <小zip>
只收清单 runtime.files 列出的成员（不含 PDB、头文件、导入库），逐件核对 bytes／SHA；
成员路径与时间戳沿用官方包，固定 deflate 9 级压缩，同一输入得到同一 SHA。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
import zipfile
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ("manifest", "upstream", "output"):
        parser.add_argument("--" + option, type=Path, required=True)
    arguments = parser.parse_args()
    manifest = json.loads(arguments.manifest.read_text(encoding="utf-8-sig"))
    archive = manifest["runtime"]["archive"]
    upstream = arguments.upstream.read_bytes()
    if (len(upstream) != archive["upstream_bytes"]
            or hashlib.sha256(upstream).hexdigest() != archive["upstream_sha256"].lower()):
        print("官方压缩包与清单 upstream_bytes／upstream_sha256 不符", file=sys.stderr)
        return 1
    if arguments.output.exists():
        print("输出已存在，不覆盖", file=sys.stderr)
        return 1
    with zipfile.ZipFile(arguments.upstream) as source, \
            zipfile.ZipFile(arguments.output, "x", zipfile.ZIP_DEFLATED, compresslevel=9) as target:
        for file in manifest["runtime"]["files"]:
            info = source.getinfo(file["member"])
            data = source.read(info)
            if len(data) != file["bytes"] or hashlib.sha256(data).hexdigest() != file["sha256"].lower():
                print(f"成员与清单不符：{file['member']}", file=sys.stderr)
                return 1
            entry = zipfile.ZipInfo(info.filename, info.date_time)
            entry.external_attr = info.external_attr
            entry.compress_type = zipfile.ZIP_DEFLATED
            target.writestr(entry, data, compresslevel=9)
    output = arguments.output.read_bytes()
    print(json.dumps({"name": arguments.output.name, "bytes": len(output),
                      "sha256": hashlib.sha256(output).hexdigest()}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
