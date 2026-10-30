#!/usr/bin/env python3
"""Pinned model preparation and synthetic speech for the macOS quality lane.

Only the models subcommand uses the network. Catalog pins come from models.rs,
not from a second model list. Tests use synthetic artifacts and mocked tools.
"""

import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import ssl
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import wave


REPO = Path(__file__).resolve().parents[2]
MODEL_SOURCE = REPO / "crates/corti-transcribe-local/src/models.rs"
MODEL_IDS = ("parakeet", "parakeet-ggml", "vad")
MAX_ATTEMPTS = 3
DOWNLOAD_TIMEOUT_SECONDS = 30
DOWNLOAD_DEADLINE_SECONDS = 900
MAX_ARCHIVE_MEMBERS = 4096
MAX_UNPACKED_BYTES = 4 * 1024**3
CHUNK_BYTES = 1024 * 1024
FIXTURE_TEXT = {
    "en": (
        "Do not deploy version one point five. Nora has fifteen euros, not fifty. "
        "Please restart the gateway. Please restart the gateway. "
        "The budget is one thousand five hundred euros. Do not change the number."
    ),
    "es": (
        "No publiques la versión uno punto cinco. Nora tiene quince euros, no cincuenta. "
        "Reinicia la puerta de enlace. Reinicia la puerta de enlace. "
        "El presupuesto es de mil quinientos euros. No cambies el número."
    ),
}


class PreparationError(Exception):
    """Content-free preparation failure suitable for a CI error line."""


def relative_path(value):
    """Accept only ordinary relative POSIX paths from the pinned catalog/archive."""
    if not isinstance(value, str) or not value or "\\" in value:
        raise PreparationError("invalid model-relative path")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or not path.parts:
        raise PreparationError("unsafe model-relative path")
    return path


def catalog_from_source(source):
    """Read the catalog's deliberately simple literal/constant fields, failing closed.

    This is not a Rust parser. Unsupported expressions or a changed catalog shape
    require a reviewed update instead of silently using stale download pins.
    """
    constants = dict(re.findall(
        r'^pub const ([A-Z][A-Z0-9_]*):\s*(?:&str|u64)\s*=\s*(.+);\s*$',
        source, re.MULTILINE,
    ))

    def resolve(expression, field):
        expression = expression.strip()
        expression = constants.get(expression, expression)
        if expression.startswith('"'):
            try:
                value = json.loads(expression)
            except json.JSONDecodeError as error:
                raise PreparationError(f"unsupported catalog string: {field}") from error
            if isinstance(value, str):
                return value
        if re.fullmatch(r"[0-9][0-9_]*", expression):
            return int(expression.replace("_", ""))
        raise PreparationError(f"unsupported catalog expression: {field}")

    start = source.find("pub fn model_catalog()")
    end = source.find("pub fn model_catalog_for(", start)
    if start < 0:
        raise PreparationError("model catalog function missing")
    body = source[start:end if end >= 0 else len(source)]
    artifacts = {}
    for block in re.findall(r"ModelSpec\s*\{([^{}]*)\}", body):
        fields = dict(re.findall(r"^\s*(\w+):\s*(.+),\s*$", block, re.MULTILINE))
        if "id" not in fields:
            continue
        model_id = resolve(fields["id"], "id")
        if model_id not in MODEL_IDS:
            continue
        if model_id in artifacts:
            raise PreparationError("duplicate required model in catalog")
        try:
            row = {key: resolve(fields[key], key) for key in (
                "id", "url", "sha256", "download_bytes", "install_rel", "present_rel",
            )}
            row["kind"] = {
                "ArtifactKind::Tarball": "tarball", "ArtifactKind::File": "file",
            }[fields["kind"]]
            if fields["diarize_only"] != "false":
                raise PreparationError("required ASR/VAD model marked diarization-only")
        except KeyError as error:
            raise PreparationError("required model catalog field missing or unsupported") from error
        if model_id == "parakeet":
            members = dict(re.findall(
                r'parakeet_(encoder|decoder|joiner|tokens):\s*parakeet\.join\("([^"\n]+)"\)',
                source,
            ))
            if set(members) != {"encoder", "decoder", "joiner", "tokens"}:
                raise PreparationError("Parakeet installed-file layout missing or unsupported")
            row["required_paths"] = [
                str(relative_path(row["install_rel"]) / relative_path(members[key]))
                for key in ("encoder", "decoder", "joiner", "tokens")
            ]
        else:
            row["required_paths"] = [row["present_rel"]]
        artifacts[model_id] = row
    if set(artifacts) != set(MODEL_IDS):
        raise PreparationError("required ASR/VAD model missing from catalog")
    ordered = [artifacts[model_id] for model_id in MODEL_IDS]
    validate_artifacts(ordered)
    encoded = json.dumps(ordered, sort_keys=True, separators=(",", ":")).encode()
    return {
        "schema_version": 1,
        "cache_key": "corti-quality-models-v1-" + hashlib.sha256(encoded).hexdigest(),
        "artifacts": ordered,
    }


