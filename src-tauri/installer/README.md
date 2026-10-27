# Windows license presentation

Canonical content lives in `src/lib/legal/`. Run `node scripts/generate-license-bundle.mjs`
after editing it; `--check` rejects drift. Original MIT text stays unchanged.
The generated TXT, RTF and WiX template contain the complete texts, including
third-party license bodies and source-availability notices. The app imports the
canonical files directly and works offline. Release attachments retain the full
texts and the matching covered-source companion. No extra license TXT is installed.

`installer.nsi` and `main.template.wxs` derive from tauri-bundler 2.9.4's
MIT/Apache-2.0 templates (Tauri CLI npm 2.11.4). Retain their upstream notices.
The generated `main.wxs` inlines RTF to bypass the CLI's path-based RTF conversion;
never replace it with the generated nested `wix/LICENSE.rtf`. NSIS reads the RTF
through LicenseData; NSIS 3.11 skips the BOM and sets the RichEdit text limit to
input length before streaming. Actual UI rendering remains an installed check.

NSIS interactive agreement starts unchecked. `/S`, `/P` or `/UPDATE` requires
`/ACCEPTTERMS=1`, explicitly provided after reviewing the complete terms.
MSI uses session-only secure `CT_ACCEPT_TERMS=1`. Execute sequence enforces it
after InstallValidate and before InstallInitialize, including quiet installs,
upgrades and repair. Install and repair navigate through CalendarTermsDlg;
maintenance Remove and complete `/x` bypass terms. There is no persistent
acceptance registry entry. Silent mode alone never means agreement.

The component addendum preserves original MIT and other upstream licenses.
It does not claim Microsoft legal approval or resolve every interpretation of
Community section 4(b). The SDK import-descriptor scope and non-installing
mirror distributor's effective agreement remain stated limits. Check the
corresponding installation guide before redistributing binaries.
