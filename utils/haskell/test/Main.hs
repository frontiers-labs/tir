-- | Smoke test for the Haskell bindings: parse -> promote slots -> print, and
-- check the schema is reachable. Linked against libtir_capi.
module Main (main) where

import Data.List (isInfixOf)
import System.Exit (exitFailure)
import Tir

moduleSrc :: String
moduleSrc =
  unlines
    [ "module {"
    , "  func.func @f(%0: !i32, %1: !i32) -> !i32 {"
    , "    %2 = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>"
    , "    ptr.store %0, %2"
    , "    %3 = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>"
    , "    ptr.store %1, %3"
    , "    %4 = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>"
    , "    %5 = ptr.load %2 : !i32"
    , "    %6 = ptr.load %3 : !i32"
    , "    %7 = muli %5, %6 : !i32"
    , "    ptr.store %7, %4"
    , "    %8 = ptr.load %4 : !i32"
    , "    %9 = constant {value = 1} : !i32"
    , "    %10 = addi %8, %9 : !i32"
    , "    func.return %10"
    , "  }"
    , "  module_end"
    , "}"
    ]

check :: String -> Bool -> IO ()
check label ok =
  if ok then putStrLn ("ok - " ++ label) else putStrLn ("FAIL - " ++ label) >> exitFailure

main :: IO ()
main = withContext $ \ctx -> do
  m <- parseModule ctx moduleSrc
  before <- opToString ctx m
  check "parses allocas" ("ptr.alloca" `isInfixOf` before)

  runPipeline ctx m "func.func(thread-state,instcombine,erase-state)"
  after <- opToString ctx m
  check "promotion removes allocas" (not ("ptr.alloca" `isInfixOf` after))
  check "promotion promotes operands" ("muli %0, %1" `isInfixOf` after)

  schema <- schemaJson
  check "schema covers builtin.addi" ("\"name\":\"addi\"" `isInfixOf` schema)
  putStrLn "all Haskell binding checks passed"
