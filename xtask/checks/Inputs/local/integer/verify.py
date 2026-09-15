import argparse

parser = argparse.ArgumentParser()
parser.add_argument("--stdout", required=True)
parser.add_argument("argument")
args = parser.parse_args()
assert open(args.stdout, "rb").read() == b""
assert args.argument == "fixture"
