namespace Glyph {

[GtkTemplate (ui = "/io/hackeros/GlyphEditor/ui/window.ui")]
public class GlyphWindow : Adw.ApplicationWindow {

    [GtkChild] private unowned Adw.HeaderBar      header_bar;
    [GtkChild] private unowned GlyphHeaderBar     toolbar;
    [GtkChild] private unowned EditorView         editor_view;
    [GtkChild] private unowned Gtk.Label          status_label;
    [GtkChild] private unowned Adw.ToastOverlay   toast_overlay;

    private File?   _current_file = null;
    private bool    _modified      = false;

    public bool modified {
        get { return _modified; }
        set {
            _modified = value;
            update_title ();
        }
    }

    public GlyphWindow (Application app) {
        Object (application: app);
    }

    construct {
        setup_actions ();
        setup_editor ();
        update_title ();
    }

    private void setup_actions () {
        ActionEntry[] entries = {
            { "open",       on_open       },
            { "save",       on_save       },
            { "save-as",    on_save_as    },
            { "close-tab",  on_close_tab  },
            { "find",       on_find       },
            { "go-to-line", on_go_to_line },
        };
        add_action_entries (entries, this);

        application.set_accels_for_action ("win.open",       { "<Ctrl>O" });
        application.set_accels_for_action ("win.save",       { "<Ctrl>S" });
        application.set_accels_for_action ("win.save-as",    { "<Ctrl><Shift>S" });
        application.set_accels_for_action ("win.find",       { "<Ctrl>F" });
        application.set_accels_for_action ("win.go-to-line", { "<Ctrl>G" });
    }

    private void setup_editor () {
        editor_view.buffer_changed.connect (() => { modified = true; });
        editor_view.cursor_moved.connect (update_cursor_position);
    }

    public void open_file (File file) {
        try {
            uint8[] contents;
            string etag;
            file.load_contents (null, out contents, out etag);
            var text = (string) contents;
            editor_view.set_text (text);
            editor_view.set_language_for_file (file);
            _current_file = file;
            modified = false;
            update_title ();
        } catch (Error e) {
            show_error ("Could not open file: " + e.message);
        }
    }

    private void save_to_file (File file) {
        try {
            var text = editor_view.get_text ();
            file.replace_contents (
                text.data, null, false,
                FileCreateFlags.REPLACE_DESTINATION, null, null
            );
            _current_file = file;
            modified = false;
            show_toast ("File saved.");
        } catch (Error e) {
            show_error ("Could not save: " + e.message);
        }
    }

    private void on_open (SimpleAction a, Variant? v) {
        var dialog = new Gtk.FileDialog ();
        dialog.open.begin (this, null, (obj, res) => {
            try {
                var file = dialog.open.end (res);
                open_file (file);
            } catch {}
        });
    }

    private void on_save (SimpleAction a, Variant? v) {
        if (_current_file != null) {
            save_to_file (_current_file);
        } else {
            on_save_as (a, v);
        }
    }

    private void on_save_as (SimpleAction a, Variant? v) {
        var dialog = new Gtk.FileDialog ();
        dialog.save.begin (this, null, (obj, res) => {
            try {
                var file = dialog.save.end (res);
                save_to_file (file);
            } catch {}
        });
    }

    private void on_close_tab  (SimpleAction a, Variant? v) { close (); }
    private void on_find        (SimpleAction a, Variant? v) { editor_view.show_find_bar (); }
    private void on_go_to_line  (SimpleAction a, Variant? v) { editor_view.show_go_to_line (); }

    private void update_title () {
        var fname = _current_file != null
            ? _current_file.get_basename ()
            : "Untitled";
        title = (modified ? "• " : "") + fname + " — Glyph";
    }

    private void update_cursor_position (int line, int col) {
        status_label.label = "Ln %d, Col %d".printf (line + 1, col + 1);
    }

    private void show_toast (string msg) {
        toast_overlay.add_toast (new Adw.Toast (msg));
    }

    private void show_error (string msg) {
        var dialog = new Adw.MessageDialog (this, "Error", msg);
        dialog.add_response ("ok", "_OK");
        dialog.present ();
    }
}

} // namespace Glyph
