desktop_file = i18n.merge_file(
    input:  'io.hackeros.GlyphEditor.desktop.in',
    output: 'io.hackeros.GlyphEditor.desktop',
    type:   'desktop',
    po_dir: '../po',
    install:       true,
    install_dir:   get_option('datadir') / 'applications',
)

install_data(
    'io.hackeros.GlyphEditor.gschema.xml',
    install_dir: get_option('datadir') / 'glib-2.0' / 'schemas',
)

install_data(
    'icons/scalable/apps/io.hackeros.GlyphEditor.svg',
    install_dir: get_option('datadir') / 'icons' / 'hicolor' / 'scalable' / 'apps',
)

validate_desktop = find_program('desktop-file-validate', required: false)
if validate_desktop.found()
    test('validate-desktop', validate_desktop,
         args: [desktop_file.full_path()])
endif