def validate_artifacts(artifacts):
    if [row.get("id") for row in artifacts] != list(MODEL_IDS):
        raise PreparationError("unexpected model catalog selection or order")
    for row in artifacts:
        if not re.fullmatch(r"[0-9a-f]{64}", row.get("sha256", "")):
            raise PreparationError("invalid pinned model digest")
        if type(row.get("download_bytes")) is not int or row["download_bytes"] <= 0:
            raise PreparationError("invalid pinned model size")
        url = urllib.parse.urlsplit(row.get("url", ""))
        if url.scheme != "https" or not url.hostname or url.username or url.password:
            raise PreparationError("invalid pinned model URL")
        install = relative_path(row["install_rel"])
        present = relative_path(row["present_rel"])
        required = [relative_path(path) for path in row["required_paths"]]
        if not required or len(set(required)) != len(required) or present not in required:
            raise PreparationError("invalid required model file set")
        if row["kind"] == "file":
            if required != [install]:
                raise PreparationError("bare model file has inconsistent install path")
        elif row["kind"] == "tarball":
            if any(not path.is_relative_to(install) or path == install for path in required):
                raise PreparationError("archive model file is outside its install directory")
        else:
            raise PreparationError("unsupported model artifact kind")


def atomic_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent,
                                     prefix=path.name + ".tmp-", delete=False) as output:
        temporary = Path(output.name)
        try:
            json.dump(value, output, ensure_ascii=False, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    try:
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def digest_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(CHUNK_BYTES), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_artifact(path, spec):
    path = Path(path)
    if path.is_symlink() or not path.is_file():
        raise PreparationError(f"model artifact missing or not a regular file: {spec['id']}")
    if path.stat().st_size != spec["download_bytes"]:
        raise PreparationError(f"model artifact size mismatch: {spec['id']}")
    if digest_file(path) != spec["sha256"]:
        raise PreparationError(f"model artifact SHA-256 mismatch: {spec['id']}")


def download_error_category(error):
    """Describe transport failure without exposing URLs or server/native text."""
    if isinstance(error, urllib.error.HTTPError):
        return f"http_status_{error.code}"
    reason = error.reason if isinstance(error, urllib.error.URLError) else error
    if isinstance(reason, ssl.SSLCertVerificationError):
        return "tls_certificate_verification"
    if isinstance(reason, ssl.SSLError):
        return "tls"
    if isinstance(reason, TimeoutError):
        return "timeout"
    if isinstance(reason, http.client.IncompleteRead):
        return "incomplete_http_body"
    if isinstance(reason, BaseException):
        return type(reason).__name__
    return "url_error"


def download_artifact(spec, destination):
    """Retry transport failures only; incorrect bytes fail without being cached."""
    destination = Path(destination)
    for attempt in range(MAX_ATTEMPTS):
        fd, name = tempfile.mkstemp(prefix="download-", dir=destination.parent)
        temporary = Path(name)
        try:
            deadline = time.monotonic() + DOWNLOAD_DEADLINE_SECONDS
            request = urllib.request.Request(spec["url"], headers={
                "User-Agent": "Mozilla/5.0", "Accept-Encoding": "identity",
            })
            with os.fdopen(fd, "wb") as output:
                with urllib.request.urlopen(request, timeout=DOWNLOAD_TIMEOUT_SECONDS) as response:
                    count = 0
                    while True:
                        if time.monotonic() > deadline:
                            raise TimeoutError("model download deadline")
                        chunk = response.read(CHUNK_BYTES)
                        if not chunk:
                            break
                        count += len(chunk)
                        if count > spec["download_bytes"]:
                            raise PreparationError(f"model artifact exceeds pinned size: {spec['id']}")
                        output.write(chunk)
                output.flush()
                os.fsync(output.fileno())
            verify_artifact(temporary, spec)
            temporary.replace(destination)
            return
        except (urllib.error.URLError, http.client.HTTPException, TimeoutError, ConnectionError) as error:
            if attempt + 1 == MAX_ATTEMPTS:
                raise PreparationError(
                    f"model download failed after {MAX_ATTEMPTS} attempts: {spec['id']} "
                    f"(cause={download_error_category(error)})"
                ) from error
            time.sleep(2**attempt)
        finally:
            temporary.unlink(missing_ok=True)


def safe_destination(root, relative):
    relative = relative_path(str(relative))
    current = root
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            raise PreparationError("refusing symlink in model destination")
    return current


def install_artifact(artifact, spec, model_dir):
    """Extract only the app's required regular files; never use extractall."""
    verify_artifact(artifact, spec)
    model_dir = Path(model_dir)
    if model_dir.is_symlink():
        raise PreparationError("refusing symlink model directory")
    model_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="install-", dir=model_dir) as temporary:
        staging = Path(temporary)
        required = {str(relative_path(path)) for path in spec["required_paths"]}
        if spec["kind"] == "file":
            target = staging / spec["install_rel"]
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(artifact, target)
        else:
            found = set()
            expanded = 0
            with tarfile.open(artifact, mode="r|bz2") as archive:
                for count, member in enumerate(archive, start=1):
                    if count > MAX_ARCHIVE_MEMBERS:
                        raise PreparationError("model archive has too many members")
                    name = str(relative_path(member.name))
                    if not member.isfile() and not member.isdir():
                        raise PreparationError("model archive contains a non-regular member")
                    expanded += member.size
                    if member.size < 0 or expanded > MAX_UNPACKED_BYTES:
                        raise PreparationError("model archive exceeds unpacked size bound")
                    if name not in required:
                        continue
                    if name in found or not member.isfile() or member.size == 0:
                        raise PreparationError("model archive has invalid required member")
                    target = staging / name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    source = archive.extractfile(member)
                    if source is None:
                        raise PreparationError("required model archive member cannot be read")
                    with source, target.open("wb") as output:
                        shutil.copyfileobj(source, output, CHUNK_BYTES)
                    if target.stat().st_size != member.size:
                        raise PreparationError("model archive member is truncated")
                    found.add(name)
            if found != required:
                raise PreparationError("model archive is missing required files")
        for name in sorted(required):
            target = safe_destination(model_dir, name)
            target.parent.mkdir(parents=True, exist_ok=True)
            (staging / name).replace(target)


