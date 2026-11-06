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


## Pixel page continuation

`compose-page-layout-a.pb` and `compose-page-layout-b.pb` record Core Text
geometry for successive source windows in the same Markdown document. A long
image URI crosses the source target size while its displayed object stays
small. A Page Down selection leaves the original header cursor outside the
second window. The shared complete-row flow anchor carries the remaining
pixel distance; Fresh accepts the destination only after measuring it and
keeps the original selection anchor and visible viewport.

To regenerate, export both pane snapshots from Fresh's real-plugin test:

```sh
FC_COMPOSE_PAGE_SCENE_A="$PWD/../file_commander/macos/Tests/FreshKitTests/Fixtures/compose-page-window-a.pb" \
FC_COMPOSE_PAGE_SCENE_B="$PWD/../file_commander/macos/Tests/FreshKitTests/Fixtures/compose-page-window-b.pb" \
  cargo test --locked -p fresh-editor --features web --test scene_socket -- --nocapture
```

Then export their geometry from File Commander on macOS and replay the Fresh
test again without the export variables:

```sh
FC_COMPOSE_PAGE_LAYOUT_A="$PWD/../editor-fresh/crates/fresh-editor/tests/scene_proto/fixtures/compose-page-layout-a.pb" \
FC_COMPOSE_PAGE_LAYOUT_B="$PWD/../editor-fresh/crates/fresh-editor/tests/scene_proto/fixtures/compose-page-layout-b.pb" \
  swift test --package-path macos --filter ComposeFlowTests
```

The optional exports sanitize only the image authority/document identifiers.
No running application, editor session or source file is modified. This large
URI also exercises complete minimum-glyph measurement. The native consumer
collects logical caret edges once per shaped sample; repeated point queries
previously made that fixture take about 29 seconds. The fixture is functional
evidence, not complete large-table or multiple-pane performance qualification.
