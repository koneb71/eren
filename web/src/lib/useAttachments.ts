import { useCallback, useEffect, useRef, useState } from "react";
import {
  api,
  Attachment,
  ATTACHMENT_ACCEPT,
  MAX_ATTACHMENTS,
  MAX_ATTACHMENT_BYTES,
} from "./api";

/** A file in the composer: uploading, uploaded, or rejected. */
export interface PendingAttachment {
  localId: string;
  name: string;
  size: number;
  status: "uploading" | "ready" | "error";
  remote?: Attachment;
  error?: string;
  /** Object URL for image previews. Revoked on removal and unmount. */
  previewUrl?: string;
}

const ALLOWED_EXTS = ATTACHMENT_ACCEPT.split(",").map((e) => e.slice(1));

function extOf(name: string): string {
  const i = name.lastIndexOf(".");
  return i === -1 ? "" : name.slice(i + 1).toLowerCase();
}

/** Same rules the server enforces, applied early so obvious rejects cost no round trip. */
function preCheck(file: File): string | null {
  if (!ALLOWED_EXTS.includes(extOf(file.name))) {
    return `${file.name}: unsupported type`;
  }
  if (file.size > MAX_ATTACHMENT_BYTES) {
    return `${file.name} is larger than ${MAX_ATTACHMENT_BYTES / 1024 / 1024} MB`;
  }
  return null;
}

/**
 * Composer attachment state, shared by the chat panel and the new-task modal.
 *
 * Uploads happen immediately and per-file, so one rejection never poisons the
 * batch and the ids are ready by the time the user submits. They belong to the
 * project, or — for a general chat, which has none — to `workspaceId`; the
 * server refuses to let either be claimed anywhere else.
 */
export function useAttachments(projectId: string, workspaceId?: string) {
  const [items, setItems] = useState<PendingAttachment[]>([]);
  const [dragging, setDragging] = useState(false);
  // Read by the unmount cleanup, which must not re-run when items change.
  const itemsRef = useRef<PendingAttachment[]>([]);
  itemsRef.current = items;

  const revoke = (item: PendingAttachment) => {
    if (item.previewUrl) URL.revokeObjectURL(item.previewUrl);
  };

  // Everything with a side effect — the preview URL, the upload — happens out
  // here, never inside a state updater: StrictMode runs updaters twice, which
  // uploaded every file twice and leaked one preview URL per file. The room
  // left is read from the ref, which is moved forward at once so a second drop
  // in the same tick sees this one's files.
  const add = useCallback(
    (incoming: FileList | File[]) => {
      const files = Array.from(incoming);
      if (!files.length) return;

      const room = MAX_ATTACHMENTS - itemsRef.current.length;
      const accepted = files.slice(0, Math.max(0, room));
      if (!accepted.length) return;

      const settle = (localId: string, patch: Partial<PendingAttachment>) =>
        setItems((cur) => cur.map((i) => (i.localId === localId ? { ...i, ...patch } : i)));

      const next = accepted.map((file): PendingAttachment => {
        const localId = `${file.name}-${file.size}-${crypto.randomUUID()}`;
        const problem = preCheck(file);
        const previewUrl = file.type.startsWith("image/")
          ? URL.createObjectURL(file)
          : undefined;

        if (!problem) {
          (projectId
            ? api.uploadAttachments(projectId, [file])
            : api.uploadWorkspaceAttachments(workspaceId ?? "", [file])
          )
            .then((r) => settle(localId, { status: "ready", remote: r.attachments[0] }))
            .catch((e) => settle(localId, { status: "error", error: String(e) }));
        }

        return {
          localId,
          name: file.name,
          size: file.size,
          status: problem ? "error" : "uploading",
          error: problem ?? undefined,
          previewUrl,
        };
      });
      itemsRef.current = [...itemsRef.current, ...next];
      setItems((prev) => [...prev, ...next]);
    },
    [projectId, workspaceId],
  );

  const remove = useCallback((localId: string) => {
    const gone = itemsRef.current.find((i) => i.localId === localId);
    if (gone) {
      revoke(gone);
      // Best effort: an already-claimed row 409s, which is fine.
      if (gone.remote) api.deleteAttachment(gone.remote.id).catch(() => {});
    }
    itemsRef.current = itemsRef.current.filter((i) => i.localId !== localId);
    setItems((prev) => prev.filter((i) => i.localId !== localId));
  }, []);

  /** After a successful submit: the rows are claimed, so only drop local state. */
  const clear = useCallback(() => {
    itemsRef.current.forEach(revoke);
    itemsRef.current = [];
    setItems([]);
  }, []);

  // Taken out of the composer for a submit that has not answered yet. Kept
  // aside rather than dropped, so a refused submit can put them back with
  // their previews intact.
  const taken = useRef<PendingAttachment[]>([]);

  /** Empty the composer for a submit, keeping the files for `restore`. */
  const take = useCallback((): PendingAttachment[] => {
    const out = itemsRef.current;
    taken.current = [...taken.current, ...out];
    itemsRef.current = [];
    setItems([]);
    return out;
  }, []);

  /** The submit was refused: the files go back in front of anything added since. */
  const restore = useCallback((back: PendingAttachment[]) => {
    if (!back.length) return;
    taken.current = taken.current.filter((i) => !back.includes(i));
    itemsRef.current = [...back, ...itemsRef.current];
    setItems((prev) => [...back, ...prev.filter((i) => !back.includes(i))]);
  }, []);

  /** The submit landed: the rows are claimed, so only the previews go. */
  const release = useCallback((done: PendingAttachment[]) => {
    taken.current = taken.current.filter((i) => !done.includes(i));
    done.forEach(revoke);
  }, []);

  // Discard anything still unclaimed when the composer goes away. Best effort
  // only — the server-side sweeper is the real backstop.
  useEffect(() => {
    return () => {
      itemsRef.current.forEach((item) => {
        revoke(item);
        if (item.remote) api.deleteAttachment(item.remote.id).catch(() => {});
      });
      // Mid-submit: whether they were claimed is not known yet, so only the
      // previews go and the sweeper decides about the rows.
      taken.current.forEach(revoke);
    };
  }, []);

  const onPaste = useCallback(
    (e: React.ClipboardEvent) => {
      const files = Array.from(e.clipboardData?.files ?? []);
      // Only swallow the event when there is actually a file — otherwise
      // pasting text into the composer would stop working.
      if (!files.length) return;
      e.preventDefault();
      add(files);
    },
    [add],
  );

  const dropProps = {
    onDragOver: (e: React.DragEvent) => {
      if (!e.dataTransfer?.types.includes("Files")) return;
      e.preventDefault();
      setDragging(true);
    },
    onDragLeave: (e: React.DragEvent) => {
      // Ignore bubbling from children, or the overlay flickers.
      if (e.currentTarget.contains(e.relatedTarget as Node)) return;
      setDragging(false);
    },
    onDrop: (e: React.DragEvent) => {
      if (!e.dataTransfer?.files.length) return;
      e.preventDefault();
      setDragging(false);
      add(e.dataTransfer.files);
    },
  };

  return {
    items,
    /** Ids ready to submit. */
    ids: items.filter((i) => i.remote).map((i) => i.remote!.id),
    /** True while any upload is still in flight — submit should wait. */
    busy: items.some((i) => i.status === "uploading"),
    full: items.length >= MAX_ATTACHMENTS,
    add,
    remove,
    clear,
    take,
    restore,
    release,
    onPaste,
    dropProps,
    dragging,
  };
}
