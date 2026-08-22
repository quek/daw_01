#!/usr/bin/env python3

# SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
# SPDX-License-Identifier: GPL-3.0-or-later

"""Flatten the adversarial-audit workflow result and adjudicate every candidate
against the REAL guard engine (sandboxed). Prints confirmed mismatches only.

usage: python process_audit.py <workflow_output_file>
"""
import sys
import os
import re
import json
import shutil
import tempfile
import subprocess

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass

HERE = os.path.dirname(os.path.abspath(__file__))
ENGINE = os.path.join(HERE, "guard_engine.py")
REAL_GUARDS = os.path.join(os.path.expanduser("~"), ".claude", "projects",
                           "F--dev-daw-01", "guards.jsonl")
# optional 2nd arg: adjudicate against a candidate registry (e.g. guards.proposed.jsonl).
GUARDS_SRC = sys.argv[2] if len(sys.argv) > 2 else REAL_GUARDS

sandbox = tempfile.mkdtemp(prefix="guard_proc_")
proj = os.path.join(sandbox, ".claude", "projects", "F--dev-daw-01")
os.makedirs(proj, exist_ok=True)
shutil.copyfile(GUARDS_SRC, os.path.join(proj, "guards.jsonl"))
ENV = dict(os.environ)
ENV.update(USERPROFILE=sandbox, HOME=sandbox, HOMEDRIVE="", HOMEPATH=sandbox)


def run(tool, tool_input):
    payload = json.dumps({"session_id": "ADJ", "tool_name": tool, "tool_input": tool_input})
    p = subprocess.run([sys.executable, ENGINE], input=payload.encode("utf-8"),
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=ENV)
    out = (p.stdout + p.stderr).decode("utf-8", "replace")
    return p.returncode, set(re.findall(r"\[(?:warn|BLOCK)\]\s+(\S+)", out))


def extract_json_array(raw):
    try:
        v = json.loads(raw)
        if isinstance(v, list):
            return v
        if isinstance(v, dict) and isinstance(v.get("result"), list):
            return v["result"]
    except Exception:
        pass
    i = raw.find("[")
    j = raw.rfind("]")
    if i >= 0 and j > i:
        return json.loads(raw[i:j + 1])
    raise SystemExit("could not find JSON array in output")


def load_actions():
    acts = {}
    with open(GUARDS_SRC, "r", encoding="utf-8") as fh:
        for ln in fh:
            ln = ln.strip()
            if not ln or ln.startswith("#"):
                continue
            try:
                r = json.loads(ln)
                acts[r.get("id")] = r.get("action")
            except Exception:
                pass
    return acts


def main():
    raw = open(sys.argv[1], "r", encoding="utf-8").read()
    groups = extract_json_array(raw)
    actions = load_actions()
    flat = []
    for g in groups:
        gid = g.get("guard_id")
        for c in g.get("candidates", []):
            c = dict(c)
            c["guard_id"] = gid
            flat.append(c)

    # classify each candidate after running it through the real engine
    true_gap = []        # intent=fire, engine fired NOTHING -> real coverage hole
    family_covered = []  # intent=fire, audited rule missed but a sibling fired -> policy still enforced
    wrongful_block = []  # intent=skip, audited rule fired with action=block -> wrongful cancel (SERIOUS)
    wrongful_warn = []   # intent=skip, audited rule fired with action=warn -> noise only
    bad_inputs = []
    n_ok_fn = 0          # intent=fire & audited fired (correct)
    n_ok_fp = 0          # intent=skip & nothing-relevant fired (correct)

    for c in flat:
        gid = c["guard_id"]
        try:
            ti = json.loads(c["tool_input_json"])
        except Exception as e:
            bad_inputs.append((c, str(e)))
            continue
        rc, fired = run(c["tool"], ti)
        c["_fired"] = sorted(fired)
        audited_fired = gid in fired
        intent = bool(c["intent_should_fire"])
        if intent:
            if audited_fired:
                n_ok_fn += 1
            elif fired:
                family_covered.append(c)
            else:
                true_gap.append(c)
        else:
            if audited_fired:
                (wrongful_block if actions.get(gid) == "block" else wrongful_warn).append(c)
            else:
                n_ok_fp += 1

    print("=== ADVERSARIAL ADJUDICATION (real engine = ground truth) ===")
    print("guards: %d   candidates: %d   bad-json: %d" % (len(groups), len(flat), len(bad_inputs)))
    print("correct: %d fire-as-intended + %d skip-as-intended" % (n_ok_fn, n_ok_fp))
    print("TRUE COVERAGE GAPS (nothing fired):      %d" % len(true_gap))
    print("family-covered (sibling guard caught it): %d" % len(family_covered))
    print("WRONGFUL BLOCKS (innocent -> cancelled):  %d" % len(wrongful_block))
    print("wrongful warns (innocent -> noise):       %d\n" % len(wrongful_warn))

    def dump(title, items, show_fired=True):
        print("=== %s (%d) ===" % (title, len(items)))
        cur = None
        for c in items:
            if c["guard_id"] != cur:
                cur = c["guard_id"]
                print("\n### %s [action=%s]" % (cur, actions.get(cur)))
            print("  - %s" % c["label"])
            print("    tool=%s input=%s" % (c["tool"], c["tool_input_json"]))
            if show_fired:
                print("    engine fired: %s" % (c["_fired"] or "NOTHING"))
            print("    why: %s" % c.get("rationale", "")[:300])
        print("")

    dump("WRONGFUL BLOCKS -- innocent input cancelled by a block guard (SERIOUS)", wrongful_block)
    dump("TRUE COVERAGE GAPS -- real mistake that NO guard catches", true_gap)
    dump("WRONGFUL WARNS -- innocent input gets a warn (low severity / noise)", wrongful_warn)
    dump("FAMILY-COVERED -- audited rule missed but a sibling guard caught it (OK)", family_covered)

    if bad_inputs:
        print("=== candidates with unparseable tool_input_json (skipped) ===")
        for c, e in bad_inputs:
            print("  %s :: %s :: %s" % (c["guard_id"], c.get("label"), e))

    shutil.rmtree(sandbox, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
