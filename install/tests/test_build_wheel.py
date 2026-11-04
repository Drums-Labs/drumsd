"""The release's SDK wheel: built from an SDK source tree with the standard library, byte-reproducible, importable, and
installable by pip without root (no build step in the source tree). A fixture SDK stands in for Drums-Labs/sdk."""
import base64
import hashlib
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.dont_write_bytecode = True     # no __pycache__ in install/
import build_wheel  # noqa: E402

PYPROJECT = """[project]
name = "drums"
version = "{v}"
description = "fixture SDK"
requires-python = ">=3.9"
license = {{text = "Proprietary"}}

[build-system]
requires = ["setuptools"]
"""


def read(path):
    with open(path, "rb") as f:
        return f.read()


def fixture_sdk(d, version="9.8.7", init_version=None):
    os.makedirs(os.path.join(d, "drums"))
    with open(os.path.join(d, "pyproject.toml"), "w") as f:
        f.write(PYPROJECT.format(v=version))
    with open(os.path.join(d, "drums", "__init__.py"), "w") as f:
        f.write(f'__version__ = "{init_version or version}"\n')
    with open(os.path.join(d, "drums", "aio.py"), "w") as f:
        f.write("from . import __version__\n")
    return d


class WheelTests(unittest.TestCase):
    def test_wheel_is_reproducible_complete_and_importable(self):
        with tempfile.TemporaryDirectory() as s, tempfile.TemporaryDirectory() as a, tempfile.TemporaryDirectory() as b:
            src = fixture_sdk(s)
            os.makedirs(os.path.join(src, "drums", "__pycache__"))
            open(os.path.join(src, "drums", "__pycache__", "x.py"), "w").close()
            w1, w2 = build_wheel.build(src, a), build_wheel.build(src, b)
            self.assertEqual(read(w1), read(w2))
            self.assertEqual(os.path.basename(w1), "drums-9.8.7-py3-none-any.whl")
            with zipfile.ZipFile(w1) as z:
                names = z.namelist()
                meta = z.read("drums-9.8.7.dist-info/METADATA").decode()
                rec = z.read("drums-9.8.7.dist-info/RECORD").decode().splitlines()
                for line in rec:
                    n, h, size = line.rsplit(",", 2)
                    if not h:
                        continue
                    data = z.read(n)
                    self.assertEqual(h, "sha256=" + base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode(), n)
                    self.assertEqual(int(size), len(data))
            self.assertEqual(sorted(n for n in names if n.startswith("drums/")), ["drums/__init__.py", "drums/aio.py"])
            self.assertEqual(len(rec), len(names))
            self.assertIn("Version: 9.8.7", meta)
            self.assertIn("License: Proprietary", meta)
            # a pure-Python wheel is importable as a zip: the package works from the wheel alone
            code = f"import sys; sys.path.insert(0, {w1!r}); import drums, drums.aio; print(drums.__version__, drums.__file__)"
            r = subprocess.run([sys.executable, "-I", "-c", code], capture_output=True, text=True, check=False)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("9.8.7", r.stdout)
            self.assertIn(w1, r.stdout)

    def test_cli_takes_sdk_dir_and_out_dir(self):
        with tempfile.TemporaryDirectory() as s, tempfile.TemporaryDirectory() as o:
            src = fixture_sdk(s)
            script = os.path.join(os.path.dirname(os.path.abspath(build_wheel.__file__)), "build_wheel.py")
            r = subprocess.run([sys.executable, "-B", script, src, o], capture_output=True, text=True, check=False)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(r.stdout.strip(), os.path.join(o, "drums-9.8.7-py3-none-any.whl"))
            r = subprocess.run([sys.executable, "-B", script, o], capture_output=True, text=True, check=False)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("usage", r.stderr)

    def test_version_mismatch_fails(self):
        with tempfile.TemporaryDirectory() as s, tempfile.TemporaryDirectory() as o:
            src = fixture_sdk(s, version="1.0.0", init_version="1.0.1")
            with self.assertRaises(SystemExit) as e:
                build_wheel.build(src, o)
            self.assertIn("version mismatch", str(e.exception))
            self.assertEqual(os.listdir(o), [])

    @unittest.skipUnless(os.environ.get("DRUMS_TEST_PIP") == "1", "set DRUMS_TEST_PIP=1 to pip install the wheel into a venv")
    def test_pip_installs_the_wheel_into_a_venv(self):
        with tempfile.TemporaryDirectory() as d:
            whl = build_wheel.build(fixture_sdk(os.path.join(d, "sdk")), os.path.join(d, "dist"))
            subprocess.run([sys.executable, "-m", "venv", os.path.join(d, "v")], check=True)
            py = os.path.join(d, "v", "bin", "python")
            subprocess.run([py, "-m", "pip", "install", "-q", "--no-index", whl], check=True)
            out = subprocess.run([py, "-I", "-c", "import drums; print(drums.__file__)"], capture_output=True, text=True, check=True, cwd=d).stdout
            self.assertIn(os.path.realpath(os.path.join(d, "v")), os.path.realpath(out.strip()))


if __name__ == "__main__":
    unittest.main()
