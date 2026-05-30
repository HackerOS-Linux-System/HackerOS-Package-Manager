executable(
    'glyph-editor',
    files([
        'main.vala',
        'application.vala',
        'window.vala',
        'editor-view.vala',
        'headerbar.vala',
        'preferences.vala',
    ]),
    vala_args: [
        '--pkg', 'gtk4',
        '--pkg', 'gtksourceview-5',
        '--pkg', 'libadwaita-1',
        '--header=config.h',
    ],
    c_args: ['-include', 'config.h'],
    dependencies: [gnome_dep, gtk_dep, source_dep, adwaita_dep],
    install: true,
)
