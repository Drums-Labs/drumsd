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


PYPROJECT_04 = """[build-system]
requires = ["setuptools>=77"]

[project]
name = "drums-sdk"
version = "0.4.0"
description = "fixture SDK, 0.4 layout"
requires-python = ">=3.9"
license = "Apache-2.0"
license-files = ["LICENSE"]
dependencies = []

[project.urls]
Repository = "https://example.invalid/sdk"

[project.scripts]
drums = "drums.cli:main"

[tool.setuptools]
packages = ["drums"]

[tool.setuptools.package-data]
drums = ["mcp_tools.json"]
"""


def fixture_sdk_04(d):
    os.makedirs(os.path.join(d, "drums"))
    files = {"pyproject.toml": PYPROJECT_04, "LICENSE": "licence text\n", "drums/__init__.py": '__version__ = "0.4.0"\n',
             "drums/cli.py": "def main():\n    return 0\n", "drums/mcp_tools.json": "{}\n", "drums/notes.txt": "x\n"}
    for rel, text in files.items():
        with open(os.path.join(d, rel), "w") as f:
            f.write(text)
    return d


NEEDS_TOMLLIB = unittest.skipIf(build_wheel.tomllib is None, "the 0.4 layout needs tomllib (Python 3.11+, as on the hosts)")


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

    @NEEDS_TOMLLIB
    def test_sdk_04_layout_dist_name_package_data_scripts_and_licence(self):
        """The SDK from 0.3.0 on: distribution drums-sdk, package drums, an SPDX licence string with license-files,
        a non-Python data file and a console script. The 0.2.x builder could not build it (the v0.2.2 gap)."""
        with tempfile.TemporaryDirectory() as s, tempfile.TemporaryDirectory() as o:
            src = fixture_sdk_04(s)
            whl = build_wheel.build(src, o)
            self.assertEqual(os.path.basename(whl), "drums_sdk-0.4.0-py3-none-any.whl")
            di = "drums_sdk-0.4.0.dist-info"
            with zipfile.ZipFile(whl) as z:
                names = set(z.namelist())
                meta = z.read(f"{di}/METADATA").decode()
                self.assertEqual(z.read(f"{di}/entry_points.txt").decode(), "[console_scripts]\ndrums = drums.cli:main\n")
                self.assertEqual(z.read(f"{di}/licenses/LICENSE"), b"licence text\n")
                self.assertEqual(z.read("drums/mcp_tools.json"), b"{}\n")
            self.assertTrue({"drums/__init__.py", "drums/cli.py", "drums/mcp_tools.json", f"{di}/RECORD",
                             f"{di}/WHEEL", f"{di}/top_level.txt"} <= names, names)
            self.assertNotIn("drums/notes.txt", names)   # not named in package-data
            for line in ("Metadata-Version: 2.4", "Name: drums-sdk", "License-Expression: Apache-2.0",
                         "License-File: LICENSE", "Project-URL: Repository, https://example.invalid/sdk"):
                self.assertIn(line + "\n", meta)

    @NEEDS_TOMLLIB
    def test_missing_package_data_fails(self):
        with tempfile.TemporaryDirectory() as s, tempfile.TemporaryDirectory() as o:
            src = fixture_sdk_04(s)
            os.remove(os.path.join(src, "drums", "mcp_tools.json"))
            with self.assertRaises(SystemExit) as e:
                build_wheel.build(src, o)
            self.assertIn("mcp_tools.json matches no file", str(e.exception))

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
