import json, os, tempfile, threading, unittest, urllib.request, urllib.error
from kvstore.store import Store
from kvstore.server import make_server


def call(base, method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, method=method, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            return resp.status, json.loads(resp.read().decode() or "null")
    except urllib.error.HTTPError as err:
        return err.code, json.loads(err.read().decode() or "null")


class HiddenHttpServeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.store = Store(os.path.join(self.tmp.name, "db.json"))
        self.server = make_server(self.store, "127.0.0.1", 0)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = "http://127.0.0.1:%d" % self.server.server_address[1]

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.tmp.cleanup()

    def test_put_get_delete_roundtrip(self):
        self.assertEqual(call(self.base, "PUT", "/kv/alpha", {"value": "one"}), (200, {"ok": True}))
        self.assertEqual(call(self.base, "GET", "/kv/alpha"), (200, {"key": "alpha", "value": "one"}))
        self.assertEqual(self.store.get("alpha"), "one")
        self.assertEqual(call(self.base, "DELETE", "/kv/alpha"), (200, {"deleted": True}))
        self.assertEqual(call(self.base, "DELETE", "/kv/alpha"), (200, {"deleted": False}))
        status, body = call(self.base, "GET", "/kv/alpha")
        self.assertEqual(status, 404)
        self.assertEqual(body, {"error": "not found"})

    def test_keys_with_prefix(self):
        for k in ("a1", "a2", "b1"):
            call(self.base, "PUT", "/kv/" + k, {"value": k})
        self.assertEqual(call(self.base, "GET", "/keys"), (200, ["a1", "a2", "b1"]))
        self.assertEqual(call(self.base, "GET", "/keys?prefix=a"), (200, ["a1", "a2"]))

    def test_bad_requests(self):
        status, body = call(self.base, "PUT", "/kv/x", {"nope": 1})
        self.assertEqual(status, 400)
        self.assertIn("error", body)
        status, _ = call(self.base, "GET", "/nothing")
        self.assertEqual(status, 404)
