#!/usr/bin/env python3
"""独立发布 OCR 固定依赖；默认 dry-run，只有 --upload 会写远端对象。

需要 --manifest、--cache-dir、--output、--receipt、--env-file。上传时需要
boto3，可由发布机的独立 Python 环境提供；客户端 EXE 不需要 Python 或密钥。
现有对象完整读取校验，单次 Put 禁用 multipart，公开匿名读回通过后才写候选。
不改生产清单、缓存、Bucket ACL 或系统设置，也不删除远端对象。
Tiny 默认发布完整六项；--only classification 只发布分类器，不宣称其余五项本轮通过。
Small 默认或 --only small-models 只发布固定 det/rec/dictionary 三项；共享三项保持原 URL。
Small 对象前缀固定独立版本，禁止将它们写入 Tiny 前缀，也不重发共享 runtime/cls/license。
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import logging
import re
import ssl
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import BinaryIO
from urllib.parse import quote, urlsplit
from urllib.request import HTTPRedirectHandler, HTTPSHandler, Request, build_opener


MAX_SINGLE_PUT = 256 * 1024 * 1024
CHUNK = 1024 * 1024
ENV_KEYS = (
    "BITIFUL_ACCESS_KEY", "BITIFUL_SECRET_KEY", "BITIFUL_BUCKET_NAME",
    "BITIFUL_ENDPOINT_URL", "BITIFUL_REGION",
)
MODEL_NAMES = {
    "detection": "det.onnx",
    "classification": "cls.onnx",
    "recognition": "rec.onnx",
    "dictionary": "ppocrv6_tiny_dict.txt",
    "license": "LICENSE-PaddleOCR.txt",
}
SMALL_MODEL_NAMES = {**MODEL_NAMES, "dictionary": "ppocrv6_dict.txt"}
SMALL_NEW_MODELS = ("detection", "recognition", "dictionary")
SMALL_REVISION = "cpu-api24-small-1"
SMALL_MODEL_DIRECTORY = "ppocrv6-small-pinned-1"
SMALL_PREFIX = "rustshot/dependencies/" + SMALL_REVISION
# 发布端固定身份；不改客户端清单。共享三项仅核清单，绝不在 Small 模式上传/改 URL。
SMALL_FIXED_IDENTITIES = {
    ("runtime", "archive"): (5294996, "a8622a7ebf7f560cade2707070a8005f12aa2b27d0b8a6055a21feef158bd2e8"),
    ("models", "detection"): (9880512, "d73e0058b7a8086bbd57f3d10b8bcd4ff95363f67e06e2762b5e814fe9c9410e"),
    ("models", "classification"): (1019454, "94a6a0a0425f2b5f08b5df72086f2d72abe40f1d22f6d12d2cd83674f11f2ff3"),
    ("models", "recognition"): (21159378, "5435fd747c9e0efe15a96d0b378d5bd157e9492ed8fd80edf08f30d02fa24634"),
    ("models", "dictionary"): (74947, "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d"),
    ("models", "license"): (11376, "3840c5c0c61c294264d2dd77b8777be6ddd90121ef4e0e64abcd22edea581d6e"),
}


class PublishFailure(Exception):
    """只保存预定义分类与公开对象 key，不保存 SDK/HTTP 异常文本。"""

    def __init__(self, kind: str, key: str = ""):
        super().__init__(kind)
        self.kind = kind
        self.key = key


@dataclass(frozen=True)
class Asset:
    path: Path
    key: str
    url: str
    size: int
    sha256: str
    field: tuple[str, ...]

    def record(self) -> dict:
        return {
            "path": str(self.path), "key": self.key, "url": self.url,
            "bytes": self.size, "sha256": self.sha256,
        }


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def read_env(path: Path) -> dict[str, str]:
    values = {}
    # 只读明确指定文件，不执行 .env，也不向子进程/系统环境写入密钥。
    for line in path.read_text(encoding="utf-8-sig").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[7:].lstrip()
        name, separator, value = line.partition("=")
        name = name.strip()
        if not separator or name not in ENV_KEYS:
            continue
        if name in values:
            raise PublishFailure("DuplicateConfigurationKey")
        value = value.strip()
        if value[:1] in ("'", '"'):
            if len(value) < 2 or value[-1] != value[0]:
                raise PublishFailure("InvalidConfigurationQuotes")
            value = value[1:-1]
        values[name] = value
    return values


def https_base(value: str) -> str:
    parsed = urlsplit(value)
    if (parsed.scheme != "https" or not parsed.netloc or parsed.username is not None
            or parsed.password is not None or parsed.query or parsed.fragment):
        raise PublishFailure("PublicBaseRequiresUnsignedHttps")
    return value.rstrip("/")


def configuration(values: dict[str, str], public_base: str | None, upload: bool) -> dict:
    required = ("BITIFUL_BUCKET_NAME", "BITIFUL_ENDPOINT_URL", "BITIFUL_REGION")
    if upload:
        required += ("BITIFUL_ACCESS_KEY", "BITIFUL_SECRET_KEY")
    if any(not values.get(name, "") for name in required):
        raise PublishFailure("MissingPublisherConfiguration")
    bucket = values["BITIFUL_BUCKET_NAME"]
    region = values["BITIFUL_REGION"]
    if not re.fullmatch(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]", bucket):
        raise PublishFailure("InvalidBucketName")
    if not re.fullmatch(r"[A-Za-z0-9-]{1,40}", region):
        raise PublishFailure("InvalidRegion")
    endpoint = https_base(values["BITIFUL_ENDPOINT_URL"])
    base = https_base(public_base or endpoint + "/" + bucket)
    return {"bucket": bucket, "region": region, "endpoint": endpoint, "public_base": base}


def plain_name(value: object) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_.-]+", value) or value in (".", ".."):
        raise PublishFailure("InvalidAssetName")
    return value


def field_at(document: dict, field: tuple[str, ...]) -> dict:
    value = document
    for part in field:
        value = value[part]
    return value


def build_assets(manifest: dict, cache: Path, base: str, prefix: str | None) -> list[Asset]:
    revision = plain_name(manifest["cache_revision"])
    if manifest.get("runtime_api") != 24:
        raise PublishFailure("UnexpectedRuntimeApi")
    small = manifest["models"].get("profile") == "PP-OCRv6-small"
    names = SMALL_MODEL_NAMES if small else MODEL_NAMES
    if revision == SMALL_REVISION and not small:
        raise PublishFailure("UnexpectedSmallManifestNamespace")
    if small:
        if revision != SMALL_REVISION or manifest["models"]["directory"] != SMALL_MODEL_DIRECTORY:
            raise PublishFailure("UnexpectedSmallManifestNamespace")
        if prefix is not None and prefix != SMALL_PREFIX:
            raise PublishFailure("SmallRequiresFixedObjectPrefix")
        for field, identity in SMALL_FIXED_IDENTITIES.items():
            item = field_at(manifest, field)
            if (type(item["bytes"]) is not int or item["bytes"] != identity[0]
                    or item["sha256"] != identity[1]):
                raise PublishFailure("UnexpectedSmallFixedIdentity")
    prefix = (prefix or "rustshot/dependencies/" + revision).strip("/")
    if (not re.fullmatch(r"[A-Za-z0-9_./-]+", prefix)
            or any(part in ("", ".", "..") for part in prefix.split("/"))):
        raise PublishFailure("InvalidObjectPrefix")
    archive = manifest["runtime"]["archive"]
    match = re.fullmatch(r"onnxruntime-win-x64-(\d+\.\d+\.\d+)-runtime\.zip", plain_name(archive["name"]))
    if not match:
        raise PublishFailure("UnexpectedRuntimeArchive")
    version = match.group(1)
    models = manifest["models"]
    model_dir = plain_name(models["directory"])
    entries = [("runtime", "archive")]
    entries += [("models", kind) for kind in MODEL_NAMES]
    expected_names = [archive["name"], *names.values()]
    assets = []
    for index, field in enumerate(entries):
        item = field_at(manifest, field)
        name = plain_name(item["name"])
        if name != expected_names[index]:
            raise PublishFailure("UnexpectedFixedAssetSet")
        size = item["bytes"]
        digest = item["sha256"]
        if (type(size) is not int or size <= 0 or size > MAX_SINGLE_PUT
                or not isinstance(digest, str) or not re.fullmatch(r"[0-9a-fA-F]{64}", digest)):
            raise PublishFailure("InvalidFixedIdentity")
        https_base(item.get("upstream_url", item["url"]).split("?", 1)[0])
        relative = f"runtime/v{version}/{name}" if index == 0 else f"models/{model_dir}/{name}"
        key = prefix + "/" + relative
        path = cache / name if index == 0 else cache / model_dir / name
        assets.append(Asset(path, key, base + "/" + quote(key, safe="/"), size, digest.lower(), field))
    if len({asset.key for asset in assets}) != len(assets):
        raise PublishFailure("DuplicateObjectKeys")
    return assets


def selected_assets(assets: list[Asset], only: str | None) -> list[Asset]:
    # build_assets 已核 Small 的固定名称、命名空间与身份；这里只缩小发布范围。
    small = any(asset.field == ("models", "dictionary")
                and asset.path.name == SMALL_MODEL_NAMES["dictionary"] for asset in assets)
    if small:
        if only is None or only == "small-models":
            selected = [asset for asset in assets if asset.field in
                        [("models", kind) for kind in SMALL_NEW_MODELS]]
            if len(selected) != len(SMALL_NEW_MODELS):
                raise PublishFailure("UnexpectedSmallAssetSelection")
            return selected
        if only not in SMALL_NEW_MODELS:
            raise PublishFailure("SmallSharedAssetPublicationNotAllowed")
    elif only == "small-models":
        raise PublishFailure("SmallSelectionRequiresSmallManifest")
    if only is None:
        return assets
    field = ("runtime", "archive") if only == "runtime" else ("models", only)
    selected = [asset for asset in assets if asset.field == field]
    if len(selected) != 1:
        raise PublishFailure("UnknownAssetSelection")
    return selected


def digest_stream(stream: BinaryIO, expected_size: int, key: str) -> str:
    digest = hashlib.sha256()
    size = 0
    while chunk := stream.read(CHUNK):
        size += len(chunk)
        if size > expected_size:
            raise PublishFailure("ReadbackExceedsExpectedBytes", key)
        digest.update(chunk)
    if size != expected_size:
        raise PublishFailure("ReadbackByteCountMismatch", key)
    return digest.hexdigest()


def verify_local(asset: Asset) -> None:
    if asset.path.is_symlink() or not asset.path.is_file() or asset.path.stat().st_size != asset.size:
        raise PublishFailure("LocalAssetTypeOrByteCountMismatch", asset.key)
    with asset.path.open("rb") as stream:
        actual = digest_stream(stream, asset.size, asset.key)
    if actual != asset.sha256:
        raise PublishFailure("LocalAssetSha256Mismatch", asset.key)


def sdk_status(error: Exception) -> int:
    response = getattr(error, "response", {})
    try:
        return int(response.get("ResponseMetadata", {}).get("HTTPStatusCode", 0))
    except (AttributeError, TypeError, ValueError):
        return 0


def sdk_client(values: dict[str, str], config: dict):
    # boto3 只在发布端显式 upload 时加载；默认 dry-run 使用 Python 标准库即可。
    import boto3
    from botocore.config import Config

    for name in ("boto3", "botocore", "urllib3", "s3transfer"):
        logging.getLogger(name).setLevel(logging.CRITICAL)
    return boto3.client(
        "s3", endpoint_url=config["endpoint"], region_name=config["region"],
        aws_access_key_id=values["BITIFUL_ACCESS_KEY"],
        aws_secret_access_key=values["BITIFUL_SECRET_KEY"],
        config=Config(
            signature_version="s3v4", s3={"addressing_style": "path"},
            request_checksum_calculation="when_required", response_checksum_validation="when_required",
            connect_timeout=20, read_timeout=180, retries={"max_attempts": 2, "mode": "standard"},
        ),
    )


def verify_existing(client, bucket: str, asset: Asset) -> None:
    response = client.get_object(Bucket=bucket, Key=asset.key)
    body = response["Body"]
    try:
        if response.get("ContentLength") != asset.size:
            raise PublishFailure("ExistingObjectByteCountConflict", asset.key)
        actual = digest_stream(body, asset.size, asset.key)
    finally:
        body.close()
    if actual != asset.sha256:
        raise PublishFailure("ExistingObjectSha256Conflict", asset.key)


def upload_or_verify(client, bucket: str, asset: Asset) -> str:
    try:
        client.head_object(Bucket=bucket, Key=asset.key)
    except Exception as error:
        if sdk_status(error) != 404:
            raise
    else:
        verify_existing(client, bucket, asset)
        return "existing_verified"
    # 同一版本 key 为不可变内容。条件 Put 避免 HEAD→Put 期间覆盖另一发布者。
    verify_local(asset)
    content_type = "application/zip" if asset.field == ("runtime", "archive") else "application/octet-stream"
    try:
        with asset.path.open("rb") as body:
            client.put_object(
                Bucket=bucket, Key=asset.key, Body=body, ContentLength=asset.size,
                ContentType=content_type, Metadata={"sha256": asset.sha256}, IfNoneMatch="*",
            )
    except Exception as error:
        if sdk_status(error) not in (409, 412):
            raise
        verify_existing(client, bucket, asset)
        return "concurrent_existing_verified"
    return "single_put_complete"


class HttpsOnlyRedirect(HTTPRedirectHandler):
    def redirect_request(self, request, file_pointer, code, message, headers, new_url):
        https_base(new_url)
        return super().redirect_request(request, file_pointer, code, message, headers, new_url)


def verify_public(asset: Asset) -> None:
    # 无 S3 签名、Authorization、Cookie；使用默认 TLS 证书校验。
    opener = build_opener(HttpsOnlyRedirect(), HTTPSHandler(context=ssl.create_default_context()))
    request = Request(asset.url, headers={"User-Agent": "RustShot-OCR-Dependency-Publisher/1"})
    with opener.open(request, timeout=180) as response:
        if response.status != 200:
            raise PublishFailure("AnonymousReadbackHttpStatus", asset.key)
        actual = digest_stream(response, asset.size, asset.key)
    if actual != asset.sha256:
        raise PublishFailure("AnonymousReadbackSha256Mismatch", asset.key)


def candidate_manifest(manifest: dict, assets: list[Asset]) -> dict:
    candidate = copy.deepcopy(manifest)
    for asset in assets:
        entry = field_at(candidate, asset.field)
        entry["upstream_url"] = entry.get("upstream_url", entry["url"])
        entry["url"] = asset.url
    return candidate


def write_json(stream, document: dict) -> None:
    stream.seek(0)
    json.dump(document, stream, ensure_ascii=False, indent=2)
    stream.write("\n")
    stream.truncate()
    stream.flush()


def run(arguments) -> int:
    receipt = {
        "schema": 1, "started_utc": utc_now(), "ok": False,
        "mode": "upload" if arguments.upload else "dry_run", "candidate_written": False,
        "upload_enabled": arguments.upload, "all_public_readbacks_verified": False,
        "bucket_acl_modified": False, "remote_objects_deleted": False,
        "manifest_or_cache_modified": False, "credentials_recorded": False,
        "signed_public_urls": False, "image_or_clipboard_uploaded": False,
        "completed": [], "stage": "prepare", "active_key": "",
    }
    # 本次独占新回执，随后只更新此已打开句柄；不覆盖旧运行结果。
    try:
        arguments.receipt.parent.mkdir(parents=True, exist_ok=True)
        receipt_file = arguments.receipt.open("x+", encoding="utf-8", newline="\n")
    except Exception as error:
        print(json.dumps({"ok": False, "kind": type(error).__name__, "key": "", "stage": "receipt_create"}))
        return 1
    with receipt_file:
        try:
            write_json(receipt_file, receipt)
            if arguments.output.exists() or arguments.output.is_symlink():
                raise PublishFailure("CandidateOutputAlreadyExists")
            manifest_bytes = arguments.manifest.read_bytes()
            manifest = json.loads(manifest_bytes.decode("utf-8-sig"))
            values = read_env(arguments.env_file)
            config = configuration(values, arguments.public_base, arguments.upload)
            all_assets = build_assets(manifest, arguments.cache_dir, config["public_base"], arguments.prefix)
            assets = selected_assets(all_assets, arguments.only)
            receipt.update({
                "configuration": config,
                "credential_presence": {name: bool(values.get(name)) for name in ENV_KEYS[:2]},
                "input_manifest": {"path": str(arguments.manifest), "bytes": len(manifest_bytes),
                                   "sha256": hashlib.sha256(manifest_bytes).hexdigest()},
                "expected_files": [asset.record() for asset in assets],
                "selection": arguments.only or ("small-models" if len(assets) == 3 else "all"),
                "manifest_asset_count": len(all_assets), "selected_asset_count": len(assets),
                "cache_revision": manifest["cache_revision"],
                "single_put_maximum_bytes": MAX_SINGLE_PUT, "multipart_used": False,
            })
            receipt["stage"] = "local_verification"
            for asset in assets:
                receipt["active_key"] = asset.key
                verify_local(asset)
            receipt["selected_local_files_verified"] = True
            receipt["all_manifest_local_files_verified"] = len(assets) == len(all_assets)
            write_json(receipt_file, receipt)
            if arguments.upload:
                receipt["stage"] = "sdk_initialize"
                client = sdk_client(values, config)
                for asset in assets:
                    receipt["active_key"] = asset.key
                    receipt["stage"] = "remote_upload_or_verification"
                    action = upload_or_verify(client, config["bucket"], asset)
                    item = {**asset.record(), "remote_action": action, "anonymous_verified": False}
                    receipt["completed"].append(item)
                    write_json(receipt_file, receipt)
                    receipt["stage"] = "anonymous_readback"
                    verify_public(asset)
                    item["anonymous_verified"] = True
                    write_json(receipt_file, receipt)
                receipt["selected_public_readbacks_verified"] = True
                receipt["all_public_readbacks_verified"] = len(assets) == len(all_assets)
            receipt["stage"] = "candidate_write"
            candidate = candidate_manifest(manifest, assets)
            # 只有全部选中项成功或明确 dry-run 才创建候选；未选中项保持原清单。
            arguments.output.parent.mkdir(parents=True, exist_ok=True)
            with arguments.output.open("x+", encoding="utf-8", newline="\n") as output:
                write_json(output, candidate)
            final_bytes = arguments.output.read_bytes()
            receipt["candidate"] = {"path": str(arguments.output), "bytes": len(final_bytes),
                                    "sha256": hashlib.sha256(final_bytes).hexdigest(),
                                    "qualification": "public_verified" if arguments.upload else "dry_run_only"}
            receipt.update(ok=True, candidate_written=True, stage="complete", active_key="")
            exit_code = 0
        except BaseException as error:
            # 绝不 str(error)/traceback：SDK/代理错误可能含签名、凭据或完整请求。
            receipt["error"] = {
                "kind": error.kind if isinstance(error, PublishFailure) else type(error).__name__,
                "key": error.key if isinstance(error, PublishFailure) and error.key else receipt["active_key"],
                "stage": receipt["stage"],
            }
            exit_code = 1
        receipt["finished_utc"] = utc_now()
        receipt["exit_code"] = exit_code
        try:
            write_json(receipt_file, receipt)
        except Exception as error:
            print(json.dumps({"ok": False, "kind": type(error).__name__, "key": "", "stage": "receipt_write"}))
            return 1
    print(json.dumps({"ok": receipt["ok"], "mode": receipt["mode"],
                      "receipt": str(arguments.receipt), "candidate_written": receipt["candidate_written"],
                      "error": receipt.get("error")}, ensure_ascii=False))
    return exit_code


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ("manifest", "cache-dir", "output", "receipt", "env-file"):
        parser.add_argument("--" + option, type=Path, required=True)
    parser.add_argument("--public-base", help="公开匿名 HTTPS 前缀；默认 endpoint/bucket")
    parser.add_argument("--prefix", help="Tiny 默认版本前缀；Small 仅接受固定 cpu-api24-small-1 前缀")
    parser.add_argument("--only", choices=("runtime", *MODEL_NAMES, "small-models"),
                        help="单件或 Small 新三件；Small 默认新三件，共享项保持原清单且不计本轮验证")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--upload", action="store_true", help="显式允许单段 Put 发布与匿名完整读回")
    mode.add_argument("--dry-run", action="store_true", help="只校验本地依赖并生成候选，无 SDK/网络调用（默认）")
    arguments = parser.parse_args()
    for field in ("manifest", "cache_dir", "output", "receipt", "env_file"):
        setattr(arguments, field, getattr(arguments, field).absolute())
    return run(arguments)


if __name__ == "__main__":
    sys.exit(main())
