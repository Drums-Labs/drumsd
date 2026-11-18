"""drums_t6: the lab verifier for the Drums task factory (install as root: /etc/drums/verifiers/drums_t6.py).

Two functions, called by drumsd's checker helper on the host, outside the desktop (docs/custom-tasks.md):

    check(ctx, kind, **options)       the primary checker
    check_alt(ctx, kind, **options)   an independent second checker for the same options: a different read path
                                      (the desktop's own tools through ctx.run instead of ctx.read_file) and/or a
                                      different parser (raw OOXML instead of openpyxl/python-docx/python-pptx,
                                      PyMuPDF instead of pypdf/Pillow). drumsd's reward audit compares the two.

Both return {"score": 0.0 or 1.0, "info": {...}}. A state the agent can reach never raises: a missing, unreadable or
malformed file scores 0 with the reason in info. Only a broken desktop (the guest server not answering) raises.
Uses the standard library and the checker venv's packages (openpyxl, python-docx, python-pptx, Pillow, PyMuPDF, pypdf).
"""
import base64
import csv
import fnmatch
import hashlib
import io
import json
import math
import re
import zipfile

VERSION = 1


def _ok(score, **info):
    return {"score": 1.0 if score else 0.0, "info": info}


def _sha(b):
    return hashlib.sha256(b).hexdigest()


def _read(ctx, path):
    return ctx.read_file(path)


def _read_alt(ctx, path):
    """The desktop's own `cat` through the guest's execute route (a different route from /file)."""
    r = ctx.run(["python3", "-I", "-c",
                 "import base64,sys\ntry:\n d=open(sys.argv[1],'rb').read()\nexcept FileNotFoundError:\n print('T6-ABSENT');sys.exit(0)\n"
                 "except IsADirectoryError:\n print('T6-DIR');sys.exit(0)\nprint(base64.b64encode(d).decode())", path], timeout=60)
    if r.get("returncode") != 0:
        raise RuntimeError(f"read through execute failed: rc {r.get('returncode')}: {(r.get('error') or '')[:200]}")
    out = (r.get("output") or "").strip()
    if out in ("T6-ABSENT", "T6-DIR"):
        return None
    return base64.b64decode(out)


def _num(x):
    if isinstance(x, bool):
        return None
    if isinstance(x, (int, float)):
        return float(x)
    if isinstance(x, str):
        s = x.strip().replace(",", "")
        try:
            return float(s)
        except ValueError:
            return None
    return None


def _same(a, b, tol):
    """Cell / field equality: numbers within tol (absolute), else stripped strings (None == empty)."""
    na, nb = _num(a), _num(b)
    if na is not None and nb is not None:
        return abs(na - nb) <= tol
    sa = "" if a is None else str(a).strip()
    sb = "" if b is None else str(b).strip()
    return sa == sb


def _trim(rows):
    rows = [[("" if v is None else v) for v in r] for r in rows]
    rows = [r for r in rows]
    while rows and all(str(v).strip() == "" for v in rows[-1]):
        rows.pop()
    width = 0
    for r in rows:
        for i, v in enumerate(r):
            if str(v).strip() != "":
                width = max(width, i + 1)
    return [list(r[:width]) + [""] * (width - len(r[:width])) for r in rows]


def _table_eq(got, want, tol):
    got, want = _trim(got), _trim(want)
    if len(got) != len(want):
        return False, f"{len(got)} rows, want {len(want)}"
    for i, (g, w) in enumerate(zip(got, want)):
        if len(g) != len(w):
            return False, f"row {i + 1}: {len(g)} columns, want {len(w)}"
        for j, (a, b) in enumerate(zip(g, w)):
            if not _same(a, b, tol):
                return False, f"row {i + 1} col {j + 1}: {a!r}, want {b!r}"
    return True, "equal"


# ------------------------------------------------------------------------------------------------ OOXML readers
# Small independent parsers for the redundant checker (no openpyxl / python-docx / python-pptx).

