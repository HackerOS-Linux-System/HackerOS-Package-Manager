namespace Glyph {

public class PreferencesDialog : Adw.PreferencesWindow {

    private GLib.Settings _settings;

    construct {
        title        = "Glyph Preferences";
        search_enabled = true;

        _settings = new GLib.Settings ("io.hackeros.GlyphEditor");

        // ── Appearance page ──────────────────────────────────────────────────
        var appearance_page = new Adw.PreferencesPage () {
            title = "Appearance",
            icon_name = "applications-graphics-symbolic",
        };

        var font_group = new Adw.PreferencesGroup () { title = "Font" };

        var font_row = new Adw.ActionRow () {
            title    = "Editor Font",
            subtitle = "Monospace font for the text editor",
        };
        var font_btn = new Gtk.FontDialogButton (new Gtk.FontDialog ()) {
            valign = Gtk.Align.CENTER,
        };
        font_row.add_suffix (font_btn);
        font_group.add (font_row);

        var size_row = new Adw.SpinRow.with_range (8, 32, 1) {
            title = "Font Size",
            value = _settings.get_int ("font-size"),
        };
        size_row.notify["value"].connect (() => {
            _settings.set_int ("font-size", (int) size_row.value);
        });
        font_group.add (size_row);
        appearance_page.add (font_group);

        // Color scheme
        var scheme_group = new Adw.PreferencesGroup () { title = "Color Scheme" };
        var scheme_row   = new Adw.ComboRow () {
            title = "Syntax Highlighting Theme",
        };
        var schemes = GtkSource.StyleSchemeManager.get_default ().scheme_ids;
        scheme_row.model = new Gtk.StringList (schemes);
        scheme_group.add (scheme_row);
        appearance_page.add (scheme_group);
        add (appearance_page);

        // ── Editor page ──────────────────────────────────────────────────────
        var editor_page = new Adw.PreferencesPage () {
            title     = "Editor",
            icon_name = "text-editor-symbolic",
        };
        var indent_group = new Adw.PreferencesGroup () { title = "Indentation" };

        var spaces_row = new Adw.SwitchRow () {
            title    = "Use Spaces",
            subtitle = "Insert spaces instead of tabs",
            active   = _settings.get_boolean ("use-spaces"),
        };
        spaces_row.notify["active"].connect (() => {
            _settings.set_boolean ("use-spaces", spaces_row.active);
        });
        indent_group.add (spaces_row);

        var tabwidth_row = new Adw.SpinRow.with_range (2, 8, 1) {
            title = "Tab Width",
            value = _settings.get_int ("tab-width"),
        };
        tabwidth_row.notify["value"].connect (() => {
            _settings.set_int ("tab-width", (int) tabwidth_row.value);
        });
        indent_group.add (tabwidth_row);
        editor_page.add (indent_group);

        var misc_group = new Adw.PreferencesGroup () { title = "Display" };

        var linenum_row = new Adw.SwitchRow () {
            title  = "Show Line Numbers",
            active = _settings.get_boolean ("show-line-numbers"),
        };
        linenum_row.notify["active"].connect (() => {
            _settings.set_boolean ("show-line-numbers", linenum_row.active);
        });
        misc_group.add (linenum_row);

        var highlight_row = new Adw.SwitchRow () {
            title  = "Highlight Current Line",
            active = _settings.get_boolean ("highlight-current-line"),
        };
        highlight_row.notify["active"].connect (() => {
            _settings.set_boolean ("highlight-current-line", highlight_row.active);
        });
        misc_group.add (highlight_row);
        editor_page.add (misc_group);
        add (editor_page);
    }
}

} // namespace Glyph
