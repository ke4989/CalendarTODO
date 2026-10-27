import type { RecurrenceFrequency, RecurrenceOptions } from "$lib/ipc/contracts";
import type { ResolvedLanguage } from "$lib/i18n";

export type RecurrenceChoice = "none" | RecurrenceFrequency;

export function defaultRecurrenceOptions(): RecurrenceOptions {
  return { timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC", weekdays: [], monthDays: [], yearMonths: [], endKind: "count", endCount: 100, endUntilAtUtcMs: null };
}

export function copyRecurrenceOptions(options: RecurrenceOptions): RecurrenceOptions {
  return { ...options, weekdays: [...options.weekdays], monthDays: [...options.monthDays], yearMonths: [...options.yearMonths] };
}

export function recurrenceInput(frequency: RecurrenceChoice, interval: number, options: RecurrenceOptions) {
  return frequency === "none" ? null : { frequency, interval: Number(interval), options: copyRecurrenceOptions(options) };
}

export function recurrenceChanged(next: ReturnType<typeof recurrenceInput>, previous: { frequency: RecurrenceFrequency; interval: number; options?: RecurrenceOptions } | null) {
  return !!next !== !!previous || next?.frequency !== previous?.frequency || next?.interval !== previous?.interval || JSON.stringify(next?.options) !== JSON.stringify(previous?.options);
}

export function recurrenceSummary(language: ResolvedLanguage, frequency: RecurrenceChoice, interval: number): string {
  const zh = language === "zh-CN";
  if (frequency === "none") return zh ? "不重复" : "Does not repeat";
  const labels: Record<RecurrenceFrequency, string> = zh
    ? { daily: "每天", weekly: "每周", monthly: "每月", yearly: "每年", customWeekly: "自定义星期", customMonthly: "自定义月日", customYearly: "自定义月份" }
    : { daily: "Daily", weekly: "Weekly", monthly: "Monthly", yearly: "Yearly", customWeekly: "Custom weekdays", customMonthly: "Custom month days", customYearly: "Custom months" };
  return interval > 1 ? `${labels[frequency]} · ${zh ? `每 ${interval} 个周期` : `every ${interval}`}` : labels[frequency];
}
