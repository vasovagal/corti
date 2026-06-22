#!/usr/bin/env python3
"""Aggregate sweep result JSONL into ranked tables + a memory/accuracy Pareto front.

  analyze.py asr  bench/results/asr_round1.jsonl [more.jsonl ...]
  analyze.py aec  bench/results/aec_round1.jsonl
  analyze.py diar bench/results/diar_round1.jsonl

Means each metric over fixtures, ranks, and (asr) prints the WER-vs-RSS Pareto front.
"""
import collections
import json
import sys


def load(paths):
    rows = []
    for p in paths:
        for ln in open(p):
            ln = ln.strip()
            if ln:
                rows.append(json.loads(ln))
    return rows


def asr(paths):
    rows = [r for r in load(paths) if r.get("ok")]
    by = collections.defaultdict(list)
    for r in rows:
        by[r["run"]].append(r)
    agg = {}
    for run, rs in by.items():
        agg[run] = (
            sum(x["wer_norm"] for x in rs) / len(rs),
            sum(x["peak_rss_mb"] for x in rs) / len(rs),
            sum(x["asr_ms"] for x in rs) / len(rs),
            rs[0].get("flags", {}),
        )
    base = agg.get("baseline", (0, 0, 0, {}))[0]
    print(f"{'run':22} {'WER':>8} {'ΔWER':>8} {'RSS_MB':>8} {'asr_ms':>8}  flags")
    for run, (wn, rss, ms, fl) in sorted(agg.items(), key=lambda kv: kv[1][0]):
        print(f"{run:22} {wn:8.4f} {wn-base:+8.4f} {rss:8.0f} {ms:8.0f}  {fl}")
    # Pareto front (minimize WER and RSS).
    pts = [(run, wn, rss) for run, (wn, rss, _, _) in agg.items()]
    front = []
    for run, wn, rss in sorted(pts, key=lambda x: (x[1], x[2])):
        if not any(o_wn <= wn and o_rss <= rss and (o_wn, o_rss) != (wn, rss) for _, o_wn, o_rss in pts):
            front.append((run, wn, rss))
    print("\nPareto front (WER vs RSS):")
    for run, wn, rss in sorted(front, key=lambda x: x[1]):
        print(f"  {run:22} WER={wn:.4f} RSS={rss:.0f}MB")


def aec(paths):
    rows = [r for r in load(paths) if r.get("ok")]
    by = collections.defaultdict(dict)
    for r in rows:
        by[r["run"]][r["fixture"]] = r["erle"]
    print(f"{'run':30} {'echoonly_ERLE':>13} {'dt_ERLE':>8} {'dt_segSNR':>10} {'dt_corr':>8}  flags")
    flags = {r["run"]: r.get("flags", {}) for r in rows}
    def k(d, f, key):
        return (d.get(f) or {}).get(key)
    rank = sorted(by.items(), key=lambda kv: -((k(kv[1], 'echoonly', 'erle_db')) or -99))
    for run, d in rank:
        eo = k(d, "echoonly", "erle_db")
        dt = k(d, "doubletalk", "erle_db")
        sn = k(d, "doubletalk", "near_seg_snr_db")
        co = k(d, "doubletalk", "near_correlation")
        print(f"{run:30} {fmt(eo):>13} {fmt(dt):>8} {fmt(sn):>10} {fmt(co):>8}  {flags[run]}")


def diar(paths):
    rows = [r for r in load(paths) if r.get("ok")]
    print(f"{'run':16} {'cpWER':>8} {'ref_spk':>8} {'hyp_spk':>8} {'spk_err':>8} {'RSS_MB':>8}  flags")
    for r in rows:
        cp = r.get("cpwer", {})
        print(f"{r['run']:16} {fmt(cp.get('cpwer')):>8} {fmt(cp.get('ref_speakers')):>8} "
              f"{fmt(cp.get('hyp_speakers')):>8} {fmt(cp.get('speaker_count_error')):>8} "
              f"{r.get('peak_rss_mb',0):8.0f}  {r.get('flags',{})}")


def fmt(v):
    if v is None:
        return "—"
    return f"{v:.3f}" if isinstance(v, float) else str(v)


if __name__ == "__main__":
    mode = sys.argv[1]
    {"asr": asr, "aec": aec, "diar": diar}[mode](sys.argv[2:])
