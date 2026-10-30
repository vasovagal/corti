"""Synthetic-only tests for the native-lane preparation helper."""

import contextlib
import hashlib
import http.client
import importlib.util
import io
import json
from pathlib import Path
import ssl
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import urllib.error
import wave


SPEC = importlib.util.spec_from_file_location("quality_native", Path(__file__).with_name("native.py"))
native = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(native)


def fixture_source():
    """Literal/constant syntax matching the app, with tiny invented artifacts."""
    source = '''
pub const PARAKEET_DIR: &str = "parakeet";
pub const GGML_MODEL_ID: &str = "parakeet-ggml";
pub const GGML_SHA256: &str = "DIGEST";
pub const GGML_DOWNLOAD_BYTES: u64 = 1_234;
parakeet_encoder: parakeet.join("encoder.onnx"),
parakeet_decoder: parakeet.join("decoder.onnx"),
parakeet_joiner: parakeet.join("joiner.onnx"),
parakeet_tokens: parakeet.join("tokens.txt"),
pub fn model_catalog() -> Vec<ModelSpec> {
    vec![
'''.replace("DIGEST", "a" * 64)
    for model_id, kind, install, present in (
        ('"parakeet"', "Tarball", "PARAKEET_DIR", '"parakeet/encoder.onnx"'),
        ("GGML_MODEL_ID", "File", '"model.gguf"', '"model.gguf"'),
        ('"vad"', "File", '"vad.onnx"', '"vad.onnx"'),
    ):
        source += f'''        ModelSpec {{
            id: {model_id},
            url: "https://example.invalid/model",
            sha256: GGML_SHA256,
            download_bytes: GGML_DOWNLOAD_BYTES,
            kind: ArtifactKind::{kind},
            diarize_only: false,
            install_rel: {install},
            present_rel: {present},
        }},
'''
    return source + "    ]\n}\n"


def file_spec(data, model_id="vad", name="vad.onnx"):
    return {
        "id": model_id, "url": "https://example.invalid/model", "kind": "file",
        "sha256": hashlib.sha256(data).hexdigest(), "download_bytes": len(data),
        "install_rel": name, "present_rel": name, "required_paths": [name],
    }


