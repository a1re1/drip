import argparse
import sys

from .store import Store


def build_parser():
    parser = argparse.ArgumentParser(prog="kv")
    parser.add_argument("--db", default=None, help="path to the JSON database file")
    sub = parser.add_subparsers(dest="command", required=True)
    p_set = sub.add_parser("set")
    p_set.add_argument("key")
    p_set.add_argument("value")
    p_get = sub.add_parser("get")
    p_get.add_argument("key")
    p_del = sub.add_parser("delete")
    p_del.add_argument("key")
    p_keys = sub.add_parser("keys")
    p_keys.add_argument("--prefix", default="")
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    store = Store(args.db)
    if args.command == "set":
        store.set(args.key, args.value)
        print("ok")
    elif args.command == "get":
        value = store.get(args.key)
        if value is None:
            print("(nil)")
            return 1
        print(value)
    elif args.command == "delete":
        print("deleted" if store.delete(args.key) else "(nil)")
    elif args.command == "keys":
        for key in store.keys(args.prefix):
            print(key)
    return 0


if __name__ == "__main__":
    sys.exit(main())
