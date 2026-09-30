"""Summarise a Time Profiler trace: main-thread samples by self (leaf) frame
and by inclusive frame. Usage: python3 tracesummary.py TRACE [TOP]"""
import subprocess
import sys
import xml.etree.ElementTree as ET
from collections import Counter

trace = sys.argv[1]
top = int(sys.argv[2]) if len(sys.argv) > 2 else 25
xml = subprocess.run(
    ["xctrace", "export", "--input", trace, "--xpath",
     '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]'],
    capture_output=True, check=True).stdout
root = ET.fromstring(xml)

# Elements appear once with id="" and are referenced afterwards with ref="".
ids = {e.attrib["id"]: e for e in root.iter() if "id" in e.attrib}


def resolve(e):
    if e is None:
        return None
    return ids[e.attrib["ref"]] if "ref" in e.attrib else e


self_c, incl_c = Counter(), Counter()
total = 0
for row in root.iter("row"):
    thread = resolve(row.find("thread"))
    bt = resolve(row.find("tagged-backtrace") if row.find("tagged-backtrace") is not None else row.find("backtrace"))
    weight = resolve(row.find("weight"))
    if thread is None or bt is None:
        continue
    fmt = thread.attrib.get("fmt", "")
    if "Main Thread" not in fmt and "main" not in fmt.lower():
        continue
    w = int(weight.text) if weight is not None and weight.text else 1
    frames = [resolve(f) for f in bt.findall("frame")]
    names = [f.attrib.get("name", "?") for f in frames if f is not None]
    if not names:
        continue
    total += w
    self_c[names[0]] += w
    for n in set(names):
        incl_c[n] += w

print(f"main-thread samples (weight): {total}")
print("\n## self")
for n, w in self_c.most_common(top):
    print(f"{100 * w / total:5.1f}%  {n[:110]}")
print("\n## inclusive")
for n, w in incl_c.most_common(top + 25):
    print(f"{100 * w / total:5.1f}%  {n[:110]}")
