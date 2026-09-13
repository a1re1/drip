import contextlib, io, json, os, tempfile, unittest
from kvstore.cli import main

def run(*argv):
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        code = main(list(argv))
    return code, out.getvalue()

class HiddenJsonTests(unittest.TestCase):
    def test_json_mode(self):
        with tempfile.TemporaryDirectory() as d:
            db = os.path.join(d, "db.json")
            code, out = run("--db", db, "--json", "set", "k", "v")
            self.assertEqual(code, 0); self.assertEqual(json.loads(out), {"ok": True})
            code, out = run("--db", db, "--json", "get", "k")
            self.assertEqual(code, 0); self.assertEqual(json.loads(out), {"key": "k", "value": "v"})
            code, out = run("--db", db, "--json", "get", "zz")
            self.assertEqual(code, 1); self.assertEqual(json.loads(out), {"key": "zz", "value": None})
            code, out = run("--db", db, "--json", "keys")
            self.assertEqual(json.loads(out), ["k"])
            code, out = run("--db", db, "--json", "delete", "k")
            self.assertEqual(json.loads(out), {"deleted": True})
            code, out = run("--db", db, "get", "k")
            self.assertEqual(out.strip(), "(nil)")
