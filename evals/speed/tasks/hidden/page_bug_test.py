import unittest
from kvstore import Store

class HiddenPageTests(unittest.TestCase):
    def test_pages(self):
        s = Store()
        for k in "abcde":
            s.set(k, 0)
        self.assertEqual(s.page(2, 1), ["a", "b"])
        self.assertEqual(s.page(2, 2), ["c", "d"])
        self.assertEqual(s.page(2, 3), ["e"])
        self.assertEqual(s.page(2, 4), [])
