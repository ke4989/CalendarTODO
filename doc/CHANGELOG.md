# Changelog

## 0.1.2

- Recover the desktop list after Show Desktop leaves a visible list below the desktop surface. Shell transition windows trigger bounded observations; a healthy list or a covering application does not authorize a restack.
- Keep date/time edits and explicit Save consistent with pending auto-save, and refresh the matching event's conflict state after a rejected remote submission.
- Allow otherwise eligible event edits when a supported DISPLAY alarm carries an opaque Apple alarm identifier; retain unsupported-content restrictions and existing notification behavior.
- Close a valid creation dialog after staging its immutable draft, show a read-only provisional row in both windows, and preserve an explicit recovery path if saving fails.
- Reconcile creation, successor deletion and independent five-second deletion Undo across both windows without letting late results replace another editor.
- Include cached past, future and untimed matches across all eligible displayed calendars in keyword search, even when future matches exist. Keep ordinary empty-query timeline browsing and explicit remote time-range reads.
- Simplify search controls and improve the result title, calendar and date hierarchy.

## 0.1.1

- Search local tasks and cached iCloud events from an independent paged view.
- Share calendar display choices between the main and desktop windows and persist them across a full exit.
- Read a selected time range from an already bound iCloud calendar. This does not replace ordinary synchronization or its deletion checks.
- Keep the remote version after a synchronization conflict. A blocked local edit is retried only as a separate successor version.
- Permanently delete a confirmed local calendar together with its local items.
- Reject iCloud event text that cannot be transported safely, without dropping the event identity on retry.
- Continue reading Agenda when older remote-edit records do not match the current submission rules.

The limits in the README still apply. In particular, the app does not create iCloud calendars, edit iCloud reminder times, move events between calendars, or deliver reminders after a full exit.

## 0.1.0

Calendar TODO for Windows x64:

- Local calendars and tasks with recurring schedules, notes, tags and pinning.
- Agenda, desktop companion and tray controls.
- iCloud Calendar synchronization and eligible event creation, editing and deletion, with editing opt-in and conflict handling.
- Supported timezone-aware daily and weekly event display, including durations and recurrence exceptions.
- Independent five-second deletion Undo and detail auto-save.
- Windows reminders, Chinese/English interface text and light/dark appearance.

See the [Chinese README](../README.md) or [English README](../README.en.md) for features and current limits, and the [installation guide](INSTALL.en.md) for WebView2 prerequisites. Available downloads and version notes are listed on the [Releases page](https://github.com/ke4989/CalendarTODO/releases).

### Desktop recovery and offline licenses

- Desktop Companion autonomously recovers after Win+D with a smooth reveal. The final integrated candidate passed manual Win+D verification; its three-finger Show Desktop integration check remains pending, with successful gesture evidence retained for the preceding recovery candidate.
- Offline license viewer in About & Support, scoped Microsoft component conditions and complete third-party texts. Both installers require explicit agreement, including unattended deployment; uninstall stays unrestricted.
