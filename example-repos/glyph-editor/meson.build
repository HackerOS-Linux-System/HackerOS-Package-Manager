project(
    'glyph-editor',
    'vala', 'c',
    version: '0.3.0',
    meson_version: '>= 0.59',
    default_options: ['warning_level=2'],
)

i18n = import('i18n')

gnome_dep    = dependency('glib-2.0',        version: '>= 2.74')
gtk_dep      = dependency('gtk4',            version: '>= 4.8')
source_dep   = dependency('gtksourceview-5', version: '>= 5.4')
adwaita_dep  = dependency('libadwaita-1',    version: '>= 1.2')

conf = configuration_data()
conf.set_quoted('APP_ID',      'io.hackeros.GlyphEditor')
conf.set_quoted('VERSION',     meson.project_version())
conf.set_quoted('GETTEXT_PKG', meson.project_name())
conf.set_quoted('LOCALEDIR',   get_option('prefix') / get_option('localedir'))
configure_file(output: 'config.h', configuration: conf)

subdir('src')
subdir('data')
subdir('po')

gnome = import('gnome')
gnome.post_install(
    glib_compile_schemas: true,
    gtk_update_icon_cache: true,
    update_desktop_database: true,
)
