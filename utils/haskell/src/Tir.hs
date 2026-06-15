{-# LANGUAGE ForeignFunctionInterface #-}

-- | Haskell bindings to TIR over the generic C ABI (@libtir_capi@).
--
-- This is a proof of concept covering the generic verbs — create a context,
-- parse a module, run a pass pipeline, print, and read the operation schema.
-- Because the C ABI is generic over the uniform IR, these few foreign imports
-- drive every dialect with no per-op code.
module Tir
  ( Context
  , OpId
  , TirError (..)
  , withContext
  , parseModule
  , runPipeline
  , opToString
  , schemaJson
  ) where

import Control.Exception (Exception, bracket, throwIO)
import Control.Monad (when)
import Data.Word (Word32)
import Foreign.C.String (CString, peekCString, withCString, withCStringLen)
import Foreign.C.Types (CBool (..), CSize (..))
import Foreign.Ptr (Ptr, nullPtr)

-- | Opaque context handle.
data Context

-- | Opaque pass-manager handle.
data PassManager

-- | An operation is addressed by its dense integer id.
type OpId = Word32

invalidId :: OpId
invalidId = 0xFFFFFFFF

-- | Raised when a C ABI call reports failure; carries the last error message.
newtype TirError = TirError String

instance Show TirError where
  show (TirError msg) = "TirError: " ++ msg

instance Exception TirError

foreign import ccall unsafe "tir_context_create"
  c_context_create :: IO (Ptr Context)

foreign import ccall unsafe "tir_context_destroy"
  c_context_destroy :: Ptr Context -> IO ()

foreign import ccall unsafe "tir_last_error"
  c_last_error :: IO CString

foreign import ccall unsafe "tir_string_free"
  c_string_free :: CString -> IO ()

foreign import ccall unsafe "tir_parse_module"
  c_parse_module :: Ptr Context -> CString -> CSize -> IO Word32

foreign import ccall unsafe "tir_op_to_string"
  c_op_to_string :: Ptr Context -> Word32 -> IO CString

foreign import ccall unsafe "tir_pipeline_parse"
  c_pipeline_parse :: CString -> IO (Ptr PassManager)

foreign import ccall unsafe "tir_pipeline_run"
  c_pipeline_run :: Ptr PassManager -> Ptr Context -> Word32 -> IO CBool

foreign import ccall unsafe "tir_pipeline_destroy"
  c_pipeline_destroy :: Ptr PassManager -> IO ()

foreign import ccall unsafe "tir_schema_json"
  c_schema_json :: IO CString

-- | The most recent error message on this thread.
lastError :: IO String
lastError = do
  msg <- c_last_error
  if msg == nullPtr then pure "unknown error" else peekCString msg

-- | Decode and free a string owned by the library, or fail if it is null.
takeOwned :: String -> CString -> IO String
takeOwned what ptr
  | ptr == nullPtr = throwIO . TirError . ((what ++ ": ") ++) =<< lastError
  | otherwise = do
      s <- peekCString ptr
      c_string_free ptr
      pure s

-- | Run an action with a fresh context, destroying it afterwards.
withContext :: (Ptr Context -> IO a) -> IO a
withContext = bracket create c_context_destroy
  where
    create = do
      ctx <- c_context_create
      when (ctx == nullPtr) $ throwIO (TirError "failed to create context")
      pure ctx

-- | Parse a textual module, returning its op id.
parseModule :: Ptr Context -> String -> IO OpId
parseModule ctx src =
  withCStringLen src $ \(buf, len) -> do
    op <- c_parse_module ctx buf (fromIntegral len)
    when (op == invalidId) $ throwIO . TirError =<< lastError
    pure op

-- | Run an MLIR-style pass pipeline (e.g. @"builtin.func(mem2reg)"@) over @root@.
runPipeline :: Ptr Context -> OpId -> String -> IO ()
runPipeline ctx root spec =
  withCString spec $ \cspec -> do
    pm <- c_pipeline_parse cspec
    when (pm == nullPtr) $ throwIO . TirError =<< lastError
    ok <- c_pipeline_run pm ctx root
    c_pipeline_destroy pm
    when (ok == CBool 0) $ throwIO . TirError =<< lastError

-- | Render an operation to its textual form.
opToString :: Ptr Context -> OpId -> IO String
opToString ctx op = takeOwned "tir_op_to_string" =<< c_op_to_string ctx op

-- | The operation schema as a JSON string.
schemaJson :: IO String
schemaJson = takeOwned "tir_schema_json" =<< c_schema_json
