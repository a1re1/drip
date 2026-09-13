import os, tempfile, unittest
from kvstore import Store
from kvstore.backends import Backend, MemoryBackend, JsonFileBackend

class HiddenBackendTests(unittest.TestCase):
    def test_memory_default(self):
        s = Store()
        s.set("a", 1)
        self.assertEqual(s.get("a"), 1)

    def test_path_compat_and_backend(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "db.json")
            Store(p).set("a", 1)
            self.assertEqual(Store(p).get("a"), 1)
            b = JsonFileBackend(p)
            self.assertIsInstance(b, Backend)
            self.assertEqual(b.load(), {"a": 1})
            s = Store(backend=b)
            s.set("b", 2)
            self.assertEqual(JsonFileBackend(p).load(), {"a": 1, "b": 2})

    def test_custom_backend(self):
        class Rec(Backend):
            def __init__(self): self.saved = []
            def load(self): return {"x": 9}
            def save(self, data): self.saved.append(dict(data))
        r = Rec(); s = Store(backend=r)
        self.assertEqual(s.get("x"), 9)
        s.set("y", 1)
        self.assertEqual(r.saved[-1], {"x": 9, "y": 1})
        self.assertIsInstance(MemoryBackend(), Backend)
