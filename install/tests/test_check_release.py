"""install/check-release.py refuses a release tarball that carries what must not ship: another repository's bundle
(v0.2.1 shipped sources/drums-lab.bundle), missing licence files, keys, internal paths, account ids. Fixture repos
stand in for drumsd and drums-criu."""
import hashlib
import importlib.util
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
spec = importlib.util.spec_from_file_location("check_release", os.path.join(HERE, "check-release.py"))
cr = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cr)

ENV = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid", "GIT_COMMITTER_NAME": "t",
       "GIT_COMMITTER_EMAIL": "t@example.invalid", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}


def git(cwd, *args):
    return subprocess.run(["git", *args], cwd=cwd, env=ENV, capture_output=True, text=True, check=True).stdout.strip()


def repo(d, files):
    os.makedirs(d)
    git(d, "init", "-q", "-b", "main")
    for i, (rel, text) in enumerate(files):
        with open(os.path.join(d, rel), "w") as f:
            f.write(text)
        git(d, "add", rel)
        git(d, "commit", "-q", "-m", f"c{i}")
    return d


def bundle(src, name, out):
    git(src, "branch", "-f", f"release-{name}", "HEAD")
    git(src, "bundle", "create", "-q", out, f"release-{name}")
    git(src, "branch", "-D", f"release-{name}")


def wheel(path, files=None):
    with zipfile.ZipFile(path, "w") as z:
        for n, b in (files or {"drums/__init__.py": b'__version__ = "0.4.0"\n'}).items():
            z.writestr(n, b)


class Fixture:
    """A release tree that passes; tests break one thing at a time and pack it."""

    def __init__(self, d):
        self.d = d
        self.drumsd = repo(os.path.join(d, "drumsd"), [("a.txt", "root\n"), ("b.txt", "two\n")])
        self.criu = repo(os.path.join(d, "criu"), [("base.c", "v4.3\n"), ("fork.c", "patch\n")])
        self.root = git(self.drumsd, "rev-list", "--max-parents=0", "HEAD")
        self.base = git(self.criu, "rev-parse", "HEAD~1")
        self.commit = git(self.drumsd, "rev-parse", "HEAD")
        self.r = os.path.join(d, "out", f"drums-release-{self.commit[:12]}")
        for sub in ("install", "python", "sources", "sdk", "docs"):
            os.makedirs(os.path.join(self.r, sub))
        self.write("install.sh", "#!/bin/sh\n")
        self.write("install/lock.json", "{}\n")
        self.write("python/x.py", "print(1)\n")
        self.write("docs/install.md", "install\n")
        for f in ("LICENSE", "NOTICE", "THIRD_PARTY_LICENSES"):
            self.write(f, f"{f}\n")
        self.write("RELEASE_COMMIT", self.commit + "\n")
        bundle(self.drumsd, "drumsd", os.path.join(self.r, "sources", "drumsd.bundle"))
        bundle(self.criu, "drums-criu", os.path.join(self.r, "sources", "drums-criu.bundle"))
        wheel(os.path.join(self.r, "sdk", "drums_sdk-0.4.0-py3-none-any.whl"))

    def write(self, rel, text):
        with open(os.path.join(self.r, rel), "w") as f:
            f.write(text)

    def pack(self):
        sums = []
        for root, _, fs in os.walk(self.r):
            for f in sorted(fs):
                full = os.path.join(root, f)
                rel = os.path.relpath(full, self.r)
                if rel != "SHA256SUMS":
                    with open(full, "rb") as fh:
                        sums.append(f"{hashlib.sha256(fh.read()).hexdigest()}  ./{rel}")
        self.write("SHA256SUMS", "\n".join(sorted(sums, key=lambda s: s.split('  ')[1])) + "\n")
        tb = self.r + ".tar.gz"
        with tarfile.open(tb, "w:gz") as t:
            t.add(self.r, arcname=os.path.basename(self.r))
        return tb

    def check(self):
        return cr.check(self.pack(), drumsd_root=self.root, criu_base=self.base)


class CheckReleaseTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.f = Fixture(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def assertRefused(self, needle):
        problems = self.f.check()
        self.assertTrue(any(needle in p for p in problems), problems)

    def test_clean_release_passes(self):
        self.assertEqual(self.f.check(), [])

    def test_another_repositorys_bundle_is_refused(self):
        lab = repo(os.path.join(self.tmp.name, "lab"), [("x", "private\n")])
        bundle(lab, "drums-lab", os.path.join(self.f.r, "sources", "drums-lab.bundle"))
        self.assertRefused("want exactly ['drums-criu.bundle', 'drumsd.bundle']")

    def test_a_private_repo_bundled_under_the_drumsd_name_is_refused(self):
        lab = repo(os.path.join(self.tmp.name, "lab"), [("x", "private\n")])
        bundle(lab, "drumsd", os.path.join(self.f.r, "sources", "drumsd.bundle"))
        self.assertRefused("is not RELEASE_COMMIT")

    def test_missing_licence_files_are_refused(self):
        os.remove(os.path.join(self.f.r, "NOTICE"))
        self.assertRefused("NOTICE: missing")

    def test_unexpected_top_level_file_is_refused(self):
        self.f.write("CHANGES-RC1.md", "notes\n")
        self.assertRefused("CHANGES-RC1.md: not an allowed top-level entry")

    def test_internal_paths_keys_and_account_ids_are_refused(self):
        # built from pieces: the release scans this file too, and it must not match its own patterns
        cases = {"/Us" + "ers/someone/drums": "macOS home path", "see drums-" + "info runbooks/x": "internal doc store citation",
                 "ssh drums-" + "linux-big": "internal ssh host alias",
                 "-----BEGIN OPENSSH " + "PRIVATE KEY-----": "private key block"}
        for text, what in cases.items():
            with self.subTest(what=what):
                self.f.write("docs/install.md", text + "\n")
                self.assertRefused(f"docs/install.md: {what}")
        self.f.write("docs/install.md", "install\n")
        old = cr.ACCOUNT_HASHES
        cr.ACCOUNT_HASHES = {hashlib.sha256(b"123456789012").hexdigest()}
        try:
            self.f.write("install/lock.json", '{"account": "123456789012"}\n')
            self.assertRefused("install/lock.json: a Drums AWS account id")
        finally:
            cr.ACCOUNT_HASHES = old

    def test_files_inside_the_wheel_are_scanned(self):
        whl = os.path.join(self.f.r, "sdk", "drums_sdk-0.4.0-py3-none-any.whl")
        wheel(whl, {"drums/__init__.py": b"# /Us" + b"ers/someone/src\n"})
        self.assertRefused("!drums/__init__.py: macOS home path")

    def test_a_secret_in_bundle_history_is_refused(self):
        with open(os.path.join(self.f.drumsd, "k.txt"), "w") as f:
            f.write("AKIA" + "Q" * 16 + "\n")
        git(self.f.drumsd, "add", "k.txt")
        git(self.f.drumsd, "commit", "-q", "-m", "key")
        git(self.f.drumsd, "rm", "-q", "k.txt")
        git(self.f.drumsd, "commit", "-q", "-m", "gone")
        self.f.commit = git(self.f.drumsd, "rev-parse", "HEAD")
        self.f.write("RELEASE_COMMIT", self.f.commit + "\n")
        bundle(self.f.drumsd, "drumsd", os.path.join(self.f.r, "sources", "drumsd.bundle"))
        self.assertRefused("sources/drumsd.bundle history: AWS access key id")

    def test_sha256sums_must_match(self):
        tb = self.f.pack()
        with tarfile.open(tb, "r:gz") as t:
            names = t.getnames()
        self.assertIn(os.path.basename(self.f.r) + "/SHA256SUMS", names)
        self.f.write("python/x.py", "changed after the sums\n")
        tb2 = self.f.r + ".tar.gz"
        with tarfile.open(tb2, "w:gz") as t:
            t.add(self.f.r, arcname=os.path.basename(self.f.r))
        problems = cr.check(tb2, drumsd_root=self.f.root, criu_base=self.f.base)
        self.assertTrue(any("python/x.py: missing from or different in SHA256SUMS" in p for p in problems), problems)

    def test_the_v021_shape_is_refused(self):
        """v0.2.1's tarball: a drums-lab bundle, no LICENSE/NOTICE, CHANGES-RC1.md at the root."""
        lab = repo(os.path.join(self.tmp.name, "lab"), [("x", "private\n")])
        bundle(lab, "drums-lab", os.path.join(self.f.r, "sources", "drums-lab.bundle"))
        for f in ("LICENSE", "NOTICE", "THIRD_PARTY_LICENSES"):
            os.remove(os.path.join(self.f.r, f))
        self.f.write("CHANGES-RC1.md", "Coordinator decisions\n")
        problems = self.f.check()
        for needle in ("drums-lab.bundle", "LICENSE: missing", "NOTICE: missing", "CHANGES-RC1.md"):
            self.assertTrue(any(needle in p for p in problems), (needle, problems))

    def test_cli_exit_codes(self):
        script = os.path.join(HERE, "check-release.py")
        r = subprocess.run([sys.executable, "-B", script], capture_output=True, text=True)
        self.assertEqual(r.returncode, 2)
        lab = repo(os.path.join(self.tmp.name, "lab"), [("x", "private\n")])
        bundle(lab, "drums-lab", os.path.join(self.f.r, "sources", "drums-lab.bundle"))
        r = subprocess.run([sys.executable, "-B", script, self.f.pack()], capture_output=True, text=True)
        self.assertEqual(r.returncode, 1)
        self.assertIn("REFUSED", r.stderr)


if __name__ == "__main__":
    unittest.main()
