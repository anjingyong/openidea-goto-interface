# Go to Interface

An OpenIDEA plugin for **Go**: it marks methods that satisfy an interface method
in the gutter, and jumps to the interface declaration when you click the marker.

OpenIDEA does not ship a Go language server, so there is no "go to
implementation" or "go to interface" for Go at all. This plugin infers it from
the source text.

## What it does

- **Gutter marker.** A marker appears on the `func` line of every method whose
  name and parameter count match exactly one method declared in a
  `type X interface { ... }` block of the same package. Clicking it opens the
  interface method; hovering shows `Interface.Method`.
- **Go to Interface** (`⌘B`) works on the same line through the `definition`
  provider.
- The **Go to Interface: Jump to Interface Method** command reports how many
  methods in the current file satisfy an interface method — useful when a marker
  is missing and you want to know whether the plugin ran at all.

## What it deliberately does not do

Ambiguity is silent. If no interface method matches, or more than one does, no
marker is shown: a marker that jumps to the wrong place costs more than a
missing one. Only text is compared — method name and parameter count, never
types, because no type information is available.

Known limits, all by design in 0.1.0:

- only the same package (directory) is scanned;
- `_test.go` files are skipped;
- interfaces declared in another module or in a dependency are not seen;
- a type alias (`type ID = string`) makes the parameter count line up while the
  types differ — the plugin cannot tell, and may match;
- interfaces in a grouped `type ( ... )` declaration are not parsed.

## Capabilities

Both are requested because there is no other way to answer the question:

- `editor.read` — the plugin needs the text of the document being edited to find
  the `func` declarations, and the caret position for `⌘B`.
- `project.read` — interface declarations live in *other* files of the package,
  so the plugin lists the package's `.go` files and reads them.

Neither capability writes anything, and the plugin makes no network requests and
runs no processes.

## Build

```sh
cargo test                       # parsing logic, no host needed
openidea-plugin build            # → dist/plugin.wasm, validated like an install
openidea-plugin link             # load this folder in the IDE (Settings › Plugins)
```

## Release

```sh
openidea-plugin keygen allen.key                      # once; keep the file secret
openidea-plugin build
openidea-plugin pack . --key allen.key --publisher allen
shasum -a 256 allen.goto-interface-<version>.oiplugin
```

Attach the `.oiplugin` to a release and list it in the registry
[`l0ng-ai/openidea-plugins`](https://github.com/l0ng-ai/openidea-plugins):
`publishers/allen.toml` once, `plugins/allen.goto-interface.toml` for each
version.

## Licence

MIT — see `LICENSE`.
