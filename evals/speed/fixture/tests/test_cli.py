import contextlib
import io
import os
import tempfile
import unittest

from kvstore.cli import main


class CliTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.db = os.path.join(self.tmp.name, "db.json")

    def tearDown(self):
        self.tmp.cleanup()

    def run_cli(self, *argv):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = main(["--db", self.db, *argv])
        return code, out.getvalue()

    def test_set_then_get(self):
        self.assertEqual(self.run_cli("set", "x", "1")[0], 0)
        code, out = self.run_cli("get", "x")
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "1")

    def test_get_missing_exit_code(self):
        self.assertEqual(self.run_cli("get", "nope")[0], 1)


if __name__ == "__main__":
    unittest.main()
