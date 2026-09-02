import unittest


class MathTests(unittest.TestCase):
    def test_add(self):
        self.assertEqual(1 + 1, 2)

    def test_sub(self):
        self.assertEqual(3 - 1, 2)

    @unittest.skip("later")
    def test_skip(self):
        pass


if __name__ == "__main__":
    unittest.main()
