# sx — Universal Query Dispatch & Search

`sx` is a fast query engine and launcher with modular plugins, offering both a TUI and a GPUI-based graphical interface.

## Frontends

- **`sxc`**: Terminal UI (TUI) powered by Ratatui.
- **`sxp`**: Fast graphical launcher powered by GPUI.

## Building & Installing

Run the build script to compile the workspace, install plugins to `~/.config/sx/plugins`, install binaries to `~/.cargo/bin`, and register the desktop entry:

```bash
./build_install.sh
```

## Usage

```bash
# Launch GUI search
sxp

# Launch TUI search in the current directory
sxc

# Launch TUI search in a specific path
sxc -d ~/Projects
```

## GNOME Wayland Note

If using GNOME on Wayland and `sxp` does not take foreground focus when triggered via a global shortcut or terminal, enable smart window focus:

```bash
gsettings set org.gnome.desktop.wm.preferences focus-new-windows 'smart'
```
