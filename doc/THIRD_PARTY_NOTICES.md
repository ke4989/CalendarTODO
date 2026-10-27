# Third-party notices

Original Calendar TODO contributions are licensed under the [MIT License](../LICENSE), Copyright (c) 2026 ke4989. Third-party material retains its own copyrights and licenses.

## Complete license texts

[THIRD_PARTY_NOTICES.txt](../src/lib/legal/THIRD_PARTY_NOTICES.txt) is the complete checked-in notice for the Windows application and installers. It contains the component/version index, individual copyright statements, full license and exception texts, upstream NOTICE material, and source-availability information. The application embeds the full text for offline reading, selection and copying in Settings → About & Support → Open-source Licenses & Third-party Notices. Both installers include it in the complete offline terms shown before agreement. Additional TXT files are not installed. Keep the complete notices with redistributed binaries, including the matching release attachments; this Markdown index does not replace them.

The notice covers 297 dependency entries: the Windows Rust normal-dependency closure, including proc-macro dependencies, plus the four packages emitted into the static frontend: `@sveltejs/kit`, `@tauri-apps/api`, `clsx` and `svelte`. This is conservative notice coverage: linker elimination can remove code, and proc macros generate code during compilation rather than run inside the installed app. A package's npm development-dependency flag alone does not determine whether its code is emitted.

## Source repository

This repository contains application source, manifests, lock files and project assets. It does not contain the downloaded Cargo registry, `node_modules`, bundled third-party fonts or generated build output. Dependencies installed by Cargo and npm keep the terms supplied in their published packages. Lock-file entries for other platforms and tools used only to build the application are not a list of components shipped in the Windows app.

The [Cargo lock file](../src-tauri/Cargo.lock) and [npm lock file](../package-lock.json) record dependency versions. Build tools include Vite, Rolldown, TypeScript and the Tauri CLI; obtain their license material with the exact packages when installing or redistributing those tools. Redistributing a dependency tree or targeting another platform requires notices for that distribution.

Adapted inline icons retain the Lucide ISC and Feather MIT copyrights, and adapted Tauri scaffolding retains the Tauri MIT copyright. Their complete notices are included under **Additional source and artwork attributions** in the text file. Lucide 0.468.0 and Feather 4.29.2 identify the reference license snapshots; the exact originally copied revision of each adapted glyph is not established. The Calendar TODO logo and its icon/favicon conversions are project artwork. Software licenses do not grant third-party trademark rights.

The development and design tools acknowledged in the README are not bundled in this repository or the application. Using their advice does not by itself incorporate their implementation or relicense the application's contributions.

## Windows application and installer components

| Component or material | License and distribution requirement |
| --- | --- |
| Rust dependencies and emitted frontend packages | Full per-component copyrights, license alternatives and any combined terms remain in the text notice |
| AWS-LC and its embedded upstream code | Complete combined upstream license/NOTICE material is retained |
| Unicode data and supporting crates | Unicode copyright and terms are retained |
| MPL-2.0 dependencies | Used unmodified; exact-version source links and full MPL terms are retained; the matching source companion also includes their source archives |
| NSIS 3.11 engine and plugins | Upstream notices are retained, including the LZMA CPL-1.0 terms and linking exception |
| nsis-tauri-utils 0.5.3 | Full MIT notice is retained |
| WiX 3.14.1 installer components | MS-RL terms and applicable source obligations are retained |
| Microsoft WebView2 SDK static loader | Full BSD-3-Clause license and exact SDK package NOTICE are retained; package-wide NOTICE references do not imply every named assembly is bundled |

Installer redistribution must include the matching `CalendarTODO-installer-sources-0.1.0.zip` at the same download location, at no additional charge. That companion supplies exact installer-component sources, applicable licenses, generated installer inputs and component correspondence, as well as the application build source. Calendar TODO offers no differing terms for covered installer components. See the [installation guide](INSTALL.en.md#updates-and-notices).

## Separate Microsoft components

WebView2 Runtime is installed and serviced separately under Microsoft's terms. Calendar TODO's NSIS and MSI installers use skip mode: they do not download, bundle or install the Runtime or its bootstrapper. Use [Microsoft's official WebView2 page](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) if it is missing. Runtime privacy and independent updates are described in [Privacy](PRIVACY.md).

The [Microsoft component conditions](../src/lib/legal/MICROSOFT_COMPONENT_TERMS.txt) apply only to the Microsoft Distributable Code within their stated scope. Original contributions remain MIT. End users and distributors must review the applicable conditions; a mirror distributor must arrange downstream agreement as well. This project does not claim Microsoft legal approval or that installer agreement alone establishes compliance.

[简体中文](../README.md) · [English](../README.en.md) · [Installation](INSTALL.en.md) · [Privacy](PRIVACY.md)
