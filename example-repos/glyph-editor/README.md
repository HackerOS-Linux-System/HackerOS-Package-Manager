# Glyph Editor

A minimal GTK4 text editor with syntax highlighting, written in Vala.

## Features

- Syntax highlighting for 200+ languages via GtkSourceView5
- Dark color scheme (Adwaita-dark) by default
- Line numbers, current line highlighting
- Find bar (Ctrl+F), Go to line (Ctrl+G)
- Preferences dialog with GSettings persistence
- Libadwaita native look & feel
- HackerOS sandbox-compatible (gui=true, full_gui=true)

## Installation

```sh
sudo hpm install glyph-editor
```

hpm will automatically install all build and runtime dependencies
(valac, libgtk-4-dev, libgtksourceview-5-dev, libadwaita-1-dev)
and build from source using Meson + Ninja.

## Usage

```sh
glyph-editor              # Open empty editor
glyph-editor file.vala    # Open a file
```

Or launch from the application menu (searches: "Glyph", "Editor").

## Building manually

```sh
meson setup _build --prefix=/usr
ninja -C _build
sudo ninja -C _build install
```

## Authors

HackerOS Team <hackeros068@gmail.com>

## License

GPL-3.0
