#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""bulk_import_accounts.py — WorkBuddy 管理面板批量账号导入向导。

用途：读入 workbuddy-accounts 导出 JSON（或手写列表/单号"), 过面板 /accounts/import
批量入库，支持 dry-run / overwrite / 强制 realm / 分批 pause。
可直接运行：python3 bulk_import_accounts.py --file accounts.json --dry-run
写作注意：
  - 只使用标准库；所有凭证明文只写入运行内存，控制台只显示 uid 前 8 位和 nickname。
  - 默认面板地址 http://127.0.0.1:8788、密码 admin；均可用 WB_BASE / WB_PANEL_PW 覆盖。
  - 国内版批量 import 环境下，panel 侧会 import 后自动触发 checkin（CN 分支），因此建议
    分批 (--batch)；批与批之间 sleep(--pause)。
  - 失败于账号 parse 阶段时，不改 POOL；失败于 create 后是单条错误（其余继续）。
"""
import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

DEFAULT_BASE = os.environ.get("WB_BASE", "http://127.0.0.1:8788").rstrip("/")
DEFAULT_PASSWORD = os.environ.get("WB_PANEL_PW", "admin")

RED = "FAIL"
OK = "OK"


def call(method, path, data=None, token=None, base=None):
    base = (base or DEFAULT_BASE).rstrip("/")
    url = base + path
    body = None
    if data is not None:
        body = json.dumps(data).encode("utf-8")
    req = urllib.request.Request(url, data=body, method=method)
    req.add_header("Content-Type", "application/json")
    req.add_header("Accept", "application/json")
    if token:
        req.add_header("X-Panel-Token", token)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        try:
            body = json.loads(e.read() or b"{}")
        except Exception:
            body = {"error": "HTTP %d %s" % (e.code, e.reason)}
        return e.code, body
    except urllib.error.URLError as e:
        return 0, {"error": "network: %r" % e}
    except Exception as e:
        return 0, {"error": "unknown: %r" % e}


def mask(v):
    s = str(v or "")
    return s[:8] + ("…" if len(s) > 8 else "") if len(s) >= 12 else s[:6] + "…"


def load_document(path):
    with open(path, "r", encoding="utf-8") as f:
        obj = json.load(f)
    return obj


def slug_repo(rows):
    if isinstance(rows, list):
        return rows
    if isinstance(rows, dict):
        if "accounts" in rows and isinstance(rows["accounts"], list):
            return rows["accounts"]
        if all(k in rows for k in ("uid", "accessToken")):
            return [rows]
        if "account" in rows and isinstance(rows["account"], dict):
            return [rows]
    return None


def main():
    ap = argparse.ArgumentParser(
        description="Bulk-import accounts into the WorkBuddy proxy panel",
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--file", "-f", required=True, help="input JSON (exported doc, bare list, or single account)")
    ap.add_argument("--base", default=DEFAULT_BASE, help="panel base URL")
    ap.add_argument("--password", default=DEFAULT_PASSWORD, help="panel password")
    ap.add_argument("--realm", choices=("intl", "cn"), default=None,
                    help="force realm instead of auto-detect")
    ap.add_argument("--dry-run", action="store_true", help="validate only, no writes")
    ap.add_argument("--overwrite", action="store_true", help="replace existing accounts by uid")
    ap.add_argument("--batch", type=int, default=0, help="split into batches of N (0 = one call)")
    ap.add_argument("--pause", type=float, default=1.0, help="seconds between batches")
    ap.add_argument("--verbose", "-v", action="store_true", help="print per-account results")
    opts = ap.parse_args()

    base = opts.base.rstrip("/")

    # --- 1. load & coerce ---
    try:
        doc = load_document(opts.file)
    except FileNotFoundError:
        print("FAIL: input file not found: %s" % opts.file, file=sys.stderr)
        return 2
    except (json.JSONDecodeError, OSError) as e:
        print("FAIL: cannot read JSON: %r" % e, file=sys.stderr)
        return 2

    rows = slug_repo(doc)
    if rows is None:
        print("FAIL: cannot coerce document into account rows (need list or {'accounts':[...]})",
              file=sys.stderr)
        return 2
    print("loaded %d candidate rows from %s" % (len(rows), opts.file))

    # --- 2. panel login ---
    status, lr = call("POST", "/panel/login", {"password": opts.password}, base=base)
    panel_token = lr.get("token") if isinstance(lr, dict) else None
    if not panel_token or status != 200:
        print("FAIL: panel login failed (%s): %s" % (status, lr), file=sys.stderr)
        return 3
    print("panel login OK, token held (not printed)")

    # --- 3. import in batches ---
    results = {"added": 0, "updated": 0, "skipped": 0, "invalid": 0, "details": []}
    n = max(1, opts.batch or len(rows))
    batches = [rows[i:i + n] for i in range(0, len(rows), n)]
    print("submitting %d batches (dry_run=%s, overwrite=%s, forced_realm=%s)"
          % (len(batches), opts.dry_run, opts.overwrite, opts.realm))

    for bi, batch in enumerate(batches, 1):
        payload = {"data": {"format": "workbuddy-accounts", "accounts": batch},
                    "dryRun": opts.dry_run,
                    "overwrite": opts.overwrite}
        if opts.realm:
            payload["realm"] = opts.realm
        status, resp = call("POST", "/accounts/import", payload, token=panel_token, base=base)
        if status != 200:
            print("  batch %d/%d FAILED (%s): %s" % (bi, len(batches), status, resp),
                  file=sys.stderr)
            results["invalid"] += len(batch)
            results["details"].append({"batch": bi, "error": resp})
            continue
        r = resp.get("result", {}) if isinstance(resp, dict) else {}
        for k in ("added", "updated", "skipped", "invalid"):
            results[k] += len(r.get(k, [])) if isinstance(r.get(k), list) else int(r.get(k, 0))
        results["details"].append({"batch": bi, "count": len(batch), "result": r})
        if opts.verbose:
            for uid in r.get("added", []):
                print("    + added   %s" % mask(uid))
            for uid in r.get("updated", []):
                print("    ~ updated %s" % mask(uid))
            for item in r.get("skipped", []):
                print("    - skipped %s: %s" % (mask(item.get("uid", "?")), item.get("reason")))
            for item in r.get("invalid", []):
                print("    ! invalid  %s: %s" % (mask(item.get("uid", "?")), item.get("reason")))
        else:
            print("  batch %d/%d: %s" % (bi, len(batches), r if not opts.dry_run else resp.get("result")))
        if bi < len(batches):
            time.sleep(opts.pause)

    # --- 4. report ---
    print("\nTOTAL: %d rows across %d batch(es)" % (len(rows), len(batches)))
    print("  added   = %d" % results["added"])
    print("  updated = %d" % results["updated"])
    print("  skipped = %d" % results["skipped"])
    print("  invalid = %d" % results["invalid"])
    if opts.dry_run:
        print("\nDRY RUN only — no writes performed. Re-run without --dry-run to commit.")
    # panel hygiene: print total account count, not secrets
    s, ab = call("GET", "/accounts", token=panel_token, base=base)
    if s == 200:
        realms = {}
        for a in (ab.get("accounts") or []):
            realms[a.get("realm")] = realms.get(a.get("realm"), 0) + 1
        print("post-import account counts by realm: %s" % realms)
    if results["invalid"] and not opts.dry_run:
        print("WARN: %d rows failed validation — inspect details above" % results["invalid"],
              file=sys.stderr)
        return 4
    return 0


if __name__ == "__main__":
    sys.exit(main())
