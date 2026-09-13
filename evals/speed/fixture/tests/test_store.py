import unittest

from kvstore import Store


class StoreTests(unittest.TestCase):
    def test_set_get(self):
        s = Store()
        s.set("a", 1)
        self.assertEqual(s.get("a"), 1)
        self.assertEqual(s.get("missing", "d"), "d")

    def test_delete(self):
        s = Store()
        s.set("a", 1)
        self.assertTrue(s.delete("a"))
        self.assertFalse(s.delete("a"))

    def test_keys_prefix(self):
        s = Store()
        for k in ("b1", "a2", "a1"):
            s.set(k, 0)
        self.assertEqual(s.keys("a"), ["a1", "a2"])


if __name__ == "__main__":
    unittest.main()
