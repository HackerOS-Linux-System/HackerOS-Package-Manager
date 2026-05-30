namespace Glyph {

public class GlyphHeaderBar : Adw.Bin {

    construct {
        var open_btn = new Gtk.Button.from_icon_name ("document-open-symbolic") {
            tooltip_text = "Open File (Ctrl+O)",
            action_name  = "win.open",
        };
        var save_btn = new Gtk.Button.from_icon_name ("document-save-symbolic") {
            tooltip_text = "Save (Ctrl+S)",
            action_name  = "win.save",
        };
        var find_btn = new Gtk.Button.from_icon_name ("edit-find-symbolic") {
            tooltip_text = "Find (Ctrl+F)",
            action_name  = "win.find",
        };

        var menu = new Gio.Menu ();
        menu.append ("_New Window",   "app.new-window");
        menu.append ("_Preferences",  "app.preferences");
        menu.append ("_About Glyph",  "app.about");
        menu.append_section (null, ({}) as Gio.MenuModel);  // separator
        menu.append ("_Quit",         "app.quit");

        var menu_btn = new Gtk.MenuButton () {
            icon_name  = "open-menu-symbolic",
            menu_model = menu,
        };

        var box = new Gtk.Box (Gtk.Orientation.HORIZONTAL, 4);
        box.add_css_class ("linked");
        box.append (open_btn);
        box.append (save_btn);
        box.append (find_btn);
        box.append (menu_btn);

        child = box;
    }
}

} // namespace Glyph
