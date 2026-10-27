[简体中文](INSTALL.md) · **English** · [Back to README](../README.en.md)

# Install Calendar TODO on Windows

Calendar TODO v0.1.0 targets Windows x64 and requires **Microsoft Edge WebView2 Runtime**. Availability on every Windows 10 or Windows 11 device is not guaranteed. Installing Microsoft Edge alone is not a reliable way to confirm that the required WebView2 Runtime is available.

## Before installation

1. Obtain the x64 Calendar TODO NSIS installer (`-setup.exe`) or MSI and its `SHA256SUMS.txt` from the project's [Releases page](https://github.com/ke4989/CalendarTODO/releases). Use the files actually offered there.
2. Compare the installer's SHA-256 with the matching release record. For example, run this PowerShell command and compare the result with the entry for that filename in `SHA256SUMS.txt`; substitute the MSI filename if needed:

   ```powershell
   Get-FileHash -LiteralPath '.\Calendar TODO_0.1.0_x64-setup.exe' -Algorithm SHA256
   ```

3. Ensure Microsoft Edge WebView2 Runtime is installed. If it is missing, go to [Microsoft's official WebView2 page](https://developer.microsoft.com/en-us/microsoft-edge/webview2/), follow Microsoft's Runtime installation instructions and review the terms Microsoft presents. Managed devices may require an administrator or your organization's IT support.

Calendar TODO's v0.1.0 NSIS and MSI installers skip WebView2 installation. They do not download, mirror, bundle or execute the Runtime installer or bootstrapper. Calendar TODO does not supply replacement Microsoft license text or assert that you have accepted Microsoft's terms. Microsoft's installation and servicing processes are separate from this application's installer.

## Install and start

Run one Calendar TODO installer and follow its instructions. The application requires the Runtime even if Calendar TODO's own installation completes successfully. Before installation, read the complete offline license texts and explicitly select the unchecked agreement control. You can cancel without installing. An upgrade uses the same agreement step. MSI repair also requires agreement; complete uninstall does not. The original project license remains MIT; Microsoft component conditions apply only to their stated Microsoft code.

Start Calendar TODO after installation. The main interface should open when the Runtime is available. The main window closes to the system tray; choose **Quit** from the tray menu to exit completely.

## If WebView2 Runtime is missing

Calendar TODO cannot display its interface without the Runtime. Installation may still finish, but application startup may fail or show a platform-provided error. The exact message and exit behavior depend on Windows and the available platform components; this release does not promise a custom missing-Runtime dialog or error code.

Close any failed application instance, follow the Runtime installation instructions on [Microsoft's official page](https://developer.microsoft.com/en-us/microsoft-edge/webview2/), then start Calendar TODO again. Calendar TODO's installer will not fetch or repair the Runtime for you. Do not download a Runtime installer from an unofficial mirror.

## Uninstall

Quit Calendar TODO from the tray, then use Windows **Settings > Apps > Installed apps** (or the equivalent Windows uninstall interface) to uninstall Calendar TODO. Select the entry corresponding to the package you installed.

Removing Calendar TODO is separate from removing Microsoft Edge WebView2 Runtime, which may be shared by other applications. This release does not promise that uninstalling Calendar TODO erases local tasks, cached calendar data, credentials or settings; read [PRIVACY.md](PRIVACY.md) before disposing of a device or sharing application files.

## Updates and notices

Calendar TODO application updates require manual download and installation. WebView2 Runtime can receive Microsoft's independent updates under Microsoft's terms and policies. See [PRIVACY.md](PRIVACY.md).

Read the full offline texts in Settings → About & Support → Open-source Licenses & Third-party Notices, or in the installer before installation. No additional license TXT files are installed. The release also provides LICENSE, MICROSOFT_COMPONENT_TERMS.txt and THIRD_PARTY_NOTICES.txt. The [source texts](../src/lib/legal/) are the canonical content shared by the app and installers. If you redistribute the installers, retain the matching `CalendarTODO-installer-sources-0.1.0.zip` and applicable licenses alongside them at the same distribution location without additional charge. The project MIT license does not replace third-party terms or Microsoft's platform conditions.

[README](../README.en.md) · [简体中文安装指南](INSTALL.md) · [Privacy](PRIVACY.md) · [Third-party notices](THIRD_PARTY_NOTICES.md)

## Unattended deployment / 静默部署

These installers are not code-signed. Unattended deployment, same-version and cross-version upgrades, repair and uninstall paths are implemented with static-check evidence; complete execution regression is planned after the first release. The parameter documentation below does not claim that every path has passed manual acceptance.

Review all offline terms in `src-tauri/installer/LICENSES.txt` or the matching release attachments before deployment. A silent flag is not agreement. Use these explicit parameters only after agreement:

```powershell
& '.\Calendar TODO_0.1.0_x64-setup.exe' /S /ACCEPTTERMS=1
msiexec.exe /i '.\Calendar TODO_0.1.0_x64_en-US.msi' /qn CT_ACCEPT_TERMS=1
# Explicit repair: same consent requirement
msiexec.exe /fa '.\Calendar TODO_0.1.0_x64_en-US.msi' /qn CT_ACCEPT_TERMS=1
```

NSIS `/P` and `/UPDATE` also require `/ACCEPTTERMS=1`. Missing acceptance refuses unattended installation before product writes. MSI `/qn` and passive execution require `CT_ACCEPT_TERMS=1`, including repair. Use the zh-CN MSI for Chinese Windows Installer UI. Neither package stores a persistent acceptance record. Windows Installer's normal deployment logs may record command-line properties. Complete uninstall (`REMOVE=ALL` or normal `/x`) requires no acceptance parameter. Do not mistake installation of the app for installation or acceptance of the separately supplied WebView2 Runtime.

静默、被动或自动部署必须先阅读完整离线条款，再显式传入上述同意参数；未传入时拒绝安装。升级/修复不能用旧同意状态绕过要求。安装器不另存长期同意记录；正常 Windows Installer 部署日志可能记录命令属性。完整卸载不要求同意参数。