def prepare_models(catalog, cache_dir, model_dir):
    validate_artifacts(catalog["artifacts"])
    cache_dir = Path(cache_dir)
    if cache_dir.is_symlink():
        raise PreparationError("refusing symlink model cache directory")
    cache_dir.mkdir(parents=True, exist_ok=True)
    for spec in catalog["artifacts"]:
        artifact = cache_dir / (spec["sha256"] + ".artifact")
        if not artifact.exists() and not artifact.is_symlink():
            download_artifact(spec, artifact)
        # Never trust a restored cache or an existing installation. Verify the
        # cached archive, then rebuild installed files from those verified bytes.
        verify_artifact(artifact, spec)
        install_artifact(artifact, spec, model_dir)
        print(f"MODEL {spec['id']} sha256={spec['sha256']} verified", flush=True)


def voices_from_output(output):
    voices = []
    for line in output.splitlines():
        match = re.match(r"^\s*(.+?)\s+([a-z]{2,3}_[A-Za-z_]+)\s+#", line)
        if match:
            voices.append((match.group(1), match.group(2)))
    return voices


def run_tool(command):
    try:
        return subprocess.run(command, check=True, capture_output=True, text=True, timeout=120).stdout
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        # Native stderr is not copied into error messages. Fixture text is the
        # only content the lane may deliberately publish.
        raise PreparationError(f"fixture tool failed: {Path(command[0]).name}") from error


