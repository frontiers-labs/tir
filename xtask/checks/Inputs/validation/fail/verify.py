import pathlib
import sys

assert pathlib.Path(sys.argv[1]).read_text() == "right\n"
