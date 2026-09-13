import os, tempfile, time, unittest
from kvstore import Store

class HiddenTtlTests(unittest.TestCase):
    def test_ttl_expires_and_persists(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "db.json")
            s = Store(p)
            s.set("short", 1, ttl=0.2)
            s.set("long", 2)
            self.assertEqual(s.get("short"), 1)
            time.sleep(0.35)
            self.assertIsNone(s.get("short"))
            self.assertEqual(s.keys(), ["long"])
            self.assertFalse(s.delete("short"))
            s2 = Store(p)
            self.assertIsNone(s2.get("short"))
            self.assertEqual(s2.get("long"), 2)

    def test_cli_ttl(self):
        import contextlib, io
        from kvstore.cli import main
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "db.json")
            self.assertEqual(main(["--db", p, "set", "k", "v", "--ttl", "0.2"]), 0)
            time.sleep(0.35)
            self.assertEqual(main(["--db", p, "get", "k"]), 1)
