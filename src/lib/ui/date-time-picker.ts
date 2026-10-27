const two = (value: number) => String(value).padStart(2, "0");

export function localDateKey(date: Date): string {
  return `${String(date.getFullYear()).padStart(4, "0")}-${two(date.getMonth() + 1)}-${two(date.getDate())}`;
}

export function localPickerNow(date: Date, seconds: boolean): string {
  return `${localDateKey(date)}T${two(date.getHours())}:${two(date.getMinutes())}${seconds ? `:${two(date.getSeconds())}` : ""}`;
}

export function offsetPickerDay(key: string, days: number): string {
  const date = new Date(`${key}T12:00:00`);
  date.setDate(date.getDate() + days);
  return localDateKey(date);
}

export function offsetPickerMonth(month: string, months: number): string {
  const date = new Date(`${month}-01T12:00:00`);
  date.setMonth(date.getMonth() + months);
  return localDateKey(date).slice(0, 7);
}

export function pickerCalendarDays(month: string): string[] {
  const first = new Date(`${month}-01T12:00:00`);
  const mondayOffset = (first.getDay() + 6) % 7;
  return Array.from({ length: 42 }, (_, index) => offsetPickerDay(`${month}-01`, index - mondayOffset));
}

export function pickerDateTime(date: string, hour: number | undefined, minute: number | undefined, second?: number): string {
  if (!date || hour === undefined || minute === undefined || !Number.isInteger(hour) || hour < 0 || hour > 23 || !Number.isInteger(minute) || minute < 0 || minute > 59
      || second !== undefined && (!Number.isInteger(second) || second < 0 || second > 59)) return "";
  return `${date}T${two(hour)}:${two(minute)}${second === undefined ? "" : `:${two(second)}`}`;
}

export function offsetPickerTimeUnit(value: number | undefined, amount: number, max: number): number {
  const range = max + 1;
  const current = Number.isInteger(value) ? value as number : 0;
  return ((current + amount) % range + range) % range;
}
