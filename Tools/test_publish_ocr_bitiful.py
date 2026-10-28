"""发布范围与候选清单的后台检查；不访问网络，也不读取发布密钥。"""
import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path
from types import SimpleNamespace


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("ocr_publisher", ROOT / "Tools/publish-ocr-bitiful.py")
PUBLISHER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = PUBLISHER
SPEC.loader.exec_module(PUBLISHER)


class PublicationScopeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.manifest = json.loads((ROOT / "dependencies/ppocr-cpu.json").read_text(encoding="utf-8"))
        for entry in [self.manifest["runtime"]["archive"],
                      *(self.manifest["models"][kind] for kind in PUBLISHER.MODEL_NAMES)]:
            entry["bytes"] = 3
            entry["sha256"] = hashlib.sha256(b"abc").hexdigest()
        self.manifest_path = self.root / "manifest.json"
        self.manifest_path.write_text(json.dumps(self.manifest), encoding="utf-8")
        self.env_path = self.root / "publisher.env"
        self.env_path.write_text("BITIFUL_BUCKET_NAME=example-bucket\n"
                                 "BITIFUL_ENDPOINT_URL=https://example.invalid\n"
                                 "BITIFUL_REGION=cn-east-1\n", encoding="utf-8")
        self.assets = PUBLISHER.build_assets(self.manifest, self.root / "cache",
                                             "https://example.invalid/example-bucket", None)

    def run_publisher(self, only, label):
        arguments = SimpleNamespace(manifest=self.manifest_path, cache_dir=self.root / "cache",
                                    output=self.root / f"{label}-candidate.json",
                                    receipt=self.root / f"{label}-receipt.json", env_file=self.env_path,
                                    prefix=None, public_base=None, upload=False, only=only)
        before = self.manifest_path.read_bytes()
        with contextlib.redirect_stdout(io.StringIO()):
            code = PUBLISHER.run(arguments)
        self.assertEqual(self.manifest_path.read_bytes(), before)
        receipt = json.loads(arguments.receipt.read_text(encoding="utf-8"))
        return code, arguments, receipt

    def seed(self, assets, data=b"abc"):
        for asset in assets:
            asset.path.parent.mkdir(parents=True, exist_ok=True)
            asset.path.write_bytes(data)

    def test_classifier_only_works_without_other_files_and_preserves_their_sources(self):
        selected = PUBLISHER.selected_assets(self.assets, "classification")
        self.seed(selected)
        code, arguments, receipt = self.run_publisher("classification", "classifier")
        self.assertEqual(code, 0)
        self.assertEqual(receipt["manifest_asset_count"], 6)
        self.assertEqual(receipt["selected_asset_count"], 1)
        self.assertTrue(receipt["selected_local_files_verified"])
        self.assertFalse(receipt["all_manifest_local_files_verified"])
        self.assertFalse(receipt["all_public_readbacks_verified"])
        self.assertEqual(receipt["expected_files"][0]["key"],
                         "rustshot/dependencies/cpu-api24-tiny-1/models/ppocrv6-tiny-pinned-1/cls.onnx")
        candidate = json.loads(arguments.output.read_text(encoding="utf-8"))
        for kind in ("detection", "recognition", "dictionary", "license"):
            self.assertEqual(candidate["models"][kind], self.manifest["models"][kind])
        self.assertEqual(candidate["runtime"], self.manifest["runtime"])
        self.assertEqual(candidate["models"]["classification"]["url"], selected[0].url)
        self.assertEqual(candidate["models"]["classification"]["upstream_url"],
                         self.manifest["models"]["classification"]["upstream_url"])

    def test_full_publication_requires_all_six_files(self):
        self.seed(PUBLISHER.selected_assets(self.assets, "classification"))
        code, arguments, receipt = self.run_publisher(None, "missing")
        self.assertEqual(code, 1)
        self.assertFalse(arguments.output.exists())
        self.seed(self.assets)
        code, arguments, receipt = self.run_publisher(None, "full")
        self.assertEqual(code, 0)
        self.assertEqual(receipt["selected_asset_count"], 6)
        self.assertTrue(receipt["all_manifest_local_files_verified"])
        self.assertFalse(receipt["all_public_readbacks_verified"])

    def test_corrupted_classifier_never_creates_candidate(self):
        self.seed(PUBLISHER.selected_assets(self.assets, "classification"), b"abd")
        code, arguments, receipt = self.run_publisher("classification", "corrupt")
        self.assertEqual(code, 1)
        self.assertFalse(arguments.output.exists())
        self.assertEqual(receipt["error"]["kind"], "LocalAssetSha256Mismatch")


class OfflineHttpError(Exception):
    def __init__(self, status):
        self.response = {"ResponseMetadata": {"HTTPStatusCode": status}}


