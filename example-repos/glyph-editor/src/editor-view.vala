namespace Glyph {

public class EditorView : Gtk.Widget {

    public signal void buffer_changed ();
    public signal void cursor_moved (int line, int col);

    private GtkSource.View   _view;
    private GtkSource.Buffer _buffer;
    private Gtk.SearchBar    _search_bar;
    private Gtk.SearchEntry  _search_entry;

    static construct {
        set_layout_manager_type (typeof (Gtk.BinLayout));
    }

    construct {
        _buffer = new GtkSource.Buffer (null) {
            highlight_syntax       = true,
            highlight_matching_brackets = true,
            style_scheme = GtkSource.StyleSchemeManager.get_default ()
                           .get_scheme ("Adwaita-dark"),
        };

        _view = new GtkSource.View.with_buffer (_buffer) {
            show_line_numbers     = true,
            highlight_current_line = true,
            tab_width             = 4,
            indent_width          = 4,
            auto_indent           = true,
            smart_backspace       = true,
            insert_spaces_instead_of_tabs = true,
            monospace             = true,
            wrap_mode             = Gtk.WrapMode.WORD_CHAR,
            background_pattern    = GtkSource.BackgroundPatternType.NONE,
            vexpand               = true,
            hexpand               = true,
        };

        // Ustaw font monospace przez CSS provider
        var css = new Gtk.CssProvider ();
        css.load_from_data ("textview { font-family: 'JetBrains Mono', 'Hack', monospace; font-size: 13px; }".data);
        _view.get_style_context ().add_provider (css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION);

        var scroll = new Gtk.ScrolledWindow () {
            child   = _view,
            vexpand = true,
            hexpand = true,
        };

        // Search bar
        _search_entry = new Gtk.SearchEntry ();
        _search_bar   = new Gtk.SearchBar () {
            child           = _search_entry,
            show_close_button = true,
        };

        var box = new Gtk.Box (Gtk.Orientation.VERTICAL, 0);
        box.append (_search_bar);
        box.append (scroll);
        box.set_parent (this);

        // Signals
        _buffer.changed.connect (() => buffer_changed ());
        _buffer.notify["cursor-position"].connect (() => {
            Gtk.TextIter iter;
            _buffer.get_iter_at_offset (out iter, _buffer.cursor_position);
            cursor_moved (iter.get_line (), iter.get_line_offset ());
        });

        _search_entry.search_changed.connect (do_search);
    }

    public override void dispose () {
        var child = get_first_child ();
        if (child != null) child.unparent ();
        base.dispose ();
    }

    public void set_text (string text) {
        _buffer.set_text (text, -1);
        // Move cursor to start
        Gtk.TextIter start;
        _buffer.get_start_iter (out start);
        _buffer.place_cursor (start);
    }

    public string get_text () {
        Gtk.TextIter start, end;
        _buffer.get_bounds (out start, out end);
        return _buffer.get_text (start, end, false);
    }

    public void set_language_for_file (File file) {
        var lang_mgr = GtkSource.LanguageManager.get_default ();
        bool uncertain;
        var content_type = ContentType.guess (file.get_path (), null, out uncertain);
        var lang = lang_mgr.guess_language (file.get_path (), content_type);
        _buffer.set_language (lang);
    }

    public void show_find_bar () {
        _search_bar.set_search_mode (true);
        _search_entry.grab_focus ();
    }

    public void show_go_to_line () {
        var dialog = new Adw.MessageDialog (
            get_ancestor (typeof (Gtk.Window)) as Gtk.Window,
            "Go to Line", null
        );
        var entry = new Gtk.Entry () {
            placeholder_text = "Line number",
            input_purpose    = Gtk.InputPurpose.DIGITS,
        };
        dialog.set_extra_child (entry);
        dialog.add_response ("cancel", "_Cancel");
        dialog.add_response ("go", "_Go");
        dialog.set_default_response ("go");
        dialog.response.connect ((id) => {
            if (id == "go") {
                var line = int.parse (entry.text) - 1;
                Gtk.TextIter iter;
                _buffer.get_iter_at_line (out iter, line);
                _buffer.place_cursor (iter);
                _view.scroll_to_iter (iter, 0.1, true, 0.5, 0.5);
            }
        });
        dialog.present ();
    }

    private void do_search () {
        // Prosta implementacja forward search
        var term = _search_entry.text;
        if (term.length == 0) return;

        Gtk.TextIter cursor, match_start, match_end;
        _buffer.get_iter_at_offset (out cursor, _buffer.cursor_position);

        if (cursor.forward_search (term, Gtk.TextSearchFlags.CASE_INSENSITIVE,
                                   out match_start, out match_end, null)) {
            _buffer.select_range (match_start, match_end);
            _view.scroll_to_iter (match_start, 0.1, true, 0.5, 0.5);
        }
    }
}

} // namespace Glyph
