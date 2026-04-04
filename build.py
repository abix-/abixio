"""build abixio (server) and abixio-ui release binaries for windows."""

import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
UI_DIR = ROOT.parent / "abixio-ui"


def find_target_dir():
    """resolve cargo target dir from global config or default."""
    global_config = Path.home() / ".cargo" / "config.toml"
    if global_config.exists():
        for line in global_config.read_text().splitlines():
            line = line.strip()
            if line.startswith("target-dir"):
                val = line.split("=", 1)[1].strip().strip('"').strip("'")
                p = Path(val)
                if p.exists():
                    return p
    return ROOT / "target"


def run(args, cwd=None):
    print(f"  > {' '.join(str(a) for a in args)}")
    result = subprocess.run(args, cwd=cwd)
    if result.returncode != 0:
        print(f"FAILED (exit {result.returncode})")
        sys.exit(1)


def main():
    target = find_target_dir()
    print(f"target dir: {target}")

    print("building abixio (server)...")
    run(["cargo", "build", "--release"], cwd=ROOT)

    print("building abixio-ui...")
    run(["cargo", "build", "--release"], cwd=UI_DIR)

    server_src = target / "release" / "abixio.exe"
    ui_src = target / "release" / "abixio-ui.exe"
    server_dst = ROOT / "abixio.exe"
    ui_dst = ROOT / "abixio-ui.exe"

    shutil.copy2(server_src, server_dst)
    shutil.copy2(ui_src, ui_dst)

    print(f"\ndone:")
    for p in [server_dst, ui_dst]:
        size_mb = p.stat().st_size / (1024 * 1024)
        print(f"  {p.name}  {size_mb:.1f} MB")


if __name__ == "__main__":
    main()
