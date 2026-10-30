#!/usr/bin/env python3
"""Check every PR commit independently and expose compiler errors as annotations."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


def commit_range(base, head, allow_empty=False):
    ancestor = subprocess.check_output(
        ["git", "merge-base", base, head], text=True
    ).strip()
    commits = subprocess.check_output(
        ["git", "rev-list", "--reverse", f"{ancestor}..{head}"], text=True
    ).splitlines()
    if (not commits and not allow_empty) or len(commits) > 256:
        raise ValueError("commit matrix must contain between 1 and 256 commits")
    return commits


def escape(value, property_value=False):
    value = str(value).replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    return value.replace(":", "%3A").replace(",", "%2C") if property_value else value


def annotations(diagnostic):
    if diagnostic.get("level") != "error":
        return []
    primary = [span for span in diagnostic.get("spans", []) if span.get("is_primary")]
    location = primary[:1] or [{}]
    result = []
    for span in location:
        properties = []
        for key, field in [("file", "file_name"), ("line", "line_start"), ("col", "column_start")]:
            if field in span:
                properties.append(f"{key}={escape(span[field], True)}")
        prefix = "::error" + (" " + ",".join(properties) if properties else "")
        result.append(f"{prefix}::{escape(diagnostic.get('message', 'compiler error'))}")
    for child in diagnostic.get("children", []):
        result.extend(annotations(child))
    return result


def check():
    failures = []
    for gate in ["check", "clippy"]:
        command = ["cargo", gate, "--locked", "--all-targets", "--message-format=json"]
        if gate == "clippy":
            command += ["--", "-D", "warnings"]
        print("+ " + " ".join(command), flush=True)
        process = subprocess.Popen(command, stdout=subprocess.PIPE, text=True)
        for line in process.stdout:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                print(line, end="", flush=True)
                continue
            if event.get("reason") == "compiler-message":
                diagnostic = event["message"]
                print(diagnostic.get("rendered", diagnostic["message"]), flush=True)
                for annotation in annotations(diagnostic):
                    print(annotation, flush=True)
        code = process.wait()
        if code:
            failures.append(gate)
            print(f"::error::{gate} failed with exit {code}", flush=True)
    sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as output:
            output.write(f"Commit `{sha}`: check={'FAIL' if 'check' in failures else 'pass'}, "
                         f"clippy={'FAIL' if 'clippy' in failures else 'pass'}.\n")
    return bool(failures)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("plan")
    plan.add_argument("--base", required=True)
    plan.add_argument("--head", required=True)
    plan.add_argument("--event", choices=["pull_request", "workflow_dispatch"], default="pull_request")
    plan.add_argument("--github-output", type=Path, required=True)
    commands.add_parser("check")
    args = parser.parse_args()
    if args.command == "check":
        return check()
    commits = commit_range(args.base, args.head, allow_empty=args.event == "workflow_dispatch")
    with args.github_output.open("a", encoding="utf-8") as output:
        output.write("commits=" + json.dumps(commits, separators=(",", ":")) + "\n")
    print(json.dumps({"commits": commits}))
    if not commits:
        print("Dispatch has no commits beyond merge-base; native runs as a head baseline.")
        if os.environ.get("GITHUB_STEP_SUMMARY"):
            with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as output:
                output.write("Dispatch has no commits beyond merge-base; commits job skipped, native is a head baseline.\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