class OfflineObjectClient:
    def __init__(self, objects=None, head_error=404, put_error=None, winner=None):
        self.objects = dict(objects or {})
        self.head_error = head_error
        self.put_error = put_error
        self.winner = winner
        self.puts = []
        self.bodies = []

    def head_object(self, **arguments):
        if arguments["Key"] not in self.objects:
            raise OfflineHttpError(self.head_error)
        return {}

    def get_object(self, **arguments):
        if arguments["Key"] not in self.objects:
            raise OfflineHttpError(404)
        data = self.objects[arguments["Key"]]
        body = io.BytesIO(data)
        self.bodies.append(body)
        # 不给 SHA Metadata/ETag，强制真正完整读流。
        return {"ContentLength": len(data), "Body": body}

    def put_object(self, **arguments):
        self.puts.append({**arguments, "Body": arguments["Body"].read()})
        if self.put_error:
            if self.winner is not None:
                self.objects[arguments["Key"]] = self.winner
            raise OfflineHttpError(self.put_error)
        self.objects[arguments["Key"]] = self.puts[-1]["Body"]


class SmallManifestGateTests(unittest.TestCase):
    def setUp(self):
        self.small = json.loads((ROOT / "dependencies/ppocr-small-cpu.json").read_text(encoding="utf-8"))
        self.tiny = json.loads((ROOT / "dependencies/ppocr-cpu.json").read_text(encoding="utf-8"))

    def build(self, manifest, prefix=None):
        return PUBLISHER.build_assets(manifest, Path("offline-cache"), "https://example.invalid/bucket", prefix)

    def test_fixed_small_names_keys_and_shared_manifest_identities(self):
        assets = self.build(self.small)
        selected = PUBLISHER.selected_assets(assets, "small-models")
        self.assertEqual([asset.field for asset in selected],
                         [("models", kind) for kind in ("detection", "recognition", "dictionary")])
        self.assertEqual([asset.key for asset in selected], [
            "rustshot/dependencies/cpu-api24-small-1/models/ppocrv6-small-pinned-1/det.onnx",
            "rustshot/dependencies/cpu-api24-small-1/models/ppocrv6-small-pinned-1/rec.onnx",
            "rustshot/dependencies/cpu-api24-small-1/models/ppocrv6-small-pinned-1/ppocrv6_dict.txt",
        ])
        self.assertEqual(PUBLISHER.selected_assets(assets, None), selected)
        for kind in ("classification", "license"):
            self.assertEqual(self.small["models"][kind], self.tiny["models"][kind])
        self.assertEqual(self.small["runtime"], self.tiny["runtime"])
        self.assertEqual(PUBLISHER.MODEL_NAMES["dictionary"], "ppocrv6_tiny_dict.txt")
        self.assertEqual(len(PUBLISHER.selected_assets(self.build(self.tiny), None)), 6)

    def test_drifted_small_identity_namespace_or_prefix_is_rejected(self):
        changes = [
            (("cache_revision",), "cpu-api24-small-2"),
            (("models", "profile"), "PP-OCRv6-tiny"),
            (("models", "directory"), "ppocrv6-tiny-pinned-1"),
            (("models", "dictionary", "name"), "ppocrv6_tiny_dict.txt"),
            *((field + ("bytes",), identity[0] + 1)
              for field, identity in PUBLISHER.SMALL_FIXED_IDENTITIES.items()),
            (("models", "recognition", "sha256"), "0" * 64),
        ]
        for field, value in changes:
            with self.subTest(field=field):
                changed = copy.deepcopy(self.small)
                parent = changed
                for part in field[:-1]:
                    parent = parent[part]
                parent[field[-1]] = value
                with self.assertRaises(PUBLISHER.PublishFailure):
                    self.build(changed)
        for prefix in ("rustshot/dependencies/cpu-api24-tiny-1", "other/small", "../small"):
            with self.subTest(prefix=prefix), self.assertRaises(PUBLISHER.PublishFailure):
                self.build(self.small, prefix)
        self.assertEqual(self.build(self.small, PUBLISHER.SMALL_PREFIX), self.build(self.small))

    def test_small_scope_cannot_publish_shared_assets_or_accept_tiny(self):
        small_assets = self.build(self.small)
        for selection in ("runtime", "classification", "license", "unknown"):
            with self.subTest(selection=selection), self.assertRaises(PUBLISHER.PublishFailure):
                PUBLISHER.selected_assets(small_assets, selection)
        with self.assertRaises(PUBLISHER.PublishFailure):
            PUBLISHER.selected_assets(self.build(self.tiny), "small-models")
        self.assertEqual(len(PUBLISHER.selected_assets(self.build(self.tiny), "classification")), 1)


class SmallPublicationScopeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.manifest = json.loads((ROOT / "dependencies/ppocr-small-cpu.json").read_text(encoding="utf-8"))
        # 合成三字节固定样本只用于离线生命周期；上面独立测试核真正固定六件身份。
        fixture_identities = dict(PUBLISHER.SMALL_FIXED_IDENTITIES)
        for kind in PUBLISHER.SMALL_NEW_MODELS:
            self.manifest["models"][kind]["bytes"] = 3
            self.manifest["models"][kind]["sha256"] = hashlib.sha256(b"abc").hexdigest()
            fixture_identities[("models", kind)] = (3, hashlib.sha256(b"abc").hexdigest())
        patcher = mock.patch.object(PUBLISHER, "SMALL_FIXED_IDENTITIES", fixture_identities)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.manifest_path = self.root / "manifest.json"
        self.manifest_path.write_text(json.dumps(self.manifest), encoding="utf-8")
        self.assets = PUBLISHER.build_assets(self.manifest, self.root / "cache", "https://example.invalid/example-bucket", None)
        self.selected = PUBLISHER.selected_assets(self.assets, "small-models")
        self.values = {"BITIFUL_BUCKET_NAME": "example-bucket", "BITIFUL_ENDPOINT_URL": "https://example.invalid",
                       "BITIFUL_REGION": "cn-east-1", "BITIFUL_ACCESS_KEY": "offline-placeholder",
                       "BITIFUL_SECRET_KEY": "offline-placeholder"}

    def seed(self):
        for asset in self.selected:
            asset.path.parent.mkdir(parents=True, exist_ok=True)
            asset.path.write_bytes(b"abc")

    def run_publisher(self, label, upload=False, only="small-models", client=None, public_data=b"abc"):
        arguments = SimpleNamespace(manifest=self.manifest_path, cache_dir=self.root / "cache",
                                    output=self.root / f"{label}-candidate.json",
                                    receipt=self.root / f"{label}-receipt.json", env_file=self.root / "never-read.env",
                                    prefix=None, public_base=None, upload=upload, only=only)
        before = self.manifest_path.read_bytes()
        cache_before = {asset.path: asset.path.read_bytes() for asset in self.selected if asset.path.exists()}
        requests = []
        def open_public(request, timeout):
            requests.append(request)
            response = io.BytesIO(public_data)
            response.status = 200
            return response
        with mock.patch.object(PUBLISHER, "read_env", return_value=self.values), \
                mock.patch.object(PUBLISHER, "sdk_client", return_value=client) as sdk, \
                mock.patch.object(PUBLISHER, "build_opener", return_value=SimpleNamespace(open=open_public)), \
                contextlib.redirect_stdout(io.StringIO()):
            code = PUBLISHER.run(arguments)
        self.assertEqual(self.manifest_path.read_bytes(), before)
        self.assertEqual({path: path.read_bytes() for path in cache_before}, cache_before)
        receipt = json.loads(arguments.receipt.read_text(encoding="utf-8"))
        self.assertNotIn("offline-placeholder", json.dumps(receipt))
        return code, arguments, receipt, sdk.call_count, requests

    def test_default_small_dry_run_needs_only_three_and_preserves_shared_sources(self):
        self.seed()
        code, arguments, receipt, calls, requests = self.run_publisher("dry", only=None)
        self.assertEqual(code, 0)
        self.assertEqual(calls, 0)
        self.assertEqual(requests, [])
        self.assertEqual(receipt["selection"], "small-models")
        self.assertEqual(receipt["manifest_asset_count"], 6)
        self.assertEqual(receipt["selected_asset_count"], 3)
        self.assertTrue(receipt["selected_local_files_verified"])
        self.assertFalse(receipt["all_manifest_local_files_verified"])
        self.assertFalse(receipt["all_public_readbacks_verified"])
        candidate = json.loads(arguments.output.read_text(encoding="utf-8"))
        expected = copy.deepcopy(self.manifest)
        for asset in self.selected:
            expected["models"][asset.field[1]]["url"] = asset.url
        self.assertEqual(candidate, expected)
        self.assertTrue(all(not asset.path.exists() for asset in self.assets if asset not in self.selected))

    def test_each_missing_short_or_equal_size_corrupt_asset_fails_before_sdk(self):
        for index, asset in enumerate(self.selected):
            for case, data in (("missing", None), ("short", b"ab"), ("corrupt", b"abd")):
                with self.subTest(asset=asset.field, case=case):
                    self.seed()
                    if data is None:
                        asset.path.unlink()
                    else:
                        asset.path.write_bytes(data)
                    code, arguments, receipt, calls, requests = self.run_publisher(f"{index}-{case}", upload=True)
                    self.assertEqual(code, 1)
                    self.assertFalse(arguments.output.exists())
                    self.assertEqual(calls, 0)
                    self.assertEqual(requests, [])
                    self.assertFalse(receipt["candidate_written"])

    def test_small_upload_completes_three_anonymous_full_readbacks_only(self):
        self.seed()
        client = OfflineObjectClient()
        code, arguments, receipt, calls, requests = self.run_publisher("upload", upload=True, client=client)
        self.assertEqual(code, 0)
        self.assertEqual(calls, 1)
        self.assertEqual(len(client.puts), 3)
        self.assertTrue(all(item["IfNoneMatch"] == "*" and item["Body"] == b"abc"
                            and item["ContentLength"] == 3 for item in client.puts))
        self.assertEqual([request.full_url for request in requests], [asset.url for asset in self.selected])
        self.assertTrue(all(not request.has_header("Authorization") and not request.has_header("Cookie")
                            for request in requests))
        self.assertTrue(receipt["selected_public_readbacks_verified"])
        self.assertFalse(receipt["all_public_readbacks_verified"])
        self.assertTrue(all(item["anonymous_verified"] for item in receipt["completed"]))
        self.assertEqual(receipt["candidate"]["qualification"], "public_verified")

    def test_anonymous_readback_failure_never_writes_candidate(self):
        self.seed()
        for label, data in (("short", b"ab"), ("long", b"abcd"), ("badsha", b"abd")):
            with self.subTest(label=label):
                client = OfflineObjectClient()
                code, arguments, receipt, _, requests = self.run_publisher(label, upload=True, client=client, public_data=data)
                self.assertEqual(code, 1)
                self.assertFalse(arguments.output.exists())
                self.assertFalse(receipt["candidate_written"])
                self.assertFalse(receipt["completed"][0]["anonymous_verified"])
                self.assertEqual(len(requests), 1)
                self.assertEqual(len(client.puts), 1)


class ImmutableSmallPublicationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        path = Path(self.directory.name) / "det.onnx"
        path.write_bytes(b"abc")
        self.asset = PUBLISHER.Asset(path,
            "rustshot/dependencies/cpu-api24-small-1/models/ppocrv6-small-pinned-1/det.onnx",
            "https://example.invalid/bucket/small/det.onnx", 3, hashlib.sha256(b"abc").hexdigest(),
            ("models", "detection"))

    def test_existing_wrong_bytes_or_sha_never_puts_and_closes_body(self):
        for data in (b"ab", b"abd"):
            with self.subTest(data=data):
                client = OfflineObjectClient({self.asset.key: data})
                with self.assertRaises(PUBLISHER.PublishFailure):
                    PUBLISHER.upload_or_verify(client, "bucket", self.asset)
                self.assertEqual(client.puts, [])
                self.assertTrue(all(body.closed for body in client.bodies))
        client = OfflineObjectClient({self.asset.key: b"abc"})
        self.assertEqual(PUBLISHER.upload_or_verify(client, "bucket", self.asset), "existing_verified")
        self.assertEqual(client.puts, [])
        self.assertTrue(client.bodies[0].closed)

    def test_conditional_put_race_only_accepts_exact_winner_and_never_retries_overwrite(self):
        for status in (409, 412):
            for winner in (b"abc", b"abd", None):
                with self.subTest(status=status, winner=winner):
                    client = OfflineObjectClient(put_error=status, winner=winner)
                    if winner == b"abc":
                        self.assertEqual(PUBLISHER.upload_or_verify(client, "bucket", self.asset),
                                         "concurrent_existing_verified")
                    else:
                        with self.assertRaises((PUBLISHER.PublishFailure, OfflineHttpError)):
                            PUBLISHER.upload_or_verify(client, "bucket", self.asset)
                    self.assertEqual(len(client.puts), 1)
                    self.assertEqual(client.puts[0]["IfNoneMatch"], "*")
                    self.assertTrue(all(body.closed for body in client.bodies))
        client = OfflineObjectClient(head_error=403)
        with self.assertRaises(OfflineHttpError):
            PUBLISHER.upload_or_verify(client, "bucket", self.asset)
        self.assertEqual(client.puts, [])

    def test_anonymous_non_200_and_unsigned_https_redirect_contract(self):
        response = io.BytesIO(b"abc")
        response.status = 403
        opener = SimpleNamespace(open=lambda request, timeout: response)
        with mock.patch.object(PUBLISHER, "build_opener", return_value=opener), \
                self.assertRaises(PUBLISHER.PublishFailure):
            PUBLISHER.verify_public(self.asset)
        self.assertTrue(response.closed)
        handler = PUBLISHER.HttpsOnlyRedirect()
        request = PUBLISHER.Request(self.asset.url)
        for url in ("http://example.invalid/object", "https://user:password@example.invalid/object",
                    "https://example.invalid/object?signature=secret"):
            with self.subTest(url=url), self.assertRaises(PUBLISHER.PublishFailure):
                handler.redirect_request(request, None, 302, "", {}, url)


if __name__ == "__main__":
    unittest.main()
