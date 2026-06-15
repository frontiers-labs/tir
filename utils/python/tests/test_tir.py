import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import tir

MODULE = """
module {
  func @f(%0: !i32, %1: !i32) -> !i32 {
    %2 = ptr.alloca : !ptr.p<!i32>
    ptr.store %0, %2
    %5 = ptr.load %2 : !i32
    %7 = muli %5, %1 : !i32
    return %7
  }
  module_end
}
"""


class TirTest(unittest.TestCase):
    def test_parse_run_pipeline_print(self):
        with tir.Context() as ctx:
            module = ctx.parse_module(MODULE)
            self.assertIn("ptr.alloca", module.to_string())
            ctx.run_pipeline(module, "builtin.func(mem2reg)")
            after = module.to_string()
            self.assertNotIn("ptr.alloca", after)

    def test_walk_and_inspect(self):
        with tir.Context() as ctx:
            module = ctx.parse_module(MODULE)
            names = [op.name for op in module.walk()]
            self.assertIn("muli", names)
            self.assertIn("func", names)
            muli = next(op for op in module.walk() if op.name == "muli")
            self.assertEqual(len(muli.operands), 2)
            self.assertEqual(muli.results[0].type, "!i32")

    def test_typed_construction(self):
        with tir.Context() as ctx:
            i32 = ctx.parse_type("!i32")
            block = ctx.create_block([i32, i32])
            region = ctx.create_region()
            region.append_block(block)
            a, b = block.args

            addi = tir.builtin.addi(ctx, a, b, result_type="!i32")
            block.append(addi)
            self.assertEqual(len(block.ops), 1)
            self.assertEqual(addi.operands[0].id, a.id)
            self.assertEqual(addi.operands[1].id, b.id)

            # A different dialect via the same generated path.
            alloca = tir.ptr.alloca(ctx, result_type="!ptr.p<!i32>")
            block.insert(0, alloca)
            self.assertEqual(alloca.dialect, "ptr")
            self.assertEqual(len(block.ops), 2)

    def test_schema_available(self):
        data = json.loads(tir.Context.schema_json())
        self.assertTrue(any(o["dialect"] == "builtin" and o["name"] == "addi" for o in data))

    def test_errors_raise(self):
        with tir.Context() as ctx:
            with self.assertRaises(tir.TirError):
                ctx.parse_module("this is not valid IR")
            with self.assertRaises(tir.TirError):
                ctx.register_target("nonsense")

    def test_target_lowering(self):
        self.assertIn("riscv64", tir.supported_targets())
        arith = (
            "module {\n  func @a(%0: !i32, %1: !i32) -> !i32 {\n"
            "    %2 = addi %0, %1 : !i32\n    return %2\n  }\n  module_end\n}"
        )
        with tir.Context() as ctx:
            module = ctx.parse_module(arith)
            ctx.run_target_pipeline(module, "rv64i", stage="isel")
            self.assertIn("riscv.", module.to_string())


if __name__ == "__main__":
    unittest.main()
