// dit focus bridge: expose the focused window over the session bus.
//
// GNOME on Wayland gives ordinary clients no way to ask "which app is
// focused?". dit needs that answer to route dictation into a terminal running
// zellij (see src/terminal_route.rs), so this extension answers it on request.
// It does no work until dit calls it: no signals, no timers, no UI.
//
// Interface: io.reddb.dit.Focus at /io/reddb/dit/Focus, owned name
// io.reddb.dit.Focus.
//   Get() -> (s app_id, s wm_class, u pid, s title)
// Every field is empty/0 when nothing is focused or a value is unknown.

import Gio from 'gi://Gio';
import Shell from 'gi://Shell';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'io.reddb.dit.Focus';
const OBJECT_PATH = '/io/reddb/dit/Focus';

const INTERFACE_XML = `
<node>
  <interface name="io.reddb.dit.Focus">
    <method name="Get">
      <arg type="s" direction="out" name="app_id"/>
      <arg type="s" direction="out" name="wm_class"/>
      <arg type="u" direction="out" name="pid"/>
      <arg type="s" direction="out" name="title"/>
    </method>
    <property name="Version" type="u" access="read"/>
  </interface>
</node>`;

class FocusService {
    get Version() {
        return 1;
    }

    Get() {
        const win = global.display.get_focus_window();
        if (!win)
            return ['', '', 0, ''];

        const app = Shell.WindowTracker.get_default().get_window_app(win);
        const pid = win.get_pid();
        return [
            app?.get_id() ?? '',
            win.get_wm_class() ?? '',
            pid > 0 ? pid : 0,
            win.get_title() ?? '',
        ];
    }
}

export default class DitFocusExtension extends Extension {
    enable() {
        this._service = new FocusService();
        this._exported = Gio.DBusExportedObject.wrapJSObject(INTERFACE_XML, this._service);
        this._exported.export(Gio.DBus.session, OBJECT_PATH);
        this._nameId = Gio.bus_own_name_on_connection(
            Gio.DBus.session,
            BUS_NAME,
            Gio.BusNameOwnerFlags.NONE,
            null,
            null);
    }

    disable() {
        if (this._nameId) {
            Gio.bus_unown_name(this._nameId);
            this._nameId = 0;
        }
        this._exported?.unexport();
        this._exported = null;
        this._service = null;
    }
}