def create_fixtures(output_dir):
    output_dir = Path(output_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    listing = run_tool(["say", "-v", "?"])
    print(listing, end="" if listing.endswith("\n") else "\n", flush=True)
    voices = voices_from_output(listing)
    selected = {}
    for language, preferred in (("en", "en_US"), ("es", "es_ES")):
        candidates = [voice for voice in voices if voice[1].startswith(language + "_")]
        if candidates:
            selected[language] = min(candidates, key=lambda item: (item[1] != preferred, item))
    if "en" not in selected:
        raise PreparationError("no installed English speech voice")
    manifest = {"schema_version": 1, "spanish_available": "es" in selected, "fixtures": []}
    print("SPANISH " + ("available" if manifest["spanish_available"] else "unavailable"), flush=True)
    for language in ("en", "es"):
        if language not in selected:
            continue
        voice, locale = selected[language]
        text_path = output_dir / f"{language}.txt"
        text_path.write_text(FIXTURE_TEXT[language] + "\n", encoding="utf-8")
        aiff = output_dir / f"{language}.aiff"
        run_tool(["say", "-v", voice, "-r", "165", "-o", str(aiff), "-f", str(text_path)])
        for sample_rate in (16000, 48000):
            path = output_dir / f"{language}-{sample_rate}.wav"
            run_tool(["afconvert", "-f", "WAVE", "-d", f"LEI16@{sample_rate}",
                      "-c", "1", str(aiff), str(path)])
            with wave.open(str(path), "rb") as audio:
                if (audio.getnchannels() != 1 or audio.getframerate() != sample_rate
                        or audio.getsampwidth() != 2 or audio.getnframes() <= sample_rate):
                    raise PreparationError("generated speech WAV has unexpected format or duration")
                frames = audio.getnframes()
                audible = False
                for block in iter(lambda: audio.readframes(4096), b""):
                    audible = audible or any(block)
                if not audible:
                    raise PreparationError("generated speech WAV contains only digital silence")
            manifest["fixtures"].append({
                "language": language, "sample_rate": sample_rate, "path": str(path),
                "voice": voice, "locale": locale, "text": FIXTURE_TEXT[language],
                "sha256": digest_file(path), "frames": frames, "synthetic": True,
            })
    atomic_json(output_dir / "fixtures.json", manifest)
    return manifest


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    catalog_cmd = commands.add_parser("catalog")
    catalog_cmd.add_argument("--source", type=Path, default=MODEL_SOURCE)
    catalog_cmd.add_argument("--output", type=Path, required=True)
    catalog_cmd.add_argument("--github-output", type=Path)
    models_cmd = commands.add_parser("models")
    models_cmd.add_argument("--catalog", type=Path, required=True)
    models_cmd.add_argument("--cache-dir", type=Path, required=True)
    models_cmd.add_argument("--model-dir", type=Path, required=True)
    fixture_cmd = commands.add_parser("fixtures")
    fixture_cmd.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "catalog":
            catalog = catalog_from_source(args.source.read_text(encoding="utf-8"))
            atomic_json(args.output, catalog)
            if args.github_output:
                with args.github_output.open("a", encoding="utf-8") as output:
                    output.write(f"model_cache_key={catalog['cache_key']}\n")
            print(f"MODEL_CACHE_KEY {catalog['cache_key']}")
        elif args.command == "models":
            catalog = json.loads(args.catalog.read_text(encoding="utf-8"))
            # Reject stale/manually modified manifests before accessing the network.
            expected = catalog_from_source(MODEL_SOURCE.read_text(encoding="utf-8"))
            if catalog != expected:
                raise PreparationError("model manifest does not match the app's pinned catalog")
            prepare_models(catalog, args.cache_dir, args.model_dir)
        else:
            create_fixtures(args.output_dir)
    except PreparationError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    except (OSError, ValueError, TypeError, KeyError, tarfile.TarError, wave.Error) as error:
        print(f"::error::native preparation failed ({type(error).__name__})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