_NS = {"m": "http://schemas.openxmlformats.org/spreadsheetml/2006/main",
       "r": "http://schemas.openxmlformats.org/officeDocument/2006/relationships",
       "rel": "http://schemas.openxmlformats.org/package/2006/relationships",
       "w": "http://schemas.openxmlformats.org/wordprocessingml/2006/main",
       "p": "http://schemas.openxmlformats.org/presentationml/2006/main",
       "a": "http://schemas.openxmlformats.org/drawingml/2006/main"}


def _et():
    import xml.etree.ElementTree as ET
    return ET


def _col_index(letters):
    n = 0
    for ch in letters:
        n = n * 26 + (ord(ch.upper()) - 64)
    return n


def _ref(ref):
    m = re.fullmatch(r"([A-Za-z]+)(\d+)", ref)
    return int(m.group(2)), _col_index(m.group(1))


def _xlsx_raw(data):
    """-> {sheet name: {(row, col): value}} from the raw XML (shared strings, inline strings, numbers, booleans)."""
    ET = _et()
    z = zipfile.ZipFile(io.BytesIO(data))
    names = set(z.namelist())
    shared = []
    if "xl/sharedStrings.xml" in names:
        for si in ET.fromstring(z.read("xl/sharedStrings.xml")).findall("m:si", _NS):
            shared.append("".join(t.text or "" for t in si.iter(f"{{{_NS['m']}}}t")))
    wb = ET.fromstring(z.read("xl/workbook.xml"))
    rels = ET.fromstring(z.read("xl/_rels/workbook.xml.rels"))
    target = {r.get("Id"): r.get("Target") for r in rels.findall("rel:Relationship", _NS)}
    out = {}
    order = []
    for sh in wb.find("m:sheets", _NS).findall("m:sheet", _NS):
        rid = sh.get(f"{{{_NS['r']}}}id")
        t = target[rid].lstrip("/")
        path = t if t.startswith("xl/") else "xl/" + t
        cells = {}
        root = ET.fromstring(z.read(path))
        for c in root.iter(f"{{{_NS['m']}}}c"):
            rc = _ref(c.get("r"))
            typ = c.get("t")
            v = c.find("m:v", _NS)
            if typ == "s" and v is not None:
                cells[rc] = shared[int(v.text)]
            elif typ == "inlineStr":
                cells[rc] = "".join(x.text or "" for x in c.iter(f"{{{_NS['m']}}}t"))
            elif typ == "b" and v is not None:
                cells[rc] = bool(int(v.text))
            elif typ in ("str", "e") and v is not None:
                cells[rc] = v.text
            elif v is not None and v.text is not None:
                f = float(v.text)
                cells[rc] = int(f) if f.is_integer() else f
        out[sh.get("name")] = cells
        order.append(sh.get("name"))
    return out, order


def _raw_rows(cells):
    if not cells:
        return []
    nr = max(r for r, _ in cells)
    nc = max(c for _, c in cells)
    return [[cells.get((r, c)) for c in range(1, nc + 1)] for r in range(1, nr + 1)]


def _docx_raw_paragraphs(data):
    """-> list of (text, bold_text) for body paragraphs, from document.xml."""
    ET = _et()
    z = zipfile.ZipFile(io.BytesIO(data))
    root = ET.fromstring(z.read("word/document.xml"))
    w = _NS["w"]
    out = []
    for p in root.iter(f"{{{w}}}p"):
        text, bold = [], []
        for r in p.iter(f"{{{w}}}r"):
            t = "".join(x.text or "" for x in r.findall(f"{{{w}}}t"))
            text.append(t)
            rpr = r.find(f"{{{w}}}rPr")
            b = None if rpr is None else rpr.find(f"{{{w}}}b")
            if b is not None and b.get(f"{{{w}}}val") not in ("0", "false", "off"):
                bold.append(t)
        out.append(("".join(text), "".join(bold)))
    return out


