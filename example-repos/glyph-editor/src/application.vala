namespace Glyph {

public class Application : Adw.Application {

    private static Application? _instance = null;

    public static Application get_instance () {
        return _instance;
    }

    public Application () {
        Object (
            application_id: "io.hackeros.GlyphEditor",
            flags: ApplicationFlags.HANDLES_OPEN
        );
        _instance = this;
    }

    construct {
        ActionEntry[] action_entries = {
            { "new-window",  on_new_window  },
            { "preferences", on_preferences },
            { "about",       on_about       },
            { "quit",        on_quit        },
        };
        add_action_entries (action_entries, this);

        set_accels_for_action ("app.new-window",  { "<Ctrl>N" });
        set_accels_for_action ("app.quit",        { "<Ctrl>Q" });
        set_accels_for_action ("app.preferences", { "<Ctrl>comma" });
    }

    public override void activate () {
        present_new_window (null);
    }

    public override void open (File[] files, string hint) {
        foreach (var file in files) {
            var win = present_new_window (file);
            _ = win; // suppress warning
        }
    }

    private GlyphWindow present_new_window (File? file) {
        var win = new GlyphWindow (this);
        win.present ();
        if (file != null) {
            win.open_file (file);
        }
        return win;
    }

    private void on_new_window (SimpleAction a, Variant? v) {
        present_new_window (null);
    }

    private void on_preferences (SimpleAction a, Variant? v) {
        var prefs = new PreferencesDialog ();
        prefs.set_transient_for (active_window as Gtk.Window);
        prefs.present ();
    }

    private void on_about (SimpleAction a, Variant? v) {
        var about = new Adw.AboutWindow () {
            application_name = "Glyph Editor",
            application_icon = "io.hackeros.GlyphEditor",
            developer_name   = "HackerOS Team",
            version          = VERSION,
            website          = "https://github.com/HackerOS-Linux-System",
            issue_url        = "https://github.com/HackerOS-Linux-System/glyph-editor/issues",
            license_type     = Gtk.License.GPL_3_0,
            transient_for    = active_window as Gtk.Window,
        };
        about.present ();
    }

    private void on_quit (SimpleAction a, Variant? v) {
        foreach (var win in get_windows ()) {
            win.close ();
        }
    }
}

} // namespace Glyph
