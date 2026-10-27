# Privacy and local data

Calendar TODO stores local tasks, settings, event caches and synchronization state in an application-owned SQLite database under the Windows application-data location. The cache may contain event titles, descriptions, locations and calendar metadata. Local storage is not advertised as encrypted at rest.

When you configure iCloud, the application uses the account information and app-specific password to communicate with Apple's CalDAV service. Saved passwords are handled by Rust and Windows Credential Manager, rather than stored as plaintext password fields in the application database. Credential Manager and operating-system access controls do not protect against every process running with access to your Windows account.

Local tasks are not uploaded to iCloud. Local completion, pinning, tags and presentation settings do not become remote calendar properties. Eligible event fields do synchronize when remote editing is enabled.

The main window performs a public GitHub Release check shortly after initialization, and an explicit update check makes the same request. GitHub receives ordinary request and connection information such as your IP address. No application account credentials are attached to that check. Opening the project or release page uses your browser and its privacy settings.

The application does not include an application-analytics or crash-upload service. This statement does not describe telemetry independently performed by Windows, WebView2, your browser, Apple or GitHub.

The interface runs in Microsoft Edge WebView2. Microsoft Defender SmartScreen is enabled by default in that runtime; it can collect and send end-user information to Microsoft as described in the [Microsoft Privacy Statement](https://aka.ms/privacy) and [Microsoft Edge Privacy Whitepaper](https://learn.microsoft.com/en-us/microsoft-edge/privacy-whitepaper#smartscreen). The WebView2 Runtime is installed and serviced separately and can check for, download and install its own updates. These Microsoft runtime updates are separate from Calendar TODO’s manual application updates. Calendar TODO v0.1.0 requires the Runtime to be installed beforehand. Its NSIS and MSI installers do not download, bundle or install the Runtime or its bootstrapper. If it is missing, use [Microsoft's official WebView2 page](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) to follow Microsoft's installation process and review the terms Microsoft presents. That separate process is governed by Microsoft's terms and privacy practices; Calendar TODO does not assert that you have accepted them.

Failures can be written to a local error log, viewable and clearable in Settings. Each record has bounded action, code and message fields, but the file itself is append-only and is not currently rotated or capped by size; normal successful operations are not logged there. An optional desktop-window trace, enabled only by an explicit environment variable, records window handles, process IDs, window class names and state transitions locally with a 512 KiB cap. Neither is sent to a server. Review any diagnostics before sharing them.

Treat the database, its sidecar files, backups and calendar exports as private. Do not upload them to public issues. Do not copy a live database or remove its ownership lock as a backup procedure. Use only a supported application backup method when one is made available.

Removing a connection does not necessarily erase cached data or historical local records. Uninstalling Calendar TODO may also leave local data and saved credentials on the device.

[README](../README.en.md) · [Installation](INSTALL.en.md) · [Third-party notices](THIRD_PARTY_NOTICES.md)

License viewing is fully offline. The installers do not send or separately persist agreement records. Normal Windows Installer deployment logs may contain command-line properties. / 许可查看完全离线；安装器不发送或另存同意记录，正常 Windows Installer 部署日志可能记录命令属性。