class CatalogTests(unittest.TestCase):
    def test_pins_are_read_from_literals_and_constants(self):
        catalog = native.catalog_from_source(fixture_source())
        self.assertEqual([row["id"] for row in catalog["artifacts"]], list(native.MODEL_IDS))
        self.assertEqual(catalog["artifacts"][1]["download_bytes"], 1234)
        self.assertEqual(catalog["artifacts"][0]["required_paths"], [
            "parakeet/encoder.onnx", "parakeet/decoder.onnx",
            "parakeet/joiner.onnx", "parakeet/tokens.txt",
        ])
        self.assertEqual(catalog, native.catalog_from_source("\n" + fixture_source()))

    def test_pin_change_changes_cache_key(self):
        before = native.catalog_from_source(fixture_source())
        after = native.catalog_from_source(fixture_source().replace("a" * 64, "b" * 64))
        self.assertNotEqual(before["cache_key"], after["cache_key"])

    def test_missing_or_unsupported_pin_fails_closed(self):
        with self.assertRaises(native.PreparationError):
            native.catalog_from_source(fixture_source().replace('id: "vad"', 'id: "other"'))
        with self.assertRaises(native.PreparationError):
            native.catalog_from_source(fixture_source().replace(
                'url: "https://example.invalid/model"', 'url: concat!("https://", "host")'))

    def test_github_output_is_deterministic(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "models.rs"
            source.write_text(fixture_source())
            with contextlib.redirect_stdout(io.StringIO()):
                result = native.main(["catalog", "--source", str(source), "--output",
                                      str(root / "catalog.json"), "--github-output", str(root / "output")])
            self.assertEqual(result, 0)
            catalog = json.loads((root / "catalog.json").read_text())
            self.assertEqual((root / "output").read_text(), f"model_cache_key={catalog['cache_key']}\n")


class ModelTests(unittest.TestCase):
    def test_restored_cache_is_verified_even_with_existing_install(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "cache"
            models = root / "models"
            cache.mkdir()
            models.mkdir()
            specs = [file_spec(b"original", model_id, model_id) for model_id in native.MODEL_IDS]
            for row in specs:
                (cache / (row["sha256"] + ".artifact")).write_bytes(b"modified")
                (models / row["install_rel"]).write_bytes(b"previous install")
            with patch.object(native.urllib.request, "urlopen") as network:
                with self.assertRaisesRegex(native.PreparationError, "SHA-256 mismatch"):
                    native.prepare_models({"artifacts": specs}, cache, models)
                network.assert_not_called()
            self.assertEqual((models / "parakeet").read_bytes(), b"previous install")

    def test_verified_cache_replaces_untrusted_install_without_network(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "cache"
            cache.mkdir()
            specs = [file_spec(model_id.encode(), model_id, model_id) for model_id in native.MODEL_IDS]
            for row in specs:
                (cache / (row["sha256"] + ".artifact")).write_bytes(row["id"].encode())
            with patch.object(native.urllib.request, "urlopen") as network:
                with contextlib.redirect_stdout(io.StringIO()):
                    native.prepare_models({"artifacts": specs}, cache, root / "models")
                network.assert_not_called()
            for row in specs:
                self.assertEqual((root / "models" / row["id"]).read_bytes(), row["id"].encode())

    def test_download_checks_hash_and_uses_generic_user_agent(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "cached"
            spec = file_spec(b"expected")
            with patch.object(native.urllib.request, "urlopen", return_value=io.BytesIO(b"modified")) as network:
                with self.assertRaisesRegex(native.PreparationError, "SHA-256 mismatch"):
                    native.download_artifact(spec, destination)
            self.assertFalse(destination.exists())
            request = network.call_args.args[0]
            self.assertEqual(request.get_header("User-agent"), "Mozilla/5.0")
            self.assertEqual(list(Path(temporary).iterdir()), [])

    def test_transport_failure_retries_are_bounded(self):
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(native.urllib.request, "urlopen", side_effect=urllib.error.URLError("offline")) as network:
                with patch.object(native.time, "sleep"):
                    with self.assertRaisesRegex(native.PreparationError, "after 3 attempts"):
                        native.download_artifact(file_spec(b"x"), Path(temporary) / "cached")
            self.assertEqual(network.call_count, 3)
            self.assertEqual(list(Path(temporary).iterdir()), [])

    def test_download_failure_reports_only_status_or_exception_category(self):
        sentinel = "synthetic-private-error-detail"
        cases = [
            (urllib.error.HTTPError("https://example.invalid/" + sentinel, 503,
                                    sentinel, None, None), "http_status_503"),
            (urllib.error.URLError(ssl.SSLCertVerificationError(sentinel)),
             "tls_certificate_verification"),
            (urllib.error.URLError(TimeoutError(sentinel)), "timeout"),
            (urllib.error.URLError(ValueError(sentinel)), "ValueError"),
            (urllib.error.URLError(sentinel), "url_error"),
        ]
        for failure, category in cases:
            with self.subTest(category=category), tempfile.TemporaryDirectory() as temporary:
                with patch.object(native.urllib.request, "urlopen", side_effect=failure) as network:
                    with patch.object(native.time, "sleep"):
                        with self.assertRaises(native.PreparationError) as caught:
                            native.download_artifact(file_spec(b"x"), Path(temporary) / "cached")
                message = str(caught.exception)
                self.assertIn(f"cause={category}", message)
                self.assertNotIn(sentinel, message)
                self.assertNotIn("https://", message)
                self.assertEqual(network.call_count, native.MAX_ATTEMPTS)

    def test_incomplete_http_body_retries_then_installs_verified_bytes(self):
        class IncompleteBody(io.BytesIO):
            def read(self, size=-1):
                raise http.client.IncompleteRead(b"partial")

        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "cached"
            with patch.object(native.urllib.request, "urlopen", side_effect=[
                IncompleteBody(), io.BytesIO(b"expected"),
            ]) as network, patch.object(native.time, "sleep"):
                native.download_artifact(file_spec(b"expected"), destination)
            self.assertEqual(network.call_count, 2)
            self.assertEqual(destination.read_bytes(), b"expected")

    def test_archive_installs_only_known_files_and_rejects_traversal(self):
        for unsafe in (False, True):
            with self.subTest(unsafe=unsafe), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive = root / "archive.tar.bz2"
                with tarfile.open(archive, "w:bz2") as output:
                    for name in ("model/known", "model/unneeded", "../escaped" if unsafe else "model/readme"):
                        member = tarfile.TarInfo(name)
                        member.size = 4
                        output.addfile(member, io.BytesIO(b"data"))
                spec = file_spec(archive.read_bytes(), "parakeet", "model")
                spec.update(kind="tarball", present_rel="model/known", required_paths=["model/known"])
                if unsafe:
                    with self.assertRaises(native.PreparationError):
                        native.install_artifact(archive, spec, root / "models")
                    self.assertFalse((root / "models/model/known").exists())
                else:
                    native.install_artifact(archive, spec, root / "models")
                    self.assertEqual((root / "models/model/known").read_bytes(), b"data")
                    self.assertFalse((root / "models/model/unneeded").exists())
                self.assertFalse((root / "escaped").exists())

    def test_archive_missing_required_file_fails_without_install(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "empty.tar.bz2"
            with tarfile.open(archive, "w:bz2"):
                pass
            spec = file_spec(archive.read_bytes(), "parakeet", "model")
            spec.update(kind="tarball", present_rel="model/known", required_paths=["model/known"])
            with self.assertRaisesRegex(native.PreparationError, "missing required files"):
                native.install_artifact(archive, spec, root / "models")


class FixtureTests(unittest.TestCase):
    def test_voice_names_with_spaces_and_language_availability(self):
        self.assertEqual(native.voices_from_output(
            "Eddy (English (US))    en_US    # Hello\nMonica es_ES # Hola\n"),
            [("Eddy (English (US))", "en_US"), ("Monica", "es_ES")])

    def test_fixtures_render_both_rates_and_report_missing_spanish(self):
        def run(command):
            if command == ["say", "-v", "?"]:
                return "Synthetic English en_US # Example\n"
            if command[0] == "say":
                Path(command[command.index("-o") + 1]).write_bytes(b"synthetic aiff placeholder")
            if command[0] == "afconvert":
                rate = int(command[command.index("-d") + 1].split("@")[1])
                with wave.open(command[-1], "wb") as output:
                    output.setnchannels(1)
                    output.setsampwidth(2)
                    output.setframerate(rate)
                    output.writeframes(b"\1\0\xff\xff" * rate)
            return ""

        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(native, "run_tool", side_effect=run), contextlib.redirect_stdout(io.StringIO()):
                manifest = native.create_fixtures(temporary)
            self.assertFalse(manifest["spanish_available"])
            self.assertEqual([row["sample_rate"] for row in manifest["fixtures"]], [16000, 48000])
            self.assertTrue(all(row["synthetic"] and row["language"] == "en" for row in manifest["fixtures"]))
            self.assertEqual(json.loads((Path(temporary) / "fixtures.json").read_text()), manifest)

    def test_missing_english_is_an_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(native, "run_tool", return_value=""), contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(native.PreparationError, "English"):
                    native.create_fixtures(temporary)


if __name__ == "__main__":
    unittest.main()