def _pptx_raw_titles(data):
    ET = _et()
    z = zipfile.ZipFile(io.BytesIO(data))
    pres = ET.fromstring(z.read("ppt/presentation.xml"))
    rels = ET.fromstring(z.read("ppt/_rels/presentation.xml.rels"))
    target = {r.get("Id"): r.get("Target") for r in rels.findall("rel:Relationship", _NS)}
    titles = []
    lst = pres.find("p:sldIdLst", _NS)
    for sid in ([] if lst is None else lst.findall("p:sldId", _NS)):
        t = target[sid.get(f"{{{_NS['r']}}}id")]
        path = "ppt/" + t.lstrip("/").replace("../", "") if not t.startswith("ppt/") else t
        root = ET.fromstring(z.read(path))
        title, first = None, None
        for sp in root.iter(f"{{{_NS['p']}}}sp"):
            txt = "\n".join("".join(x.text or "" for x in para.iter(f"{{{_NS['a']}}}t"))
                            for para in sp.iter(f"{{{_NS['a']}}}p")).strip()
            ph = sp.find(".//p:nvPr/p:ph", _NS)
            if ph is not None and ph.get("type") in ("title", "ctrTitle") and title is None:
                title = txt
            if first is None and txt:
                first = txt
        titles.append(title if title is not None else (first or ""))
    return titles


# ------------------------------------------------------------------------------------------------ primary checkers

