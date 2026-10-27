<p align="center">
  <a href="README.md">简体中文</a> · <strong>English</strong>
</p>

<p align="center">
  <img src="assets/logo.png" width="88" height="88" alt="Calendar TODO logo">
</p>

<h1 align="center">Calendar TODO</h1>

<p align="center">Keep iCloud Calendar and your task list on your Windows desktop, and focus on what needs doing</p>

<p align="center">
  <a href="https://github.com/ke4989/CalendarTODO/releases">Downloads &amp; releases</a> ·
  <a href="doc/INSTALL.en.md">Installation guide</a> ·
  <a href="#build-from-source">Build from source</a>
</p>

Calendar TODO is a Windows agenda and task tool. Connect iCloud Calendar to view and manage events, or keep your items locally. Plan in the main window or keep the desktop list beside your everyday work.

## Screenshots

### Main window

Review your agenda, manage calendars and edit item details in the side pane.

![Calendar TODO main window with an agenda list and detail editor](assets/screenshots/main-window.png)

### Desktop list

Keep a smaller view of your tasks in desktop companion or always-on-top mode.

<p align="center">
  <img src="assets/screenshots/desktop-panel.png" width="360" alt="Calendar TODO desktop list">
</p>

## Features

- **Calendars and tasks** — Organize local items by calendar, with dates, notes, tags, pinning and fixed recurrence.
- **Windows desktop list** — A compact separate window with collapse controls, adjustable background opacity and tray shortcuts.
- **iCloud Calendar** — Choose calendars to display and synchronize. Enable remote editing to create, update and delete eligible events.
- **Everyday editing** — Drag to reorder and mark items complete. Event edits save automatically.
- **Reminders and appearance** — Windows due reminders while the app is running, Chinese and English UI text, two palettes, and light, dark or system appearance.

## Download and install

The target platform is **Windows x64**, with **Microsoft Edge WebView2 Runtime installed beforehand**. macOS, Linux and ARM64 packages are not provided.

