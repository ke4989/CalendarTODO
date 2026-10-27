import type { LocalTodo, MoveLocalSubtask } from "../ipc/contracts";

export function localSubtaskMoveRequest(todo: LocalTodo, subtaskId: string, direction: "up" | "down"): MoveLocalSubtask | null {
  if (todo.recurrence !== null || todo.occurrenceKey !== "SINGLETON" || !todo.workItemId) return null;
  const index = todo.subtasks.findIndex((item) => item.subtaskId === subtaskId);
  if (index < 0) return null;
  const neighbor = todo.subtasks[index + (direction === "up" ? -1 : 1)];
  if (!neighbor) return null;
  return { todoId: todo.todoId, subtaskId, direction, expectedNeighborId: neighbor.subtaskId };
}

/** UI publication gate only; the actor is authoritative for ownership and adjacency. */
export class LocalSubtaskMoveSession {
  private generation = 0;
  private active: { generation: number; todoId: string; workItemId: string } | null = null;
  private pending = new Set<string>();
  get epoch(): number { return this.generation; }
  get busyIds(): ReadonlySet<string> { return new Set(this.pending); }
  open(todo: LocalTodo) {
    this.generation++;
    this.active = todo.workItemId ? { generation: this.generation, todoId: todo.todoId, workItemId: todo.workItemId } : null;
  }
  close() { this.generation++; this.active = null; }
  async move(
    todo: LocalTodo,
    subtaskId: string,
    direction: "up" | "down",
    port: (input: MoveLocalSubtask) => Promise<LocalTodo>,
    current: () => LocalTodo | null,
    publish: (result: LocalTodo) => void,
    failure: () => void,
    busy: (value: boolean) => void,
  ): Promise<boolean> {
    const request = localSubtaskMoveRequest(todo, subtaskId, direction);
    const context = this.active;
    if (!request || !context || context.todoId !== todo.todoId || context.workItemId !== todo.workItemId || this.pending.has(todo.todoId)) return false;
    this.pending.add(todo.todoId);
    busy(true);
    const valid = () => {
      const latest = current();
      return this.active === context && latest === todo && latest?.todoId === context.todoId &&
        latest.workItemId === context.workItemId && latest.recurrence === null && latest.occurrenceKey === "SINGLETON";
    };
    try {
      const result = await port(request);
      if (!valid()) return false;
      if (result.todoId !== context.todoId || result.workItemId !== context.workItemId ||
          result.recurrence !== null || result.occurrenceKey !== "SINGLETON") throw new Error("mismatch");
      publish(result);
      return true;
    } catch {
      if (valid()) failure();
      return false;
    } finally {
      // No successor for this TODO can start until this operation releases its lock.
      this.pending.delete(todo.todoId);
      busy(false);
    }
  }
}