def _text(ctx, o, read):
    data = read(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing", path=o["path"])
    try:
        s = data.decode("utf-8")
    except UnicodeDecodeError:
        return _ok(False, reason="not utf-8")
    mode = o.get("mode", "exact_strip")
    want = o.get("expected")
    if mode == "exact_strip":
        return _ok(s.strip() == want.strip(), got=s.strip()[:200])
    if mode == "lines":
        got = [x.rstrip() for x in s.strip().splitlines()]
        return _ok(got == [x.rstrip() for x in want], got=got[:20])
    if mode == "lines_set":
        got = sorted(x.strip() for x in s.strip().splitlines() if x.strip())
        return _ok(got == sorted(want), got=got[:20])
    if mode == "number":
        n = _num(s.strip())
        return _ok(n is not None and abs(n - float(want)) <= float(o.get("tol", 1e-6)), got=s.strip()[:50])
    if mode == "contains_all":
        return _ok(all(x in s for x in want) and not any(x in s for x in o.get("absent", [])))
    raise ValueError(f"text mode {mode!r}")


def _files(ctx, o, read):
    """must_exist {path: sha256 or null}; must_absent [paths]; dirs [paths]; listing {dir: [names]} exact entries."""
    for p, h in (o.get("must_exist") or {}).items():
        data = read(ctx, p)
        if data is None:
            return _ok(False, reason="missing", path=p)
        if h is not None and _sha(data) != h:
            return _ok(False, reason="content differs", path=p)
    for p in o.get("must_absent") or []:
        if read(ctx, p) is not None:
            return _ok(False, reason="still present", path=p)
    for d in o.get("dirs") or []:
        r = ctx.run(["test", "-d", d])
        if r.get("returncode") != 0:
            return _ok(False, reason="not a directory", path=d)
    for d, names in (o.get("listing") or {}).items():
        r = ctx.run(["python3", "-I", "-c", "import json,os,sys\ntry:\n print(json.dumps(sorted(os.listdir(sys.argv[1]))))\n"
                     "except OSError:\n print('null')", d])
        got = json.loads((r.get("output") or "null").strip() or "null")
        if got is None or sorted(names) != got:
            return _ok(False, reason="listing differs", dir=d, got=(got or [])[:30])
    return _ok(True)


def _zip(ctx, o, read):
    data = read(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    try:
        z = zipfile.ZipFile(io.BytesIO(data))
        got = {}
        for i in z.infolist():
            if i.is_dir():
                continue
            name = i.filename.split("/")[-1] if o.get("basename_only", True) else i.filename
            got[name] = _sha(z.read(i))
    except (zipfile.BadZipFile, OSError, ValueError) as e:
        return _ok(False, reason=f"bad zip: {type(e).__name__}")
    return _ok(got == o["members"], got=sorted(got)[:30])


def _open_xlsx(data):
    import openpyxl
    return openpyxl.load_workbook(io.BytesIO(data), data_only=True)


def _xlsx_sheet(wb, sheet):
    if sheet is None:
        return wb.worksheets[0]
    if isinstance(sheet, int):
        return wb.worksheets[sheet]
    return wb[sheet] if sheet in wb.sheetnames else None


def _xlsx(ctx, o, read):
    data = read(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    try:
        wb = _open_xlsx(data)
    except Exception as e:  # noqa: BLE001 - any unreadable workbook is a 0
        return _ok(False, reason=f"unreadable: {type(e).__name__}")
    kind = o["kind"]
    tol = float(o.get("tol", 1e-6))
    if kind == "xlsx_sheetnames":
        return _ok(wb.sheetnames == o["names"], got=wb.sheetnames)
    ws = _xlsx_sheet(wb, o.get("sheet"))
    if ws is None:
        return _ok(False, reason="no such sheet", sheets=wb.sheetnames)
    if kind == "xlsx_cells":
        for ref, want in o["cells"].items():
            got = ws[ref].value
            if not _same(got, want, tol):
                return _ok(False, reason="cell differs", cell=ref, got=str(got)[:50])
        return _ok(True)
    if kind == "xlsx_table":
        rows = [list(r) for r in ws.iter_rows(values_only=True)]
        eq, why = _table_eq(rows, o["rows"], tol)
        return _ok(eq, why=why)
    raise ValueError(kind)


def _csv(ctx, o, read):
    data = read(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    try:
        rows = list(csv.reader(io.StringIO(data.decode("utf-8-sig"))))
    except (UnicodeDecodeError, csv.Error) as e:
        return _ok(False, reason=f"unreadable: {type(e).__name__}")
    eq, why = _table_eq(rows, o["rows"], float(o.get("tol", 1e-6)))
    return _ok(eq, why=why)


def _docx_paras(data):
    import docx
    d = docx.Document(io.BytesIO(data))
    return [(p.text, "".join(r.text for r in p.runs if r.bold)) for p in d.paragraphs]


def _docx(ctx, o, read, paras_fn):
    data = read(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    try:
        paras = paras_fn(data)
    except Exception as e:  # noqa: BLE001
        return _ok(False, reason=f"unreadable: {type(e).__name__}")
    texts = [t.strip() for t, _ in paras if t.strip()]
    if texts != [t.strip() for t in o["paragraphs"]]:
        return _ok(False, reason="paragraphs differ", got=texts[:12])
    if o["kind"] == "docx_bold":
        bold = "".join(b for _, b in paras).strip()
        return _ok(bold == o["bold"].strip(), bold=bold[:100])
    return _ok(True)


def _pdf_pypdf(data):
    import pypdf
    return "\n".join((p.extract_text() or "") for p in pypdf.PdfReader(io.BytesIO(data)).pages)


def _pdf_fitz(data):
    import fitz
    with fitz.open(stream=data, filetype="pdf") as d:
        return "\n".join(p.get_text() for p in d)


def _norm_ws(s):
    return re.sub(r"\s+", " ", s).strip()


def _pdf(ctx, o, read, text_fn):
    data = read(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    if not data.startswith(b"%PDF"):
        return _ok(False, reason="not a PDF")
    try:
        t = _norm_ws(text_fn(data))
    except Exception as e:  # noqa: BLE001
        return _ok(False, reason=f"unreadable: {type(e).__name__}")
    ok = all(_norm_ws(x) in t for x in o.get("contains", [])) and not any(_norm_ws(x) in t for x in o.get("absent", []))
    return _ok(ok, chars=len(t))


def _pptx_titles(data):
    import pptx
    prs = pptx.Presentation(io.BytesIO(data))
    out = []
    for s in prs.slides:
        title = None
        for sh in s.shapes:
            if sh.is_placeholder and sh.placeholder_format.type is not None and \
                    int(sh.placeholder_format.type) in (1, 3) and sh.has_text_frame:   # TITLE, CENTER_TITLE
                title = sh.text_frame.text.strip()
                break
        if title is None:
            first = next((sh.text_frame.text.strip() for sh in s.shapes if sh.has_text_frame and sh.text_frame.text.strip()), "")
            title = first
        out.append(title)
    return out


def _pptx(ctx, o, read, titles_fn):
    data = read(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    try:
        titles = titles_fn(data)
    except Exception as e:  # noqa: BLE001
        return _ok(False, reason=f"unreadable: {type(e).__name__}")
    return _ok(titles == o["titles"], got=titles[:20])


def _img_pil(data):
    from PIL import Image
    im = Image.open(io.BytesIO(data))
    im.load()
    return im.size, list(im.convert("RGB").getdata()), im.format


def _img_fitz(data):
    import fitz
    pm = fitz.Pixmap(data)
    if pm.alpha or pm.n != 3:
        pm = fitz.Pixmap(fitz.csRGB, pm, 0) if pm.n != 3 or pm.alpha else pm
    s = pm.samples
    px = [tuple(s[i:i + 3]) for i in range(0, len(s), 3)]
    fmt = "PNG" if data.startswith(b"\x89PNG") else "JPEG" if data[:3] == b"\xff\xd8\xff" else None
    return (pm.width, pm.height), px, fmt


def _image(ctx, o, read, img_fn):
    data = read(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    try:
        size, px, fmt = img_fn(data)
        wsize, wpx, _ = img_fn(base64.b64decode(o["expected_b64"]))
    except Exception as e:  # noqa: BLE001
        return _ok(False, reason=f"unreadable: {type(e).__name__}")
    if o.get("format") and fmt != o["format"]:
        return _ok(False, reason="format", got=fmt)
    if tuple(size) != tuple(wsize):
        return _ok(False, reason="size", got=list(size), want=list(wsize))
    tot = 0
    for a, b in zip(px, wpx):
        tot += abs(a[0] - b[0]) + abs(a[1] - b[1]) + abs(a[2] - b[2])
    mae = tot / (3.0 * max(1, len(px)))
    return _ok(mae <= float(o.get("max_mae", 6.0)), mae=round(mae, 3))


_CDP = ("import json,sys,urllib.request\n"
        "try:\n d=json.load(urllib.request.urlopen('http://127.0.0.1:%d/json/list',timeout=10))\n"
        "except Exception as e:\n print(json.dumps({'error':type(e).__name__}));sys.exit(0)\n"
        "print(json.dumps([x.get('url') for x in d if x.get('type')=='page']))")


def _tabs(ctx, o, _read):
    r = ctx.run(["python3", "-I", "-c", _CDP % int(o.get("port", 1337))], timeout=30)
    got = json.loads((r.get("output") or "null").strip() or "null")
    if not isinstance(got, list):
        return _ok(False, reason="no browser", got=got)
    norm = lambda u: (u or "").rstrip("/")  # noqa: E731
    urls = {norm(u) for u in got}
    ok = all(norm(u) in urls for u in o["urls"]) and not any(norm(u) in urls for u in o.get("forbid", []))
    return _ok(ok, tabs=sorted(urls)[:20])


def _walk_bm(node, out):
    if isinstance(node, dict):
        if node.get("type") == "url":
            out.append((node.get("url"), node.get("name")))
        for c in node.get("children", []) or []:
            _walk_bm(c, out)
        for v in (node.get("roots") or {}).values():
            _walk_bm(v, out)


def _bookmarks(ctx, o, read):
    data = read(ctx, o.get("path", "/home/user/.config/google-chrome/Default/Bookmarks"))
    if data is None:
        return _ok(False, reason="no bookmarks file")
    try:
        doc = json.loads(data)
    except ValueError:
        return _ok(False, reason="unreadable")
    found = []
    _walk_bm(doc, found)
    norm = lambda u: (u or "").rstrip("/")  # noqa: E731
    urls = [norm(u) for u, _ in found]
    ok = all(norm(u) in urls for u in o["urls"]) and not any(norm(u) in urls for u in o.get("forbid", []))
    return _ok(ok, n=len(found))


def _jsonc(s):
    s = re.sub(r'("(?:\\.|[^"\\])*")|//[^\n]*|/\*.*?\*/', lambda m: m.group(1) or "", s, flags=re.S)
    s = re.sub(r",(\s*[}\]])", r"\1", s)
    return json.loads(s) if s.strip() else {}


def _json_key(ctx, o, read):
    data = read(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    try:
        doc = _jsonc(data.decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        return _ok(False, reason="unreadable")
    for k, want in o["keys"].items():
        if not isinstance(doc, dict) or k not in doc or doc[k] != want:
            return _ok(False, reason="key differs", key=k, got=None if not isinstance(doc, dict) else doc.get(k))
    for k, want in (o.get("unchanged") or {}).items():
        if not isinstance(doc, dict) or doc.get(k) != want:
            return _ok(False, reason="other key changed", key=k)
    return _ok(True)


_HARNESS = ("import importlib.util,sys\n"
            "spec=importlib.util.spec_from_file_location('t6mod',sys.argv[1]);m=importlib.util.module_from_spec(spec)\n"
            "try:\n spec.loader.exec_module(m)\nexcept BaseException as e:\n print('T6-IMPORT-FAIL',type(e).__name__);sys.exit(3)\n"
            "fails=0\n"
            "for line in sys.argv[2].split('\\n'):\n"
            " line=line.strip()\n"
            " if not line: continue\n"
            " try:\n  ok=bool(eval(line,{'m':m}))\n except BaseException:\n  ok=False\n"
            " fails+=0 if ok else 1\n"
            "print(sys.argv[3] if fails==0 else 'T6-FAIL %d'%fails)\n")


def _pytests(ctx, o, read, order=1):
    src = read(ctx, o["path"])
    if src is None:
        return _ok(False, reason="missing")
    tests = o["tests"] if order == 1 else list(reversed(o["tests"]))
    nonce = o["nonce"] if order == 1 else o["nonce"][::-1]
    r = ctx.run(["timeout", "20", "python3", "-I", "-c", _HARNESS, o["path"], "\n".join(tests), nonce], timeout=40)
    out = (r.get("output") or "").strip().splitlines()
    passed = r.get("returncode") == 0 and bool(out) and out[-1] == nonce
    if passed and order == 2:
        # the redundant checker also parses the source on the host (no execution): the function must be defined
        import ast
        try:
            tree = ast.parse(src.decode("utf-8"))
        except (SyntaxError, UnicodeDecodeError, ValueError):
            return _ok(False, reason="does not parse")
        names = {n.name for n in ast.walk(tree) if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))}
        passed = o["function"] in names
    return _ok(passed, tail=out[-1:] if out else [])


def _sqlite(ctx, o, read):
    import os
    import sqlite3
    data = read(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    p = os.path.join(ctx.workdir, "t6.sqlite")
    with open(p, "wb") as f:
        f.write(data)
    try:
        rows = sqlite3.connect(p).execute(o["query"]).fetchall()
    except sqlite3.Error as e:
        return _ok(False, reason=f"sqlite: {type(e).__name__}")
    got = sorted(tuple(str(x) for x in r) for r in rows)
    return _ok(got == sorted(tuple(r) for r in o["rows"]), got=got[:10])


def _all(ctx, o, alt):
    """kind "all": every sub-check must pass (multi-step and multi-app tasks)."""
    infos = []
    for sub in o["checks"]:
        r = (check_alt if alt else check)(ctx, **sub)
        infos.append(r["info"])
        if r["score"] != 1.0:
            return {"score": 0.0, "info": {"failed": sub["kind"], "sub": r["info"]}}
    return {"score": 1.0, "info": {"subs": len(infos)}}


def check(ctx, kind, **o):
    o["kind"] = kind
    if kind == "text_file":
        return _text(ctx, o, _read)
    if kind == "files_state":
        return _files(ctx, o, _read)
    if kind == "zip_members":
        return _zip(ctx, o, _read)
    if kind in ("xlsx_cells", "xlsx_table", "xlsx_sheetnames"):
        return _xlsx(ctx, o, _read)
    if kind == "csv_table":
        return _csv(ctx, o, _read)
    if kind in ("docx_paragraphs", "docx_bold"):
        return _docx(ctx, o, _read, _docx_paras)
    if kind == "pdf_text":
        return _pdf(ctx, o, _read, _pdf_pypdf)
    if kind == "pptx_titles":
        return _pptx(ctx, o, _read, _pptx_titles)
    if kind == "image":
        return _image(ctx, o, _read, _img_pil)
    if kind == "chrome_tabs":
        return _tabs(ctx, o, _read)
    if kind == "chrome_bookmarks":
        return _bookmarks(ctx, o, _read)
    if kind == "json_keys":
        return _json_key(ctx, o, _read)
    if kind == "python_tests":
        return _pytests(ctx, o, _read, 1)
    if kind == "sqlite_rows":
        return _sqlite(ctx, o, _read)
    if kind == "all":
        return _all(ctx, o, False)
    raise ValueError(f"unknown kind {kind!r}")


def _xlsx_alt(ctx, o):
    data = _read_alt(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    try:
        sheets, order = _xlsx_raw(data)
    except Exception as e:  # noqa: BLE001
        return _ok(False, reason=f"unreadable: {type(e).__name__}")
    kind = o["kind"]
    tol = float(o.get("tol", 1e-6))
    if kind == "xlsx_sheetnames":
        return _ok(order == o["names"], got=order)
    sheet = o.get("sheet")
    name = order[0] if sheet is None else order[sheet] if isinstance(sheet, int) else sheet
    if name not in sheets:
        return _ok(False, reason="no such sheet")
    cells = sheets[name]
    if kind == "xlsx_cells":
        for ref, want in o["cells"].items():
            if not _same(cells.get(_ref(ref)), want, tol):
                return _ok(False, reason="cell differs", cell=ref)
        return _ok(True)
    eq, why = _table_eq(_raw_rows(cells), o["rows"], tol)
    return _ok(eq, why=why)


def _csv_alt(ctx, o):
    data = _read_alt(ctx, o["path"])
    if data is None:
        return _ok(False, reason="missing")
    try:
        import pandas as pd
        df = pd.read_csv(io.BytesIO(data), header=None, dtype=str, keep_default_na=False, encoding="utf-8-sig")
        rows = df.values.tolist()
    except Exception as e:  # noqa: BLE001
        return _ok(False, reason=f"unreadable: {type(e).__name__}")
    eq, why = _table_eq(rows, o["rows"], float(o.get("tol", 1e-6)))
    return _ok(eq, why=why)


def check_alt(ctx, kind, **o):
    o["kind"] = kind
    if kind == "text_file":
        return _text(ctx, o, lambda c, p: _read_alt(c, p))
    if kind == "files_state":
        return _files(ctx, o, lambda c, p: _read_alt(c, p))
    if kind == "zip_members":
        return _zip(ctx, o, lambda c, p: _read_alt(c, p))
    if kind in ("xlsx_cells", "xlsx_table", "xlsx_sheetnames"):
        return _xlsx_alt(ctx, o)
    if kind == "csv_table":
        return _csv_alt(ctx, o)
    if kind in ("docx_paragraphs", "docx_bold"):
        return _docx(ctx, o, lambda c, p: _read_alt(c, p), _docx_raw_paragraphs)
    if kind == "pdf_text":
        return _pdf(ctx, o, lambda c, p: _read_alt(c, p), _pdf_fitz)
    if kind == "pptx_titles":
        return _pptx(ctx, o, lambda c, p: _read_alt(c, p), _pptx_raw_titles)
    if kind == "image":
        return _image(ctx, o, lambda c, p: _read_alt(c, p), _img_fitz)
    if kind == "chrome_bookmarks":
        return _bookmarks(ctx, o, lambda c, p: _read_alt(c, p))
    if kind == "json_keys":
        return _json_key(ctx, o, lambda c, p: _read_alt(c, p))
    if kind == "python_tests":
        return _pytests(ctx, o, lambda c, p: _read_alt(c, p), 2)
    if kind == "sqlite_rows":
        return _sqlite(ctx, o, lambda c, p: _read_alt(c, p))
    if kind == "all":
        return _all(ctx, o, True)
    raise ValueError(f"no redundant checker for kind {kind!r}")
