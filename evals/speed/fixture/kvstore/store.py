import json
import time


class Store:
    """A dict-backed key/value store with optional JSON persistence."""

    def __init__(self, path=None):
        self.path = path
        self._data = {}
        if path:
            try:
                with open(path) as fh:
                    self._data = json.load(fh)
            except FileNotFoundError:
                self._data = {}

    def set(self, key, value):
        if not isinstance(key, str) or not key:
            raise ValueError("key must be a non-empty string")
        self._data[key] = value
        self._flush()

    def get(self, key, default=None):
        return self._data.get(key, default)

    def delete(self, key):
        if key in self._data:
            del self._data[key]
            self._flush()
            return True
        return False

    def keys(self, prefix=""):
        return sorted(k for k in self._data if k.startswith(prefix))

    def page(self, page_size, page_number):
        """Return the keys on page `page_number` (1-based)."""
        keys = self.keys()
        start = page_size * page_number
        return keys[start:start + page_size]

    def _flush(self):
        if self.path:
            with open(self.path, "w") as fh:
                json.dump(self._data, fh)
