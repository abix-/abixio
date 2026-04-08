#!/usr/bin/env python3
"""4KB small object benchmark with persistent connections (keep-alive).

Measures actual server throughput, not client/process overhead.
Uses requests.Session for HTTP keep-alive (single TCP connection reused).
Reports objects/sec (not MB/s -- meaningless for 4KB).

Usage:
    # start servers first, then:
    python tests/bench_4kb.py --port 11000 --name AbixIO
    python tests/bench_4kb.py --port 11001 --name RustFS
    python tests/bench_4kb.py --port 11003 --name MinIO

    # or run all three:
    python tests/bench_4kb.py --all
"""

import argparse
import time
import requests
import subprocess
import os
import sys
import signal
import tempfile
import shutil

WARMUP = 50
OPS = 1000
DATA = b"x" * 4096


def bench_server(name, port, session=None):
    """Benchmark a single server on the given port."""
    s = session or requests.Session()
    base = f"http://127.0.0.1:{port}/bench4kb"

    # create bucket (ignore errors -- may already exist)
    s.put(base)

    # warmup
    for i in range(WARMUP):
        s.put(f"{base}/w{i}", data=DATA, headers={"Content-Length": "4096"})

    # PUT
    start = time.perf_counter()
    for i in range(OPS):
        r = s.put(f"{base}/p{i}", data=DATA, headers={"Content-Length": "4096"})
    put_elapsed = time.perf_counter() - start
    put_ops = OPS / put_elapsed

    # GET
    start = time.perf_counter()
    for i in range(OPS):
        r = s.get(f"{base}/p{i}")
        _ = r.content
    get_elapsed = time.perf_counter() - start
    get_ops = OPS / get_elapsed

    # HEAD
    start = time.perf_counter()
    for i in range(OPS):
        s.head(f"{base}/p{i}")
    head_elapsed = time.perf_counter() - start
    head_ops = OPS / head_elapsed

    # DELETE
    start = time.perf_counter()
    for i in range(OPS):
        s.delete(f"{base}/p{i}")
    del_elapsed = time.perf_counter() - start
    del_ops = OPS / del_elapsed

    put_ms = put_elapsed / OPS * 1000
    get_ms = get_elapsed / OPS * 1000
    head_ms = head_elapsed / OPS * 1000
    del_ms = del_elapsed / OPS * 1000

    put_mbps = put_ops * 4096 / 1024 / 1024
    get_mbps = get_ops * 4096 / 1024 / 1024

    print(f"| {name:<12} "
          f"| {put_ops:>6.0f} obj/s  {put_mbps:>5.1f} MB/s  {put_ms:.2f}ms "
          f"| {get_ops:>6.0f} obj/s  {get_mbps:>5.1f} MB/s  {get_ms:.2f}ms "
          f"| {head_ops:>6.0f} obj/s  {head_ms:.2f}ms "
          f"| {del_ops:>6.0f} obj/s  {del_ms:.2f}ms |")

    return {"put": put_ops, "get": get_ops, "head": head_ops, "delete": del_ops}


def start_server(binary, args, env=None):
    """Start a server process, return (proc, tmpdir)."""
    tmpdir = tempfile.mkdtemp()
    full_env = os.environ.copy()
    if env:
        full_env.update(env)
    proc = subprocess.Popen(
        [binary] + args,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=full_env,
    )
    time.sleep(1.5)
    return proc, tmpdir


def main():
    parser = argparse.ArgumentParser(description="4KB benchmark with keep-alive")
    parser.add_argument("--port", type=int, help="server port")
    parser.add_argument("--name", default="server", help="server name")
    parser.add_argument("--all", action="store_true", help="benchmark all three servers")
    parser.add_argument("--ops", type=int, default=1000, help="operations per test")
    args = parser.parse_args()

    global OPS
    OPS = args.ops

    print(f"4KB benchmark: {OPS} operations per test, persistent connection (keep-alive)")
    print(f"| {'Server':<12} | {'PUT':^34} | {'GET':^34} | {'HEAD':^18} | {'DELETE':^18} |")
    print(f"|{'-'*14}|{'-'*36}|{'-'*36}|{'-'*20}|{'-'*20}|")

    if args.all:
        abixio_bin = os.environ.get("ABIXIO_BIN", os.path.join(
            os.path.dirname(__file__), "..", "target", "release", "abixio.exe"
        ))
        # try common locations
        for p in [abixio_bin, r"C:\code\endless\rust\target\release\abixio.exe"]:
            if os.path.exists(p):
                abixio_bin = p
                break

        rustfs_bin = os.environ.get("RUSTFS_BIN", r"C:\tools\rustfs.exe")
        minio_bin = os.environ.get("MINIO_BIN", r"C:\tools\minio.exe")

        procs = []
        try:
            # AbixIO with log store
            tmpdir_a = tempfile.mkdtemp()
            os.makedirs(os.path.join(tmpdir_a, ".abixio.sys", "log"), exist_ok=True)
            if os.path.exists(abixio_bin):
                proc_a = subprocess.Popen(
                    [abixio_bin, "--volumes", tmpdir_a, "--listen", ":11000", "--no-auth"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
                procs.append((proc_a, tmpdir_a))
                time.sleep(1)

            # AbixIO file tier (no log store)
            tmpdir_af = tempfile.mkdtemp()
            if os.path.exists(abixio_bin):
                proc_af = subprocess.Popen(
                    [abixio_bin, "--volumes", tmpdir_af, "--listen", ":11010", "--no-auth"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
                procs.append((proc_af, tmpdir_af))
                time.sleep(1)

            # RustFS
            tmpdir_r = tempfile.mkdtemp()
            if os.path.exists(rustfs_bin):
                proc_r = subprocess.Popen(
                    [rustfs_bin, "server", tmpdir_r,
                     "--address", ":11001", "--console-address", ":11002"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    env={**os.environ, "RUSTFS_ROOT_USER": "benchuser",
                         "RUSTFS_ROOT_PASSWORD": "benchpass"},
                )
                procs.append((proc_r, tmpdir_r))
                time.sleep(1)

            # MinIO
            tmpdir_m = tempfile.mkdtemp()
            if os.path.exists(minio_bin):
                proc_m = subprocess.Popen(
                    [minio_bin, "server", tmpdir_m,
                     "--address", ":11003", "--console-address", ":11004"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    env={**os.environ, "MINIO_ROOT_USER": "benchuser",
                         "MINIO_ROOT_PASSWORD": "benchpass"},
                )
                procs.append((proc_m, tmpdir_m))
                time.sleep(1)

            if os.path.exists(abixio_bin):
                bench_server("AbixIO(log)", 11000)
                bench_server("AbixIO(file)", 11010)
            if os.path.exists(rustfs_bin):
                bench_server("RustFS", 11001)
            if os.path.exists(minio_bin):
                bench_server("MinIO", 11003)

        finally:
            for proc, tmpdir in procs:
                proc.kill()
                proc.wait()
                shutil.rmtree(tmpdir, ignore_errors=True)
    else:
        if not args.port:
            print("error: --port required (or use --all)")
            sys.exit(1)
        bench_server(args.name, args.port)


if __name__ == "__main__":
    main()
