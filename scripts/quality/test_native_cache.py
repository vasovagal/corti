"""Synthetic restored-cache tests; Cargo is always mocked and no libraries run."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("native_cache", Path(__file__).with_name("native_cache.py"))
native_cache = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(native_cache)

# The locked build script's complete static link output, used only as fixture data.
LIBRARIES = (
    "sherpa-onnx-c-api", "sherpa-onnx-core", "kaldi-decoder-core",
    "sherpa-onnx-kaldifst-core", "sherpa-onnx-fstfar", "sherpa-onnx-fst",
    "kaldi-native-fbank-core", "kissfft-float", "piper_phonemize", "espeak-ng",
    "ucd", "onnxruntime", "ssentencepiece_core",
)
ARCHIVE = "sherpa-onnx-v1.13.2-osx-arm64-static-lib"


class NativeCacheTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.repo = Path(temporary.name).resolve()
        self.target = self.repo / "target with spaces"
        self.extraction = self.target / "sherpa-onnx-prebuilt" / ARCHIVE
        self.lib_dir = self.extraction / "lib"
        self.commands = []
        self.runner = patch.object(native_cache.subprocess, "run", side_effect=self.run_cargo).start()
        self.addCleanup(patch.stopall)

    def run_cargo(self, command, **kwargs):
        self.commands.append(command)
        self.assertEqual(kwargs["cwd"], self.repo)
        self.assertTrue(kwargs["check"])
        if command[1] == "metadata":
            self.assertEqual(command, ["cargo", "metadata", "--format-version", "1",
                                       "--no-deps", "--locked", "--offline"])
            return native_cache.subprocess.CompletedProcess(command, 0, json.dumps({
                "target_directory": str(self.target), "build_directory": str(self.target),
            }))
        self.assertEqual(command[:6], ["cargo", "clean", "--locked", "-p",
                                      "sherpa-onnx-sys", "--target"])
        # A clean must happen after any incomplete extraction was removed.
        self.assertFalse(self.extraction.exists())
        return native_cache.subprocess.CompletedProcess(command, 0)

    def output(self, profile="debug", host=False, lib_dir=None, package="sherpa-onnx-sys", suffix="a" * 16):
        root = self.target if host else self.target / native_cache.TARGET
        path = root / profile / "build" / f"{package}-{suffix}" / "output"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            "cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR\n"
            f"cargo:rustc-link-search=native={lib_dir or self.lib_dir}\n"
            + "".join(f"cargo:rustc-link-lib=static={name}\n" for name in LIBRARIES)
            + "cargo:rustc-link-lib=dylib=c++\ncargo:rustc-link-lib=framework=Foundation\n",
            encoding="utf-8",
        )
        return path

    def libraries(self):
        self.lib_dir.mkdir(parents=True)
        for name in LIBRARIES:
            (self.lib_dir / f"lib{name}.a").write_bytes(b"synthetic archive")

    def repair(self, env=None):
        with contextlib.redirect_stdout(io.StringIO()):
            return native_cache.repair_cache(self.repo, {} if env is None else env)

    def assert_cleaned_both_profiles(self):
        self.assertEqual(self.commands[1:], [
            ["cargo", "clean", "--locked", "-p", "sherpa-onnx-sys",
             "--target", "aarch64-apple-darwin", "--target-dir", str(self.target),
             "--profile", profile]
            for profile in ("dev", "release")
        ])

    def test_complete_thirteen_libraries_do_not_clean(self):
        self.output()
        self.libraries()
        self.assertEqual(self.repair(), 0)
        self.assertEqual(len(self.commands), 1)
        self.assertEqual(len(list(self.lib_dir.iterdir())), 13)

    def test_cold_cache_does_not_clean_or_create_target(self):
        self.assertEqual(self.repair(), 0)
        self.assertEqual(len(self.commands), 1)
        self.assertFalse(self.target.exists())

    def test_external_metadata_directories_are_rejected_before_cleanup(self):
        for key in ("target_directory", "build_directory"):
            with self.subTest(directory=key):
                metadata = {"target_directory": str(self.target), "build_directory": str(self.target)}
                metadata[key] = str(self.repo.parent / "shared-cargo-target")
                self.runner.side_effect = None
                self.runner.return_value = native_cache.subprocess.CompletedProcess(
                    ["cargo", "metadata"], 0, json.dumps(metadata),
                )
                self.runner.reset_mock()
                with self.assertRaises(native_cache.CacheRepairError):
                    self.repair()
                self.assertEqual(self.runner.call_count, 1)
                self.assertEqual(self.runner.call_args.args[0][1], "metadata")
                self.assertFalse(self.target.exists())

    def test_metadata_directory_cannot_alias_checkout_or_follow_symlink(self):
        external = self.repo / "another-checkout"
        external.mkdir()
        self.target.symlink_to(external, target_is_directory=True)
        with self.assertRaises(native_cache.CacheRepairError):
            self.repair()
        self.target.unlink()
        self.target = self.repo
        with self.assertRaises(native_cache.CacheRepairError):
            self.repair()
        self.assertEqual(len(self.commands), 2)

    def test_missing_native_tree_cleans_both_profiles(self):
        self.output()
        self.assertEqual(self.repair(), 1)
        self.assert_cleaned_both_profiles()

    def test_partial_lib_directory_is_removed_but_archive_and_neighbors_remain(self):
        self.output()
        self.libraries()
        (self.lib_dir / "libssentencepiece_core.a").unlink()
        archive = self.extraction.with_name(ARCHIVE + ".tar.bz2")
        archive.write_bytes(b"cached compressed archive")
        neighbor = self.extraction.with_name("unrelated")
        neighbor.mkdir()
        sentinel = neighbor / "keep"
        sentinel.write_bytes(b"unrelated native cache")
        unrelated = self.output(package="another-package")
        self.assertEqual(self.repair(), 1)
        self.assertFalse(self.extraction.exists())
        self.assertEqual(archive.read_bytes(), b"cached compressed archive")
        self.assertEqual(sentinel.read_bytes(), b"unrelated native cache")
        self.assertTrue(unrelated.is_file())
        self.assert_cleaned_both_profiles()

    def test_empty_lib_directory_and_zero_length_archive_are_incomplete(self):
        for empty in (True, False):
            with self.subTest(empty=empty):
                self.commands.clear()
                self.output()
                self.libraries()
                for library in self.lib_dir.iterdir():
                    if empty:
                        library.unlink()
                if not empty:
                    (self.lib_dir / "libonnxruntime.a").write_bytes(b"")
                self.assertEqual(self.repair(), 1)
                self.assert_cleaned_both_profiles()

    def test_host_and_target_debug_release_outputs_deduplicate_extraction(self):
        for host in (True, False):
            for profile in ("debug", "release"):
                self.output(profile=profile, host=host)
        with patch.object(native_cache.shutil, "rmtree", wraps=native_cache.shutil.rmtree) as remove:
            self.libraries()
            (self.lib_dir / "libonnxruntime.a").unlink()
            self.assertEqual(self.repair(), 1)
        remove.assert_called_once_with(self.extraction)
        self.assert_cleaned_both_profiles()

    def test_external_override_does_not_even_invoke_metadata(self):
        self.output()
        self.libraries()
        for value in (str(self.lib_dir), ""):
            self.assertEqual(self.repair({"SHERPA_ONNX_LIB_DIR": value}), 0)
        self.runner.assert_not_called()
        self.assertTrue(self.lib_dir.is_dir())

    def test_previous_external_override_is_untouched(self):
        external = self.repo / "external" / "lib"
        external.mkdir(parents=True)
        self.output(lib_dir=external)
        self.assertEqual(self.repair(), 0)
        self.assertEqual(len(self.commands), 1)
        self.assertTrue(external.is_dir())

    def test_malformed_or_unsupported_paths_fail_before_mutation(self):
        for value in (
            "relative/lib", str(self.target / ".." / "elsewhere" / "lib"),
            str(self.extraction / "nested" / "lib"),
            str(self.target / "sherpa-onnx-prebuilt" / "unrelated" / "lib"),
            str(self.extraction).replace("static-lib", "shared-lib") + "/lib",
        ):
            with self.subTest(path=value):
                self.commands.clear()
                self.output(lib_dir=value)
                with self.assertRaises(native_cache.CacheRepairError):
                    self.repair()
                self.assertEqual(len(self.commands), 1)

    def test_invalid_later_output_prevents_earlier_repair(self):
        self.output()
        self.libraries()
        (self.lib_dir / "libonnxruntime.a").unlink()
        self.output(profile="release", lib_dir="relative/lib")
        with self.assertRaises(native_cache.CacheRepairError):
            self.repair()
        self.assertTrue(self.extraction.is_dir())
        self.assertEqual(len(self.commands), 1)

    def test_symlink_extraction_and_library_are_never_followed(self):
        external = self.repo / "external"
        external.mkdir()
        sentinel = external / "keep"
        sentinel.write_bytes(b"keep")
        self.output()
        self.extraction.parent.mkdir(parents=True)
        self.extraction.symlink_to(external, target_is_directory=True)
        with self.assertRaises(native_cache.CacheRepairError):
            self.repair()
        self.extraction.unlink()
        self.libraries()
        library = self.lib_dir / "libonnxruntime.a"
        library.unlink()
        library.symlink_to(sentinel)
        with self.assertRaises(native_cache.CacheRepairError):
            self.repair()
        self.assertEqual(sentinel.read_bytes(), b"keep")
        self.assertTrue(self.extraction.is_dir())
        self.assertEqual(len(self.commands), 2)

    def test_symlink_build_output_is_not_read(self):
        output = self.output()
        external = self.repo / "external-output"
        output.replace(external)
        output.symlink_to(external)
        with self.assertRaises(native_cache.CacheRepairError):
            self.repair()
        self.assertEqual(len(self.commands), 1)
        self.assertTrue(external.is_file())

    def test_malformed_link_directive_is_rejected(self):
        output = self.output()
        output.write_text(output.read_text().replace("static=onnxruntime", "static=../onnxruntime"))
        with self.assertRaises(native_cache.CacheRepairError):
            self.repair()
        self.assertEqual(len(self.commands), 1)


if __name__ == "__main__":
    unittest.main()
