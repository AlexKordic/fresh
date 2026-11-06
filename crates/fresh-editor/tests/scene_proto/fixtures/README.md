# Native table navigation fixture

`compose-table-layout.pb` is a `ComposeLayout` measured by File Commander's
shared Core Text renderer from its real-plugin `compose-tables.pb` scene.
The scene socket contract opens the same Markdown source, validates this
geometry against parser-owned cells, and drives Fresh's normal navigation and
selection actions. No macOS framework is required to replay the fixture.
`compose-table-source-layout.pb` records the same document after a missing-cell
motion reveals structural source. It uses a different style revision to verify
that logical column intent survives a pixel-goal reset during source editing.
Both fixtures use complete parser-owned column measurement inputs. Their widths
remain stable when source windows show different row subsets or reveal syntax;
the scene store retains inputs referred to by later pane updates.

To regenerate with sibling `file_commander` and `editor-fresh` checkouts, run
from File Commander on macOS:

```sh
FC_COMPOSE_TABLE_LAYOUT_FILE="$PWD/../editor-fresh/crates/fresh-editor/tests/scene_proto/fixtures/compose-table-layout.pb" \
FC_COMPOSE_TABLE_SOURCE_LAYOUT_FILE="$PWD/../editor-fresh/crates/fresh-editor/tests/scene_proto/fixtures/compose-table-source-layout.pb" \
  swift test --package-path macos --filter ComposeTableNavigationTests
```

Then run Fresh's `scene_socket` test with the `web` feature. The optional
`FC_COMPOSE_TABLE_LAYOUT_FILE` and `FC_COMPOSE_TABLE_SOURCE_LAYOUT_FILE`
environment variables select other geometry artifacts for a development round
trip; ordinary tests use the checked-in files.
