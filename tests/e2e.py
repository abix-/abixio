#!/usr/bin/env python3
"""
End-to-end test for AbixIO server + admin API.

Usage:
    python tests/e2e.py [--endpoint http://localhost:10000]

If no endpoint given, starts a server on a temp directory automatically.
Requires: requests (pip install requests)
"""

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time

try:
    import requests
except ImportError:
    print("ERROR: pip install requests")
    sys.exit(1)


class TestRunner:
    def __init__(self):
        self.passed = 0
        self.failed = 0
        self.errors = []

    def check(self, name, condition, detail=""):
        if condition:
            self.passed += 1
            print(f"  PASS  {name}")
        else:
            self.failed += 1
            self.errors.append(name)
            msg = f"  FAIL  {name}"
            if detail:
                msg += f" -- {detail}"
            print(msg)

    def summary(self):
        total = self.passed + self.failed
        print(f"\n{'='*60}")
        print(f"  {self.passed}/{total} passed, {self.failed} failed")
        if self.errors:
            print(f"  failures:")
            for e in self.errors:
                print(f"    - {e}")
        print(f"{'='*60}")
        return self.failed == 0


def wait_for_server(endpoint, timeout=10):
    """Wait until server responds."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            r = requests.get(f"{endpoint}/", timeout=2)
            return True
        except Exception:
            time.sleep(0.2)
    return False


def run_tests(endpoint, disk_paths=None):
    t = TestRunner()
    print(f"\nTesting against {endpoint}\n")

    # =========================================================
    # S3 API tests
    # =========================================================
    print("--- S3 API ---")

    # create bucket
    r = requests.put(f"{endpoint}/e2e-test")
    t.check("create bucket", r.status_code == 200, f"got {r.status_code}")

    # create bucket again -> 409
    r = requests.put(f"{endpoint}/e2e-test")
    t.check("create bucket duplicate -> 409", r.status_code == 409, f"got {r.status_code}")

    # head bucket
    r = requests.head(f"{endpoint}/e2e-test")
    t.check("head bucket exists", r.status_code == 200)

    r = requests.head(f"{endpoint}/nonexistent-bucket-xyz")
    t.check("head bucket missing -> 404", r.status_code == 404)

    # list buckets
    r = requests.get(f"{endpoint}/")
    t.check("list buckets", r.status_code == 200 and "e2e-test" in r.text)

    # put object
    r = requests.put(f"{endpoint}/e2e-test/hello.txt",
                     data="hello world",
                     headers={"Content-Type": "text/plain"})
    t.check("put object", r.status_code == 200)
    t.check("put object has etag", "etag" in r.headers)

    # put more objects
    requests.put(f"{endpoint}/e2e-test/docs/readme.txt", data="readme content")
    requests.put(f"{endpoint}/e2e-test/docs/guide.txt", data="guide content")
    requests.put(f"{endpoint}/e2e-test/photos/cat.jpg", data="fake image data")

    # get object
    r = requests.get(f"{endpoint}/e2e-test/hello.txt")
    t.check("get object", r.status_code == 200 and r.text == "hello world")
    t.check("get object content-type", r.headers.get("content-type") == "text/plain")
    t.check("get object has etag", "etag" in r.headers)
    t.check("get object has last-modified", "last-modified" in r.headers)

    # head object
    r = requests.head(f"{endpoint}/e2e-test/hello.txt")
    t.check("head object", r.status_code == 200)
    t.check("head object content-length", r.headers.get("content-length") == "11")

    # get nonexistent object
    r = requests.get(f"{endpoint}/e2e-test/nonexistent")
    t.check("get missing object -> 404", r.status_code == 404)

    # put empty body
    r = requests.put(f"{endpoint}/e2e-test/empty", data="")
    t.check("put empty object", r.status_code == 200)
    r = requests.get(f"{endpoint}/e2e-test/empty")
    t.check("get empty object", r.status_code == 200 and r.headers.get("content-length") == "0")

    # list objects
    r = requests.get(f"{endpoint}/e2e-test?list-type=2")
    t.check("list objects", r.status_code == 200 and "hello.txt" in r.text)

    # list with prefix
    r = requests.get(f"{endpoint}/e2e-test?list-type=2&prefix=docs/")
    t.check("list objects prefix", r.status_code == 200 and "readme.txt" in r.text and "cat.jpg" not in r.text)

    # list with delimiter
    r = requests.get(f"{endpoint}/e2e-test?list-type=2&delimiter=/")
    t.check("list objects delimiter", r.status_code == 200 and "CommonPrefixes" in r.text)

    # delete object
    r = requests.delete(f"{endpoint}/e2e-test/hello.txt")
    t.check("delete object", r.status_code == 204)
    r = requests.get(f"{endpoint}/e2e-test/hello.txt")
    t.check("get after delete -> 404", r.status_code == 404)

    # delete nonexistent
    r = requests.delete(f"{endpoint}/e2e-test/nonexistent")
    t.check("delete missing -> 404", r.status_code == 404)

    # =========================================================
    # Admin API tests
    # =========================================================
    print("\n--- Admin API ---")

    # status
    r = requests.get(f"{endpoint}/_admin/status")
    t.check("admin status 200", r.status_code == 200)
    if r.status_code == 200:
        data = r.json()
        t.check("admin status server=abixio", data.get("server") == "abixio")
        t.check("admin status has version", "version" in data)
        t.check("admin status has uptime", "uptime_secs" in data)
        t.check("admin status data_shards=2", data.get("data_shards") == 2)
        t.check("admin status parity_shards=2", data.get("parity_shards") == 2)
        t.check("admin status total_disks=4", data.get("total_disks") == 4)

    # disks
    r = requests.get(f"{endpoint}/_admin/disks")
    t.check("admin disks 200", r.status_code == 200)
    if r.status_code == 200:
        data = r.json()
        disks = data.get("disks", [])
        t.check("admin disks count=4", len(disks) == 4)
        t.check("admin disks all online", all(d["online"] for d in disks))
        t.check("admin disks have space info", all(d["total_bytes"] > 0 for d in disks))
        t.check("admin disks have bucket count", all("bucket_count" in d for d in disks))
        t.check("admin disks have object count", all("object_count" in d for d in disks))

    # heal status
    r = requests.get(f"{endpoint}/_admin/heal")
    t.check("admin heal status 200", r.status_code == 200)
    if r.status_code == 200:
        data = r.json()
        t.check("admin heal mrf_pending >= 0", data.get("mrf_pending", -1) >= 0)
        t.check("admin heal has scanner", "scanner" in data)
        t.check("admin heal scanner has intervals", "scan_interval" in data.get("scanner", {}))

    # unknown admin endpoint
    r = requests.get(f"{endpoint}/_admin/nonexistent")
    t.check("admin unknown -> 404", r.status_code == 404)

    # =========================================================
    # Object inspection
    # =========================================================
    print("\n--- Object Inspection ---")

    # re-upload for inspection
    requests.put(f"{endpoint}/e2e-test/inspect-me.txt", data="inspect this data")

    r = requests.get(f"{endpoint}/_admin/object?bucket=e2e-test&key=inspect-me.txt")
    t.check("admin inspect 200", r.status_code == 200)
    if r.status_code == 200:
        data = r.json()
        t.check("inspect bucket", data.get("bucket") == "e2e-test")
        t.check("inspect key", data.get("key") == "inspect-me.txt")
        t.check("inspect size", data.get("size") == 16)
        t.check("inspect has etag", "etag" in data)
        t.check("inspect erasure data=2", data.get("erasure", {}).get("data") == 2)
        t.check("inspect erasure parity=2", data.get("erasure", {}).get("parity") == 2)
        shards = data.get("shards", [])
        t.check("inspect 4 shards", len(shards) == 4)
        t.check("inspect all shards ok", all(s["status"] == "ok" for s in shards))
        t.check("inspect all have checksums", all(s.get("checksum") for s in shards))

    # inspect missing params
    r = requests.get(f"{endpoint}/_admin/object")
    t.check("inspect no params -> 400", r.status_code == 400)

    r = requests.get(f"{endpoint}/_admin/object?bucket=e2e-test")
    t.check("inspect no key -> 400", r.status_code == 400)

    # inspect nonexistent object
    r = requests.get(f"{endpoint}/_admin/object?bucket=e2e-test&key=nope")
    t.check("inspect missing -> 404", r.status_code == 404)

    # =========================================================
    # Erasure resilience + heal
    # =========================================================
    print("\n--- Erasure Resilience ---")

    if disk_paths:
        requests.put(f"{endpoint}/e2e-test/resilience.txt", data="survive disk failure")

        # inspect to get distribution
        r = requests.get(f"{endpoint}/_admin/object?bucket=e2e-test&key=resilience.txt")
        data = r.json()
        shards = data["shards"]

        # delete shards from 2 disks
        disk0 = shards[0]["disk"]
        disk1 = shards[1]["disk"]
        shard_dir0 = os.path.join(disk_paths[disk0], "e2e-test", "resilience.txt")
        shard_dir1 = os.path.join(disk_paths[disk1], "e2e-test", "resilience.txt")
        if os.path.exists(shard_dir0):
            shutil.rmtree(shard_dir0)
        if os.path.exists(shard_dir1):
            shutil.rmtree(shard_dir1)

        # data still readable
        r = requests.get(f"{endpoint}/e2e-test/resilience.txt")
        t.check("resilience: read after 2 disk loss", r.status_code == 200 and r.text == "survive disk failure")

        # inspect shows missing
        r = requests.get(f"{endpoint}/_admin/object?bucket=e2e-test&key=resilience.txt")
        data = r.json()
        missing = sum(1 for s in data["shards"] if s["status"] == "missing")
        t.check("resilience: missing shards detected", missing >= 1, f"missing={missing}")

        # heal
        r = requests.post(f"{endpoint}/_admin/heal?bucket=e2e-test&key=resilience.txt")
        t.check("resilience: heal 200", r.status_code == 200)
        if r.status_code == 200:
            data = r.json()
            t.check("resilience: heal result=repaired", data.get("result") == "repaired")
            t.check("resilience: shards_fixed >= 1", (data.get("shards_fixed") or 0) >= 1)

        # verify all shards restored
        r = requests.get(f"{endpoint}/_admin/object?bucket=e2e-test&key=resilience.txt")
        data = r.json()
        all_ok = all(s["status"] == "ok" for s in data["shards"])
        t.check("resilience: all shards ok after heal", all_ok)

        # heal already healthy
        r = requests.post(f"{endpoint}/_admin/heal?bucket=e2e-test&key=resilience.txt")
        data = r.json()
        t.check("resilience: re-heal returns healthy", data.get("result") == "healthy")
    else:
        print("  SKIP  erasure resilience (no disk paths available)")

    # =========================================================
    # Summary
    # =========================================================
    return t.summary()


def main():
    parser = argparse.ArgumentParser(description="AbixIO end-to-end tests")
    parser.add_argument("--endpoint", default=None, help="Server endpoint (default: start local server)")
    parser.add_argument("--binary", default=None, help="Path to abixio binary")
    args = parser.parse_args()

    if args.endpoint:
        print(f"Using existing server at {args.endpoint}")
        ok = run_tests(args.endpoint)
        sys.exit(0 if ok else 1)

    # start local server
    binary = args.binary
    if not binary:
        # try common locations
        for candidate in [
            "target/release/abixio",
            "target/release/abixio.exe",
            "target/debug/abixio",
            "target/debug/abixio.exe",
            os.path.expandvars(r"%USERPROFILE%\.cargo\bin\abixio.exe"),
        ]:
            if os.path.exists(candidate):
                binary = candidate
                break

    if not binary:
        print("ERROR: abixio binary not found. Build with 'cargo build --release' or pass --binary")
        sys.exit(1)

    # create temp disks
    tmpdir = tempfile.mkdtemp(prefix="abixio-e2e-")
    disk_paths = []
    for i in range(4):
        p = os.path.join(tmpdir, f"d{i}")
        os.makedirs(p)
        disk_paths.append(p)

    disks_arg = ",".join(disk_paths)
    port = 19876  # unlikely to conflict
    endpoint = f"http://127.0.0.1:{port}"

    print(f"Starting server: {binary}")
    print(f"  disks: {disks_arg}")
    print(f"  endpoint: {endpoint}")

    proc = subprocess.Popen(
        [binary, "--listen", f"0.0.0.0:{port}",
         "--disks", disks_arg,
         "--data", "2", "--parity", "2", "--no-auth"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    try:
        if not wait_for_server(endpoint):
            print("ERROR: server did not start within 10 seconds")
            proc.kill()
            sys.exit(1)

        ok = run_tests(endpoint, disk_paths)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(tmpdir, ignore_errors=True)

    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
