# Plugin unit tests

Pure-logic tests for plugin code, run with Node's built-in TypeScript transform
(including parameter properties used by the shared picker):

    ./tests/run.sh

Not wired into CI; run them when touching the code they cover. A test imports
the real plugin source but must not import a plugin's top-level module, which
calls `getEditor()` at load.
