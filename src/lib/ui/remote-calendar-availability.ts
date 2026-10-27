import type { AttachmentView, CalendarAgendaItem, CalendarSettingsView } from "$lib/ipc/contracts";
import { remoteText } from "$lib/i18n";

const blockedReasonEnglish: Record<string, string> = {
  "日历当前不可用": "This calendar is currently unavailable.",
  "请先启用账户": "Enable the account first.",
  "请先配置账户凭据": "Configure the account credentials first.",
  "请在设置中允许远程日历编辑": "Allow remote calendar editing in Settings first.",
  "请先在设置中显示此日历": "Show this calendar in Settings first.",
  "此日历没有写入权限": "This calendar does not grant write access.",
  "此账户或日历不支持安全同步": "This account or calendar does not support safe synchronization.",
  "日历地址不符合当前 iCloud 来源策略": "This calendar address does not match the current iCloud source policy.",
  "此事件仅可查看": "This event is read-only.",
  "邀请、重复、全天或不支持的事件仅可查看": "Invitations, recurring, all-day, or unsupported events are read-only.",
  "此事件有待完成的同步操作": "This event already has a pending synchronization operation."
};

function localizedBlockedReason(value: string | null | undefined, fallback: string): string {
  if (!value) return fallback;
  return remoteText(value, blockedReasonEnglish[value] ?? "Remote editing is unavailable for this item.");
}

/** Presentation only. Rust independently qualifies the destination and live owner. */
export function remoteCalendarBlockedReason(calendar: CalendarSettingsView | undefined, _attachments: ReadonlyMap<string, AttachmentView>, item?: CalendarAgendaItem): string | null {
  if (!calendar) return remoteText("calendarUnavailable");
  if (item) return item.remoteEditEligible ? null : localizedBlockedReason(item.remoteEditBlockedReason, remoteText("eventReadOnly"));
  return calendar.remoteCreateEligible ? null : localizedBlockedReason(calendar.remoteCreateBlockedReason, remoteText("calendarReadOnly"));
}
