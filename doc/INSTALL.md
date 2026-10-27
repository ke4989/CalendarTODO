[English](INSTALL.en.md) · **简体中文** · [返回项目首页](../README.md)

# 在 Windows 上安装 Calendar TODO

Calendar TODO v0.1.0 面向 Windows x64，需要预先安装 **Microsoft Edge WebView2 Runtime**。不应假定每台 Windows 10／11 设备都已具备该 Runtime；仅安装了 Microsoft Edge 浏览器也不能可靠地证明 Runtime 可用。

## 安装前

1. 从项目 [Releases 页面](https://github.com/ke4989/CalendarTODO/releases)获取对应版本的 x64 NSIS 安装包（`-setup.exe`）或 MSI，以及 `SHA256SUMS.txt`。下载以发布页实际提供的文件为准。
2. 核对安装包的 SHA-256。例如在 PowerShell 中运行以下命令，将输出与同一版本校验清单中对应文件的记录比较；使用 MSI 时替换文件名：

   ```powershell
   Get-FileHash -LiteralPath '.\Calendar TODO_0.1.0_x64-setup.exe' -Algorithm SHA256
   ```

3. 确认 WebView2 Runtime 已安装。若缺失，请打开[微软官方 WebView2 页面](https://developer.microsoft.com/en-us/microsoft-edge/webview2/)，按微软指引安装 Runtime，并查看微软提供的条款。受管理的设备可能需要管理员或组织 IT 支持。

Calendar TODO v0.1.0 的 NSIS 与 MSI 安装包跳过 WebView2 安装：不下载、代理、镜像、捆绑或执行 Runtime 安装程序及 bootstrapper。项目不提供替代微软的许可文本，也不声称你已接受微软条款。Runtime 的安装和维护流程由微软独立提供。

## 安装与启动

选择一种 Calendar TODO 安装包并按提示完成安装。即使 Calendar TODO 安装成功，启动界面仍需要 Runtime。安装前须阅读完整离线条款，主动勾选默认未选中的同意选项；可取消安装。升级和 MSI 修复须重新同意，完整卸载不受此限制。原创部分仍为 MIT，Microsoft 附加条件仅限其声明范围。

安装后启动 Calendar TODO。Runtime 可用时，应能打开主界面。关闭主窗口会收起到系统托盘；要完全退出，请在托盘菜单选择“退出”。

## 缺少 WebView2 Runtime 时

没有 Runtime，Calendar TODO 无法显示界面。安装可能仍能完成，但启动时可能失败或出现平台提供的错误提示；具体提示和退出表现取决于 Windows 及可用组件，不保证有专用的缺 Runtime 对话框或错误码。

关闭启动失败的应用实例，通过[微软官方页面](https://developer.microsoft.com/en-us/microsoft-edge/webview2/)完成 Runtime 安装，再重新启动 Calendar TODO。Calendar TODO 安装器不会替你获取或修复 Runtime，请勿使用非官方镜像。

## 卸载

先从托盘完全退出 Calendar TODO，再到 Windows **设置 → 应用 → 已安装的应用**（或对应版本的卸载界面）选择 Calendar TODO，按提示卸载。

卸载 Calendar TODO 与卸载 WebView2 Runtime 是两件事；其他应用可能共用 Runtime。当前版本不保证卸载时清除所有本地待办、日历缓存、凭据或设置。在转交设备或分享应用文件前，请阅读[隐私说明（英文）](PRIVACY.md)。

## 更新与第三方声明

Calendar TODO 更新需要手动下载安装。WebView2 Runtime 可按微软条款和策略独立更新，详见[隐私说明（英文）](PRIVACY.md)。

完整文本内嵌于“设置 → 关于与支持 → 开源许可证与第三方声明”，可离线阅读、选择和复制；安装器也在安装前提供全部文本。不额外安装许可 TXT。发行附件保留 LICENSE、MICROSOFT_COMPONENT_TERMS.txt 和 THIRD_PARTY_NOTICES.txt。重新分发须保留完整声明和匹配的 `CalendarTODO-installer-sources-0.1.0.zip`，在相同下载位置免费提供，并履行 Microsoft 代码的下游同意要求。

[项目首页](../README.md) · [English installation guide](INSTALL.en.md) · [隐私说明（英文）](PRIVACY.md) · [第三方声明](THIRD_PARTY_NOTICES.md)

## Unattended deployment / 静默部署

本版本安装包未进行代码签名。静默部署、同版本及跨版本升级、修复和卸载路径已实现并有静态检查证据，完整实际回归安排在首次发布后；以下参数说明不代表这些路径已全部人工验收。

Review all offline terms in `src-tauri/installer/LICENSES.txt` or the matching release attachments before deployment. A silent flag is not agreement. Use these explicit parameters only after agreement:

```powershell
& '.\Calendar TODO_0.1.0_x64-setup.exe' /S /ACCEPTTERMS=1
msiexec.exe /i '.\Calendar TODO_0.1.0_x64_en-US.msi' /qn CT_ACCEPT_TERMS=1
# Explicit repair: same consent requirement
msiexec.exe /fa '.\Calendar TODO_0.1.0_x64_en-US.msi' /qn CT_ACCEPT_TERMS=1
```

NSIS `/P` and `/UPDATE` also require `/ACCEPTTERMS=1`. Missing acceptance refuses unattended installation before product writes. MSI `/qn` and passive execution require `CT_ACCEPT_TERMS=1`, including repair. Use the zh-CN MSI for Chinese Windows Installer UI. Neither package stores a persistent acceptance record. Windows Installer's normal deployment logs may record command-line properties. Complete uninstall (`REMOVE=ALL` or normal `/x`) requires no acceptance parameter. Do not mistake installation of the app for installation or acceptance of the separately supplied WebView2 Runtime.

静默、被动或自动部署必须先阅读完整离线条款，再显式传入上述同意参数；未传入时拒绝安装。升级/修复不能用旧同意状态绕过要求。安装器不另存长期同意记录；正常 Windows Installer 部署日志可能记录命令属性。完整卸载不要求同意参数。
