#!/usr/bin/env python3
"""一次性转换脚本：ESP-Handheld iching_data.c 的 64 卦数据 → src/divination/iching_data.rs。
仅开发期使用，产物已提交仓库，不需要保留在构建流程中。"""
import json
import re

SRC = "/Users/chaos/Desktop/github/ESP-Handheld/main/modules/iching/iching_data.c"
OUT = "/Users/chaos/Desktop/apps/ruozhi/src/divination/iching_data.rs"
HEADER = "/Users/chaos/Desktop/apps/ruozhi/src/divination/iching_data_header.txt"

text = open(SRC, encoding="utf-8").read()

m = re.search(r"g_iching\[64\]\s*=\s*\{(.*?)\n\};", text, re.S)
body = m.group(1)

records = []
depth = 0
cur = ""
for ch in body:
    if ch == "{":
        depth += 1
        if depth == 1:
            cur = ""
            continue
    if ch == "}":
        depth -= 1
        if depth == 0:
            records.append(cur)
            continue
    if depth >= 1:
        cur += ch

assert len(records) == 64, f"expect 64 records, got {len(records)}"

FIELDS = ["name", "gua_ci", "xiang", "daxiang", "yushi", "shiye",
          "jingshang", "qiuming", "hunlian", "juece"]


def split_fields(rec: str):
    out = []
    i = 0
    n = len(rec)
    while len(out) < 10 and i < n:
        if rec[i] == '"':
            j = i + 1
            buf = []
            while j < n:
                if rec[j] == '\\' and j + 1 < n and rec[j + 1] == '"':
                    buf.append('"')
                    j += 2
                elif rec[j] == '"':
                    break
                else:
                    buf.append(rec[j])
                    j += 1
            out.append("".join(buf))
            i = j + 1
        else:
            i += 1
    assert len(out) == 10, f"expect 10 fields, got {len(out)}: {rec[:60]}"
    return out


def rust_str(s: str) -> str:
    return json.dumps(s, ensure_ascii=False)


lines = ["pub const ICHING: [Hexagram; 64] = ["]
for rec in records:
    f = split_fields(rec)
    if f[2].startswith(("：", ":")):
        f[2] = f[2][1:]
    lines.append("    Hexagram {")
    lines.append(f"        name: {rust_str(f[0])},")
    for key, val in zip(FIELDS[1:], f[1:]):
        lines.append(f"        {key}: {rust_str(val)},")
    lines.append("    },")
lines.append("];")
lines.append("")
# 与固件 binary_to_index[64] 逐字一致：伏羲二进制序(上卦<<3|下卦) → 文王卦序下标
lines.append("pub const binary_to_index: [u8; 64] = [")
lines.append("    1, 14, 6, 45, 23, 35, 18, 10,")
lines.append("    22, 51, 3, 17, 26, 21, 40, 25,")
lines.append("    7, 38, 28, 47, 2, 62, 59, 4,")
lines.append("    19, 52, 58, 56, 41, 36, 60, 8,")
lines.append("    15, 61, 39, 31, 50, 54, 53, 33,")
lines.append("    34, 55, 63, 49, 20, 29, 37, 13,")
lines.append("    44, 30, 46, 27, 16, 48, 57, 42,")
lines.append("    11, 32, 5, 43, 24, 12, 9, 0,")
lines.append("];")

header = open(HEADER, encoding="utf-8").read()
with open(OUT, "w", encoding="utf-8") as fh:
    fh.write(header + "\n" + "\n".join(lines) + "\n")

print("wrote", OUT, "records:", len(records))
