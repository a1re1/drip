import contextlib, io, os, tempfile, unittest
from kvstore.cli import main

class HiddenCountTests(unittest.TestCase):
    def test_count(self):
        with tempfile.TemporaryDirectory() as d:
            db = os.path.join(d, "db.json")
            for k in ("a1", "a2", "b1"):
                main(["--db", db, "set", k, "v"])
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(main(["--db", db, "count"]), 0)
            self.assertEqual(out.getvalue().strip(), "3")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(main(["--db", db, "count", "--prefix", "a"]), 0)
            self.assertEqual(out.getvalue().strip(), "2")
