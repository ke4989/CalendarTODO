import { invoke } from "@tauri-apps/api/core";
import type { SearchHit } from "./contracts";
export type SearchField = "title" | "notes" | "location" | "tags";
export type SearchDirection = "before" | "after" | "noTime";
export interface SearchPreferences { fields: SearchField[]; includeCompleted: boolean }
export type SearchIdentity = { kind: "remoteOnly"; sessionId: string } | { kind: "todo"; todoId: string; occurrenceKey: string } | { kind: "calendar"; seriesId: string; resourceId: string; recurrenceKey: string };
export interface SearchPosition { time: number; key: string }
export interface SearchCursor { position: SearchPosition; revision: number; fingerprint: string; direction: SearchDirection }
export interface WorkspaceQuery { text: string; preferences: SearchPreferences; anchorUtcMs: number; anchorKey?: string | null; direction: SearchDirection; cursor: SearchCursor | null }
export interface SearchSummary { key: string; identity: SearchIdentity; calendarId: string; calendarName: string; calendarColor: string | null; title: string; notes: string | null; location: string | null; tags: string[]; startAtUtcMs: number | null; endAtUtcMs: number | null; completed: boolean }
export interface SearchPage { items: SearchSummary[]; continuation: SearchCursor | null; exhausted: boolean; stale: boolean; revision: number; scanned: number }
export interface RangeInput { startAtUtcMs: number; endAtUtcMs: number }
export interface RangeView { taskId: string; state: string; completed: number; total: number; unsupported: number; range: RangeInput }
export const searchRemoteStart = (input: RangeInput): Promise<RangeView> => invoke("search_remote_start",{ input });
export const searchRemoteStep = (taskId: string): Promise<RangeView> => invoke("search_remote_step",{ taskId });
export const searchRemoteCancel = (): Promise<void> => invoke("search_remote_cancel");
export const searchRemoteDetail = (sessionId: string): Promise<SearchSummary> => invoke("search_remote_detail",{ sessionId });
export const searchWorkspacePage = (query: WorkspaceQuery): Promise<SearchPage> => invoke("search_workspace_page", { query });
export const searchWorkspaceDetail = (identity: SearchIdentity): Promise<SearchHit> => invoke("search_workspace_detail", { identity });
export const searchWorkspacePreferences = (update: SearchPreferences | null = null): Promise<SearchPreferences> => invoke("search_workspace_preferences", { update });
