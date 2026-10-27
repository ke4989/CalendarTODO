<script lang="ts">
  import DialogShell from "$lib/components/DialogShell.svelte";
  import Button from "$lib/components/Button.svelte";
  import { translate, type ResolvedLanguage } from "$lib/i18n";
  import mit from "$lib/legal/MIT.txt?raw";
  import microsoft from "$lib/legal/MICROSOFT_COMPONENT_TERMS.txt?raw";
  import thirdParty from "$lib/legal/THIRD_PARTY_NOTICES.txt?raw";

  let { language, close }: { language: ResolvedLanguage; close: () => void } = $props();
  let section = $state("mit");
  const bodies: Record<string, string> = { mit, microsoft, thirdParty };
</script>

<DialogShell id="licenses-title" title={translate(language, "licenses")} closeLabel={translate(language, "close")} {close}>
  <label class="license-category">
    <span>{translate(language, "licenseCategory")}</span>
    <select bind:value={section} data-autofocus>
      <option value="mit">Calendar TODO — MIT License</option>
      <option value="microsoft">{translate(language, "microsoftTerms")}</option>
      <option value="thirdParty">{translate(language, "thirdPartyNotices")}</option>
    </select>
  </label>
  <p class="license-help">{translate(language, "licenseOfflineHint")}</p>
  {#key section}<textarea class="license-body" readonly aria-label={translate(language, "licenseText")} value={bodies[section]}></textarea>{/key}
  {#snippet footer()}<Button variant="primary" type="button" onclick={close}>{translate(language, "close")}</Button>{/snippet}
</DialogShell>

<style>
  .license-category{display:grid;gap:8px;color:var(--color-text);font-size:var(--font-size-body)}
  select{width:100%;min-width:0;min-height:40px;padding:8px;border:1px solid var(--color-border);border-radius:var(--radius-control);color:var(--color-text);background:var(--color-surface)}
  .license-help{margin:12px 0;color:var(--color-text-muted);font-size:var(--font-size-caption);line-height:1.5}
  .license-body{display:block;width:100%;height:min(45vh,420px);min-height:100px;margin:0;padding:8px;box-sizing:border-box;resize:none;border:1px solid var(--color-border);border-radius:var(--radius-control);background:var(--color-surface);white-space:pre-wrap;overflow-wrap:anywhere;user-select:text;font:13px/1.65 var(--font-ui);color:var(--color-text);text-align:left}
</style>