Use the [Releases page](https://github.com/ke4989/CalendarTODO/releases) to find available downloads and version notes.

| Package | Use |
|---|---|
| NSIS: `Calendar TODO_<version>_x64-setup.exe` | Preferred for a regular installation |
| MSI: `Calendar TODO_<version>_x64_en-US.msi` | Optional Windows Installer package, when offered in the release |

1. Download one installer and verify its SHA-256 against that release's `SHA256SUMS.txt`.
2. Make sure WebView2 Runtime is installed, then run the Calendar TODO installer.
3. Open the app and create your first item in a local calendar. No cloud account is required.
4. To connect iCloud events and include them in your task list, obtain an app-specific password from the [Apple Account website](https://account.apple.com/). The password is stored in Windows Credential Manager; the app does not expose it.

**The installers do not download, bundle or install WebView2 Runtime.** Do not assume every Windows 10 or Windows 11 device already has it. If it is missing, follow [Microsoft's official WebView2 instructions](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) to install the Runtime yourself and review the terms Microsoft presents. Calendar TODO installation may finish without the Runtime, while the application interface cannot start.

See the [installation guide](doc/INSTALL.en.md) for hash verification, Runtime recovery and uninstallation. Check each release's notes for Windows compatibility, signing status and known issues. The standalone application EXE is not offered as a portable package.

## Getting started

- **Start locally.** Create an item and add dates, notes or tags. Main-window details save after a short pause; editing in the desktop list uses an explicit Save action.
- **Choose your view.** Show or hide the desktop list from the tray, and switch between desktop companion and always-on-top modes.
- **Complete or undo.** Mark items complete as you work. To restore a deleted item, select **Undo** in its own five-second notification.
- **Quit from the tray.** Closing the main window hides it to the tray. Choose **Quit** to exit completely, and keep the app running if you rely on reminders.

### Connect iCloud Calendar

In **Settings → Calendars & Accounts**, add your Apple Account and an [app-specific password](https://support.apple.com/102654), then select the calendars to display and synchronize. China-region iCloud is the primary compatibility target; support for every CalDAV server is not claimed.

Remote editing is off by default. **Once enabled**, eligible creates, edits and deletions synchronize to iCloud. Use the app's conflict-resolution or retry actions if a synchronization operation fails or conflicts.

**Local tasks and iCloud events are separate data sources.** Local tasks are not uploaded to iCloud. Completion, tags, pinning and other local presentation metadata are not written into remote calendar fields. **Apple Reminders is not supported.**

### Current limits

- Creating iCloud calendars, editing iCloud reminder times and moving remote events between calendars are not available in the app.
- Local fixed recurrence does not edit iCloud recurrence rules. Existing unsupported calendar content is retained with editing restrictions.
- There is no cloud backup service for local tasks or completion metadata. Unsaved drafts may not be recovered correctly after a crash or restart.

## Privacy

Local tasks, settings and calendar caches use an application-owned SQLite database. Saved app-specific passwords use Windows Credential Manager. Local calendar data is currently not encrypted at rest.

Connecting iCloud exchanges data with Apple's CalDAV service.

Read [Privacy](doc/PRIVACY.md) for details.

## Build from source

Built with TAURI® 2, Rust, Svelte and SQLite.

Prepare a Windows x64 development environment with Node.js 22.12+ and npm, an MSVC Rust toolchain supporting edition 2024, the Visual Studio 2022 C++ desktop tools and Windows SDK, and CMake. Running the app also requires WebView2 Runtime.

Keep the checkout at a short path, such as `D:\src\CalendarTODO`, to avoid Windows path-length issues in native dependencies. From a Visual Studio developer terminal with MSVC and Windows SDK variables loaded, run these commands at the repository root:

```powershell
npm ci --include=dev
npm run check
npm run tauri -- build --target x86_64-pc-windows-msvc --bundles nsis,msi
```

Tauri builds the frontend, application and installers. The default output directory is `src-tauri/target/x86_64-pc-windows-msvc/release/bundle/`; if `CARGO_TARGET_DIR` is set, use that location instead. Building and packaging may download public dependencies and installer tools. No real account, credentials, user database or private configuration is needed.

Use `npm run tauri -- dev` for development mode.

## License and acknowledgements

Original Calendar TODO contributions are licensed under the [MIT License](LICENSE), Copyright (c) 2026 ke4989. The application and installers provide complete license texts for offline viewing. [Microsoft component conditions](src/lib/legal/MICROSOFT_COMPONENT_TERMS.txt) apply only within their stated scope and do not change the MIT licensing of original project contributions. Installation requires explicit agreement to applicable terms; unattended deployment requires the acceptance parameter documented in the [installation guide](doc/INSTALL.en.md). Third-party code, icons, and installer components retain their respective licenses; see the [third-party notices](doc/THIRD_PARTY_NOTICES.md) and the complete [THIRD_PARTY_NOTICES.txt](src/lib/legal/THIRD_PARTY_NOTICES.txt). Installer redistribution must also follow applicable component conditions and source-companion requirements described in the [installation guide](doc/INSTALL.en.md).

Development used [Codex](https://openai.com/codex/), [Pi](https://pi.dev/), and the [impeccable](https://github.com/pbakaus/impeccable) / [UI UX Pro Max](https://github.com/nextlevelbuilder/ui-ux-pro-max-skill) design skills for the UI. Thanks also to [magpie](https://github.com/yetone/magpie) and the free Deepseek credits from [Workbuddy](https://www.workbuddy.cn/events/invite?inviteCode=732a87vsu6y). Their source and license boundaries are recorded in the third-party notices. TAURI is a trademark of The Tauri Programme within the Commons Conservancy. Apple, iCloud, Windows and Microsoft Edge WebView2 names describe compatibility and platform dependencies. Calendar TODO is an independent project and is not endorsed by those organizations.

## Documentation

[简体中文](README.md) · [English](README.en.md) · [Installation](doc/INSTALL.en.md) · [Changelog](doc/CHANGELOG.md) · [Privacy](doc/PRIVACY.md) · [MIT License](LICENSE) · [Third-party notices](doc/THIRD_PARTY_NOTICES.md)
