import unittest
from kvstore.textutil import truncate_middle, join_compact_fields, is_compact_token

class HiddenBigFileTests(unittest.TestCase):
    def test_exact_width(self):
        text = "abcdefghijklmnop"
        for width in range(2, 12):
            got = truncate_middle(text, width)
            self.assertEqual(len(got), width, (width, got))
            self.assertIn("…", got)
            self.assertTrue(text.startswith(got.split("…")[0]))
            self.assertTrue(text.endswith(got.split("…")[1]))
        self.assertEqual(truncate_middle("abcdefghij", 6), "abc…ij")
        self.assertEqual(truncate_middle("abcdefghij", 5), "ab…ij")

    def test_short_and_edge(self):
        self.assertEqual(truncate_middle("abc", 3), "abc")
        self.assertEqual(truncate_middle("abc", 10), "abc")
        self.assertEqual(truncate_middle("abcdef", 1), "…")
        self.assertEqual(truncate_middle("abcdef", 0), "")

    def test_neighbours_untouched(self):
        self.assertEqual(join_compact_fields(["a", " ", "b"]), "a.b")
        self.assertTrue(is_compact_token("ab-c"))
        self.assertFalse(is_compact_token(" ab"))
