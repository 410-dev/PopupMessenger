#!/usr/bin/env python3
"""Bumps relay/VERSION ("<protocol>.<YYMMDD>.<NN>") for a new server build.

- protocol: `PROTOCOL` in src/lib.rs
- YYMMDD:   today's local date
- NN:       01 for the first build of a day (or after a protocol change), then +1

Run it before building a server release:  python relay/bump-version.py
"""
import datetime
import pathlib
import re

root = pathlib.Path(__file__).resolve().parent
proto = re.search(r"pub const PROTOCOL: u64 = (\d+);", (root / "src" / "lib.rs").read_text(encoding="utf-8")).group(1)
today = datetime.date.today().strftime("%y%m%d")
path = root / "VERSION"
old = path.read_text(encoding="utf-8").strip() if path.exists() else ""
m = re.fullmatch(r"(\d+)\.(\d{6})\.(\d+)", old)
n = int(m.group(3)) + 1 if m and m.group(1) == proto and m.group(2) == today else 1
new = f"{proto}.{today}.{n:02d}"
path.write_text(new + "\n", encoding="utf-8", newline="\n")
print(f"{old or '(none)'} -> {new}")
