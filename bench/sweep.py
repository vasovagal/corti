#!/usr/bin/env python3
"""Serial parameter-sweep runner for the corti bench harness.

ONE Mac → ONE experiment at a time. This runs each (config × fixture) through `corti-bench` strictly
serially under `/usr/bin/time -l` (peak RSS) — corti-bench's own flock is the backstop. Results stream to a
JSONL file (resumable: a (run,fixture) already present is skipped), so a long unattended sweep survives
interruption and the optimizer can read partial progress live.

Modes:
  asr  — `corti-bench process` on a mono pristine clip → WER (vs the clip's aligned reference) + peak RSS +
         asr_ms. (Add diarize:true + a turns ref to also get cpWER.)
  aec  — `corti-bench aec` on a 2-track synthetic fixture → dB-ERLE + near-end preservation via erle.py.

Spec JSON: {"mode":"asr","fixtures":[{"id","wav","ref"[, "turns"]}],"runs":[{"name","flags":{...}}]}
Flags use corti-bench's CLI names (e.g. "vad-threshold", "asr-decoding", "asr-beam", "diarize":true).
"""
import argparse
import json
import re
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/aarch64-apple-darwin/release/corti-bench"
PY = ROOT / "bench/.venv/bin/python"
SCOR = ROOT / "bench/scoring"
RSS_RE = re.compile(r"(\d+)\s+maximum resident set size")


def flags_to_argv(flags: dict) -> list[str]:
    argv = []
    for k, v in flags.items():
        if v is True:
            argv.append(f"--{k}")
        elif v is False or v is None:
            continue
        else:
            argv += [f"--{k}", str(v)]
    return argv


def time_l(cmd: list[str]) -> tuple[subprocess.CompletedProcess, int]:
    p = subprocess.run(["/usr/bin/time", "-l"] + cmd, capture_output=True, text=True)
    m = RSS_RE.search(p.stderr)
    rss = int(m.group(1)) if m else 0
    return p, rss


def last_json(stdout: str) -> dict:
    lines = [ln for ln in stdout.strip().splitlines() if ln.strip().startswith("{")]
    return json.loads(lines[-1]) if lines else {}


def score_wer(ref: Path, hyp: Path) -> dict:
    r = subprocess.run([str(PY), str(SCOR / "wer.py"), "--ref", str(ref), "--hyp", str(hyp)],
                       capture_output=True, text=True)
    return json.loads(r.stdout) if r.stdout.strip() else {"error": r.stderr[-300:]}


def score_cpwer(turns: Path, hyp: Path) -> dict:
    r = subprocess.run([str(PY), str(SCOR / "cpwer.py"), "--ref-turns", str(turns), "--hyp", str(hyp)],
                       capture_output=True, text=True)
    return json.loads(r.stdout) if r.stdout.strip() else {"error": r.stderr[-300:]}


def run_asr(fix: dict, run: dict, tmp: Path) -> dict:
    hyp = tmp / f"{run['name']}__{fix['id']}.json"
    cmd = [str(BIN), "process", "--input", fix["wav"], "--out", str(hyp)] + flags_to_argv(run.get("flags", {}))
    t0 = time.time()
    p, rss = time_l(cmd)
    wall = time.time() - t0
    if p.returncode != 0:
        return {"ok": False, "stderr": p.stderr[-400:]}
    env = last_json(p.stdout)
    wer = score_wer(Path(fix["ref"]), hyp)
    out = {
        "ok": True,
        "wer_norm": wer.get("wer_normalized"),
        "wer_raw": wer.get("wer_raw"),
        "peak_rss_mb": round(rss / 1024 / 1024, 1),
        "asr_ms": env.get("asr_ms"),
        "wall_s": round(wall, 1),
        "n_segments": env.get("n_segments"),
    }
    if run.get("flags", {}).get("diarize") and fix.get("turns"):
        out["cpwer"] = score_cpwer(Path(fix["turns"]), hyp)
    return out


def run_aec(fix: dict, run: dict, tmp: Path) -> dict:
    dump = tmp / f"{run['name']}__{fix['id']}_ch"
    clean = tmp / f"{run['name']}__{fix['id']}_clean.wav"
    cmd = [str(BIN), "aec", "--input", fix["wav"], "--out", str(clean),
           "--dump-channels", str(dump)] + flags_to_argv(run.get("flags", {}))
    t0 = time.time()
    p, rss = time_l(cmd)
    if p.returncode != 0:
        return {"ok": False, "stderr": p.stderr[-400:]}
    erle_cmd = [str(PY), str(SCOR / "erle.py"), "--mic", str(dump / "mic.wav"),
                "--far", str(dump / "far.wav"), "--out", str(dump / "clean.wav")]
    if fix.get("near"):
        erle_cmd += ["--near", fix["near"]]
    r = subprocess.run(erle_cmd, capture_output=True, text=True)
    erle = json.loads(r.stdout) if r.stdout.strip() else {"error": r.stderr[-300:]}
    return {"ok": True, "erle": erle, "wall_s": round(time.time() - t0, 1), "peak_rss_mb": round(rss / 1024 / 1024, 1)}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--spec", required=True)
    ap.add_argument("--out", required=True, help="results.jsonl (appended; resumable)")
    ap.add_argument("--tmp", default="/tmp/corti-sweep")
    a = ap.parse_args()
    spec = json.loads(Path(a.spec).read_text())
    mode = spec.get("mode", "asr")
    tmp = Path(a.tmp)
    tmp.mkdir(parents=True, exist_ok=True)
    out_path = Path(a.out)
    done = set()
    if out_path.exists():
        for ln in out_path.read_text().splitlines():
            try:
                d = json.loads(ln)
                done.add((d["run"], d["fixture"]))
            except Exception:
                pass

    runner = run_aec if mode == "aec" else run_asr
    total = len(spec["runs"]) * len(spec["fixtures"])
    i = 0
    with out_path.open("a") as f:
        for run in spec["runs"]:
            for fix in spec["fixtures"]:
                i += 1
                key = (run["name"], fix["id"])
                if key in done:
                    print(f"[{i}/{total}] skip {key}", flush=True)
                    continue
                print(f"[{i}/{total}] {mode} run={run['name']} fixture={fix['id']} flags={run.get('flags', {})}", flush=True)
                res = runner(fix, run, tmp)
                rec = {"run": run["name"], "fixture": fix["id"], "flags": run.get("flags", {}), **res}
                f.write(json.dumps(rec) + "\n")
                f.flush()
                short = {k: rec.get(k) for k in ("wer_norm", "peak_rss_mb", "asr_ms", "erle") if k in rec}
                print(f"    -> {short}", flush=True)
    print(f"done: {i} cells, results in {out_path}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
