import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { motion } from "framer-motion";
import {
  api,
  Agent,
  ChatMessage,
  ChatSummary,
  Effort,
  OpenQuestion,
  Skill,
  Tier,
} from "../../lib/api";
import { useRunStream } from "../../lib/ws";
import { useAttachments } from "../../lib/useAttachments";
import { agentSpans } from "../../lib/mention";
import { AttachmentBar, AttachmentList } from "../AttachmentBar";
import { ComposerSettings } from "./ComposerSettings";
import { useMentionPicker } from "../MentionPicker";
import { ArticlePicker } from "../kb/ArticlePicker";
import { QuestionCard } from "./QuestionCard";
import { Markdown } from "../Markdown";
import { Button } from "../ui/Button";
import { toolName } from "../../lib/brand";

/**
 * One conversation: the scroller, the live run, and the composer.
 *
 * Extracted from ChatPanel so the same thread can be the project page's
 * 380px rail and the Chat page's full-width column. It renders a fragment on
 * purpose — the rail's root div and header stay in ChatPanel, so the rail's
 * DOM is exactly what it was before the extraction.
 *
 * Whose state is whose: the thread owns everything about *this conversation*
 * (messages, the live run, the draft, the composer settings). The caller owns
 * which conversation is open and what the list of conversations looks like —
 * which is why title derivation is reported back through `onSent` rather than
 * refreshed here.
 */
export function ChatThread({
  projectId,
  workspaceId,
  projectKind,
  chatId,
  chat,
  onSent,
  centered,
}: {
  /** Null for a *general* chat — no project, no repo, no board. The `@` file
   *  picker is project machinery and disappears with it; attachments go to
   *  the workspace instead. */
  projectId: string | null;
  /**
   * The workspace this *project* belongs to — not whichever one the sidebar is
   * showing. The server resolves `@mentions` against the project's workspace,
   * so resolving them here against any other list would offer agents that
   * cannot bind and draw chips for mentions that did not.
   */
  workspaceId?: string;
  /** "repo" | "app" | "space". Plan mode is only offered where there is
   *  something to act *on* — a space chat's tools are all read-only, so a
   *  toggle there would be a control that does nothing. */
  projectKind?: string;
  chatId: string | null;
  /** The open chat's summary, for seeding tier/effort. */
  chat?: ChatSummary;
  /** The first message names the chat server-side — the caller's cue to
   *  refresh its list and pick the title up. */
  onSent?: () => void;
  /** Cap the content width for a full-page mount. Off by default, which is
   *  the rail's original layout. */
  centered?: boolean;
}) {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [activeRunId, setActiveRunId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);
  // null = the machine default. Switching mid-chat starts a fresh session,
  // because a session id only means something to the CLI that minted it.
  const [engine, setEngine] = useState<string | null>(null);
  // Seeded from the chat once it loads, then owned here — see the composer.
  const [tier, setTier] = useState<Tier>("medium");
  // Empty means "resolve from the tier", which is what the server stores as
  // NULL and what clearing the box returns to.
  const [modelId, setModelId] = useState("");
  const [effort, setEffort] = useState<Effort | null>(null);
  // The agent library, fetched once. Both the `@` picker and the message
  // bubbles resolve names against this one list, so a chip is only ever drawn
  // for an agent that exists.
  const [agents, setAgents] = useState<Agent[]>([]);
  // Skills share that `@` namespace, so they are offered from the same picker
  // and drawn as the same chip. Only the enabled ones: a switched-off skill
  // binds to nothing on the server, and offering it would promise otherwise.
  const [skills, setSkills] = useState<Skill[]>([]);
  // Knowledge-base pages to put in front of the assistant for the next turn.
  // Per-message, like a file attachment and unlike the tier — a runbook is
  // chosen for a question, and one that stuck to the chat would be pasted
  // into every later turn of a conversation that has moved on.
  // Ids for the wire, titles for the optimistic bubble — the persisted message
  // carries them back, but not for the ~2.5s until the next poll.
  const [articleIds, setArticleIds] = useState<string[]>([]);
  const [articleChips, setArticleChips] = useState<Array<{ id: string; title: string }>>([]);
  // Propose rather than act. Sticks to the conversation, like the tier — and
  // is turned off by approving a plan, because carrying it out is the thing
  // plan mode is for not doing.
  const [planMode, setPlanMode] = useState(false);
  // The assistant's clarifying question, if it asked one. Read back from the
  // server on every poll rather than held here — that is what makes it
  // survive a refresh, and what makes it disappear the moment it is answered.
  const [openQuestion, setOpenQuestion] = useState<OpenQuestion | null>(null);
  const canPlan = projectId !== null && projectKind !== "space";
  const scrollRef = useRef<HTMLDivElement>(null);
  const streamEvents = useRunStream(activeRunId);
  const general = projectId === null;
  // A general chat's uploads belong to its workspace; without one to name
  // there is nowhere for them to go, and the attach button stays hidden.
  const att = useAttachments(projectId ?? "", workspaceId);
  const canAttach = !general || !!workspaceId;
  const composerRef = useRef<HTMLTextAreaElement>(null);
  // Caret is tracked separately: it moves on click and arrow keys, not just
  // on change, and the mention token depends on where it is.
  const [caret, setCaret] = useState(0);
  const mention = useMentionPicker({
    projectId: projectId ?? "",
    agents,
    skills,
    text: draft,
    caret,
    onApply: (text, nextCaret) => {
      setDraft(text);
      setCaret(nextCaret);
      // The textarea is uncontrolled w.r.t. selection, so place it by hand
      // after React has written the new value.
      requestAnimationFrame(() => {
        composerRef.current?.setSelectionRange(nextCaret, nextCaret);
        composerRef.current?.focus();
      });
    },
  });

  useEffect(() => {
    if (!workspaceId) return;
    setAgents([]); // a stale library would resolve mentions against the wrong workspace
    setSkills([]);
    api.agents(workspaceId).then((r) => setAgents(r.agents)).catch(() => {});
    api
      .skills(workspaceId)
      .then((r) => setSkills(r.skills.filter((s) => s.enabled)))
      .catch(() => {});
  }, [workspaceId]);

  // One list, because the server parses one namespace: a chip is drawn for
  // anything that would actually bind, whichever kind it turns out to be.
  const agentNames = useMemo(
    () => [...agents.map((a) => a.name), ...skills.map((s) => s.name)],
    [agents, skills],
  );

  // Switching conversations must drop the previous thread's messages and
  // stream, or the old run's text bleeds into the new chat. Deliberately an
  // effect rather than a React `key` on this component — a key would also
  // clear the draft and the engine choice, which today's behaviour keeps.
  useEffect(() => {
    setMessages([]);
    setActiveRunId(null);
    setOpenQuestion(null);
    setError(null);
    settling.current = null;
    pollGen.current++;
  }, [chatId]);

  // A conversation remembers what it was last run with, so reopening it picks
  // up where you left off rather than snapping back to the defaults. Guarded by
  // the ref so a routine refresh of the chat list can't undo a choice you made
  // in the composer but haven't sent yet.
  const seededFor = useRef<string | null>(null);
  useEffect(() => {
    if (!chatId || !chat || seededFor.current === chatId) return;
    seededFor.current = chatId;
    setTier(chat.modelTier ?? "medium");
    setModelId(chat.modelId ?? "");
    setEffort(chat.effort);
    setPlanMode(chat.planMode ?? false);
  }, [chatId, chat]);

  // The gap this papers over is real and server-side: the engine's
  // run_completed event reaches the browser from inside the stream loop, but
  // the assistant's reply row is inserted only after the loop returns. A
  // client that clears the live bubble on the event and trusts the next
  // fetch shows the reply, then nothing, then the reply again — the flicker
  // is the reply having no home for a beat. So the turn "settles" instead:
  // the live bubble stays until the persisted row (assistant or the failure
  // pill — both carry the run id) is actually in the list, with a bounded
  // grace for the one ending that never writes a row (a cancel).
  const settling = useRef<{ runId: string; polls: number } | null>(null);
  const refreshRef = useRef<() => void>(() => {});
  // A poll's answer only counts if nothing changed while it was out. Switching
  // chats, sending and answering each move this on, so a response that left
  // before them is dropped: otherwise chat A's messages, run and question land
  // in chat B, or a poll from just before a send resets the new run to none.
  // The open chat is checked too, since a switch renders before its effect
  // moves the counter.
  const pollGen = useRef(0);
  const openChat = useRef(chatId);
  openChat.current = chatId;

  const refresh = useCallback(async () => {
    if (!chatId) return;
    const gen = pollGen.current;
    try {
      const r = await api.chatMessages(chatId);
      if (gen !== pollGen.current || openChat.current !== chatId) return;
      setMessages(r.messages);
      setOpenQuestion(r.openQuestion);
      const s = settling.current;
      if (s) {
        const landed = r.messages.some((m) => m.runId === s.runId);
        if (!landed && s.polls < 8) {
          // The row is milliseconds away; check again quickly rather than
          // waiting out the slow poll, and keep the live bubble meanwhile.
          s.polls += 1;
          setTimeout(() => refreshRef.current(), 300);
          return;
        }
        settling.current = null;
        setActiveRunId(null);
        return;
      }
      setActiveRunId(r.activeRunId);
    } catch {
      /* server restarting; retry next tick */
    }
  }, [chatId]);
  useEffect(() => {
    refreshRef.current = refresh;
  }, [refresh]);

  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, 2500);
    return () => clearInterval(interval);
  }, [refresh]);

  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [messages.length, streamEvents.length]);

  // One turn at a time: the button is not the only way in (Enter is another),
  // and the composer is emptied before the request answers.
  const sending = useRef(false);
  const send = async () => {
    // An attachment on its own is a legitimate turn, so text is not required.
    if (!chatId || activeRunId || att.busy || sending.current) return;
    if (!draft.trim() && att.ids.length === 0 && articleIds.length === 0) return;
    sending.current = true;
    setError(null);
    const typed = draft;
    const content = draft.trim();
    const attachmentIds = att.ids;
    const pages = articleIds;
    const pageChips = articleChips;
    // Carry the chips into the optimistic bubble, or they'd vanish for the
    // ~2.5s until the next poll returns the real message.
    const sent = att.items.filter((i) => i.remote).map((i) => i.remote!);
    // Emptied at once so the composer is free while the turn starts — but
    // kept, because a refused turn (a paused agent, a spent budget, a turn
    // already running) must hand the message back rather than lose it.
    setDraft("");
    const files = att.take();
    setArticleIds([]);
    setArticleChips([]);
    pollGen.current++;
    setMessages((prev) => [
      ...prev,
      {
        id: "pending",
        role: "user",
        content,
        runId: null,
        ts: new Date().toISOString(),
        attachments: sent,
        articles: pageChips,
        isPlan: false,
        planOutcome: null,
        stopped: false,
      },
    ]);
    try {
      const r = await api.sendChat(chatId, content, {
        attachmentIds,
        articleIds: pages,
        engine: engine ?? undefined,
        modelTier: tier,
        modelId,
        effort,
        planMode: canPlan ? planMode : undefined,
      });
      pollGen.current++;
      att.release(files);
      setActiveRunId(r.runId);
      // The first message names the chat server-side — the caller's list is
      // what shows it.
      onSent?.();
    } catch (e) {
      pollGen.current++;
      setError(String(e).replace(/^Error:\s*/, ""));
      setMessages((prev) => prev.filter((m) => m.id !== "pending"));
      // Anything typed meanwhile stays, after the message that came back.
      setDraft((cur) => (cur ? `${typed}\n${cur}` : typed));
      att.restore(files);
      setArticleIds((cur) => [...pages, ...cur.filter((id) => !pages.includes(id))]);
      setArticleChips((cur) => [
        ...pageChips,
        ...cur.filter((c) => !pageChips.some((p) => p.id === c.id)),
      ]);
      refresh();
    } finally {
      sending.current = false;
    }
  };

  /**
   * Stop the turn that is running.
   *
   * One press, unlike the Activity page's confirm-then-stop. That page guards
   * runs that have been going for hours and sits next to rows you click to
   * open; this is a conversation, the turn is seconds old, and the reason to
   * stop is usually that you have read enough — a confirmation step there
   * would be in the way of the thing it is protecting.
   *
   * Nothing is cleared optimistically. The run's own ending writes the partial
   * reply, and clearing `activeRunId` here would take the live bubble away
   * before that row exists — the gap the settle logic was written to close.
   */
  const [stopping, setStopping] = useState(false);
  const stop = async () => {
    if (!activeRunId || stopping) return;
    setStopping(true);
    setError(null);
    try {
      await api.cancelRun(activeRunId);
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setStopping(false);
    }
    refresh();
  };

  /** Answer the assistant's clarifying question. Clears the card optimistically
   *  so it cannot be clicked twice while the turn starts — the server refuses a
   *  second answer anyway, but a button that looks live is a button people
   *  press. */
  const answerQuestion = async (answers: string[][]) => {
    if (!chatId || !openQuestion || activeRunId) return;
    const id = openQuestion.id;
    setError(null);
    setOpenQuestion(null);
    pollGen.current++;
    try {
      const r = await api.answerQuestion(chatId, id, answers);
      pollGen.current++;
      setActiveRunId(r.runId);
      refresh();
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
      refresh(); // puts the card back if it is genuinely still open
    }
  };

  /** Carry out a plan. Leaves plan mode, so the composer's toggle has to
   *  follow — the server has already turned it off on the row. */
  const approve = async (messageId: string, edited?: string) => {
    if (!chatId || activeRunId) return;
    setError(null);
    try {
      const r = await api.approveChatPlan(chatId, messageId, edited);
      setPlanMode(false);
      setActiveRunId(r.runId);
      refresh();
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
      refresh();
    }
  };

  // Live stream: show assistant text + tool chips while the turn runs.
  const liveText = streamEvents
    .filter((e) => e.type === "assistant_text")
    .map((e) => String(e.text))
    .join("\n");
  const liveTools = streamEvents.filter((e) => e.type === "tool_call");
  const turnDone = streamEvents.some(
    (e) => e.type === "run_completed" || e.type === "run_failed",
  );

  useEffect(() => {
    if (turnDone && activeRunId) {
      if (settling.current?.runId !== activeRunId) {
        settling.current = { runId: activeRunId, polls: 0 };
      }
      refresh();
    }
  }, [turnDone, activeRunId, refresh]);

  // Both wrappers exist only on the Chat page. The rail path renders the
  // children bare, so its DOM is exactly what ChatPanel produced before the
  // extraction — an extra <div>, even an inert one, would already be a
  // different tree to debug against.
  const wrap = (extra: string, children: React.ReactNode) =>
    centered ? <div className={`mx-auto w-full max-w-3xl ${extra}`.trim()}>{children}</div> : children;

  return (
    <>
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto p-4">
        {wrap(
          "",
          <>
          {messages.length === 0 && !activeRunId && (
            <div className="mt-8 px-4 text-center text-sm text-fg-muted">
              Describe what you want done — e.g. “fix the flaky login test and
              open it for review”. The assistant creates tasks on the board and
              keeps you posted here. Type <span className="font-medium">@</span> to
              hand the work to one of your agents, or to point at a file.
            </div>
          )}
          <div className="flex flex-col gap-3">
            {messages.map((m) => (
              <Message
                key={m.id + m.ts}
                message={m}
                agentNames={agentNames}
                onApprove={activeRunId ? undefined : approve}
              />
            ))}
            {activeRunId && (
              <div className="flex flex-col gap-2">
                {liveTools.map((t, i) => (
                  <ToolChip key={i} name={String(t.tool_name)} input={t.input} />
                ))}
                {liveText ? (
                  <div className="max-w-[85%] self-start rounded-2xl rounded-bl-sm bg-panel-2 px-3 py-2 text-sm">
                    <Markdown>{liveText}</Markdown>
                  </div>
                ) : (
                  <Thinking />
                )}
              </div>
            )}
            {/* Last, under the reply that asked it — the assistant's turn
                usually ends by saying why it is asking. Hidden while a turn
                runs, because answering into a busy chat is refused anyway. */}
            {openQuestion && !activeRunId && (
              <QuestionCard
                key={openQuestion.id}
                open={openQuestion}
                onAnswer={answerQuestion}
              />
            )}
          </div>
          </>,
        )}
      </div>

      {error && (
        <div className="mx-4 mb-1 rounded-lg bg-danger-subtle px-3 py-1.5 text-xs text-danger-fg">
          {error}
        </div>
      )}

      <div className="relative border-t border-border p-3" {...(canAttach ? att.dropProps : {})}>
        {wrap(
          "relative",
          <>
          {!general && mention.node}
          <div
            className={`flex flex-col gap-1.5 rounded-xl border bg-panel px-3 py-2 focus-within:border-accent ${
              att.dragging ? "border-accent ring-2 ring-accent/30" : "border-border"
            }`}
          >
            {/* Above the textarea, not beside it: what the assistant will be
                given to read is part of the question, and a control tucked
                into the icon row reads as a setting. Workspace-scoped, so it
                is offered in a general chat too, as file attachments are. */}
            {workspaceId && (
              <ArticlePicker
                workspaceId={workspaceId}
                selected={articleIds}
                onChange={(ids, chosen) => {
                  setArticleIds(ids);
                  setArticleChips(chosen.map((a) => ({ id: a.id, title: a.title })));
                }}
                compact
              />
            )}
            {canAttach && (att.items.length > 0 || att.dragging) && (
              <AttachmentBar
                items={att.items}
                onAdd={att.add}
                onRemove={att.remove}
                full={att.full}
                disabled={!!activeRunId}
              />
            )}
            <div className="flex items-end gap-2">
              {canAttach && att.items.length === 0 && !att.dragging && (
                <AttachmentBar
                  items={[]}
                  onAdd={att.add}
                  onRemove={att.remove}
                  full={att.full}
                  disabled={!!activeRunId}
                />
              )}
              <textarea
                ref={composerRef}
                value={draft}
                onChange={(e) => {
                  setDraft(e.target.value);
                  setCaret(e.target.selectionStart ?? 0);
                }}
                onSelect={(e) => setCaret(e.currentTarget.selectionStart ?? 0)}
                onPaste={canAttach ? att.onPaste : undefined}
                onKeyDown={(e) => {
                  // The picker gets first refusal: otherwise Enter sends the
                  // message instead of choosing the highlighted file.
                  if (!general && mention.handleKey(e)) {
                    e.preventDefault();
                    return;
                  }
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    send();
                  }
                }}
                rows={Math.min(4, Math.max(1, draft.split("\n").length))}
                placeholder={
                  chat?.agentName
                    ? activeRunId
                      ? `${chat.agentName} is working…`
                      : `Message ${chat.agentName}…`
                    : activeRunId
                      ? "Assistant is working…"
                      : "What should we work on?"
                }
                disabled={!!activeRunId}
                className="min-w-0 flex-1 resize-none bg-transparent text-sm outline-none disabled:opacity-60"
              />
              {/* One control, two jobs — the same place your hand already is.
                  A separate Stop elsewhere on the page would be a second
                  thing to find at the moment you least want to look. */}
              {activeRunId ? (
                <Button
                  variant="primary"
                  onClick={stop}
                  disabled={stopping}
                  title="Stop — keeps what it has said so far"
                  aria-label="Stop the assistant"
                  className="bg-fg! px-2.5! text-bg! hover:bg-fg/85! disabled:opacity-40!"
                >
                  ■
                </Button>
              ) : (
                <Button
                  variant="primary"
                  onClick={send}
                  disabled={
                    att.busy ||
                    (!draft.trim() && att.ids.length === 0 && articleIds.length === 0)
                  }
                  className="px-2.5! disabled:opacity-40!"
                >
                  ↑
                </Button>
              )}
            </div>
            {/* Which CLI, which model, and how hard it thinks. All three stick to
                the chat rather than the message — choosing "think harder" and
                having it last one turn would be a strange thing to have chosen. */}
            <div className="flex items-center gap-2">
              <ComposerSettings
                engine={engine}
                onEngine={setEngine}
                tier={tier}
                onTier={setTier}
                modelId={modelId}
                onModelId={setModelId}
                effort={effort}
                onEffort={setEffort}
                disabled={!!activeRunId}
                usesTools={!general}
              />
              {/* On the row, not inside the settings popover. A collapsed
                  control is fine for "which model"; plan mode changes whether
                  anything happens, and a mode you cannot see you are in is
                  the failure the mode exists to prevent. */}
              {canPlan && (
                <button
                  onClick={() => setPlanMode((v) => !v)}
                  disabled={!!activeRunId}
                  title={
                    planMode
                      ? "Plan mode is on — the assistant will propose, not act"
                      : "Plan mode: research and propose, create nothing until you approve"
                  }
                  className={`ring-focus rounded-md px-1.5 py-0.5 text-[11px] transition-colors disabled:opacity-50 ${
                    planMode
                      ? "bg-accent-subtle font-medium text-accent-fg"
                      : "text-fg-muted hover:bg-panel-2 hover:text-fg"
                  }`}
                >
                  ◷ Plan{planMode ? " mode" : ""}
                </button>
              )}
            </div>
          </div>
          </>,
        )}
      </div>
    </>
  );
}

function Message({
  message,
  agentNames,
  onApprove,
}: {
  message: ChatMessage;
  agentNames: string[];
  /** Carry out a plan. Absent while a turn is running — approving into a
   *  busy chat is refused server-side anyway, and a button that always
   *  fails is worse than one that is not there. */
  onApprove?: (messageId: string, edited?: string) => void;
}) {
  if (message.role === "user") {
    return (
      <motion.div
        initial={{ opacity: 0, y: 4 }}
        animate={{ opacity: 1, y: 0 }}
        className="flex max-w-[85%] flex-col items-end self-end"
      >
        {/* Above the bubble, not inside it: the bubble is solid accent, and
            bordered file chips read badly on it. */}
        <AttachmentList attachments={message.attachments} />
        {/* Which pages this turn was given. Kept on the message rather than
            cleared with the composer: afterwards, this is the only record of
            what the assistant was actually handed. */}
        {message.articles?.length > 0 && (
          <div className="mb-1 flex flex-wrap justify-end gap-1">
            {message.articles.map((a) => (
              <Link
                key={a.id}
                to={`/knowledge/${a.id}`}
                title="Knowledge-base page given to the assistant for this message"
                className="ring-focus max-w-56 truncate rounded-lg border border-accent/40 bg-accent/5 px-2 py-0.5 text-[11px] text-fg-muted hover:text-fg"
              >
                ▦ {a.title}
              </Link>
            ))}
          </div>
        )}
        {message.content && (
          <div className="rounded-2xl rounded-br-sm bg-accent px-3 py-2 text-sm whitespace-pre-wrap text-on-accent">
            <WithMentions text={message.content} agentNames={agentNames} />
          </div>
        )}
      </motion.div>
    );
  }
  if (message.role === "system") {
    return (
      <div className="self-center rounded-full bg-tier-easy-soft px-3 py-1 text-xs text-tier-easy">
        {message.content}
      </div>
    );
  }
  return (
    <motion.div
      initial={{ opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      className={`max-w-[85%] self-start rounded-2xl rounded-bl-sm px-3 py-2 text-sm ${
        message.isPlan ? "border border-accent/30 bg-accent/5" : "bg-panel-2"
      }`}
    >
      {message.isPlan && (
        <div className="mb-1 flex items-center gap-1.5 text-[10px] font-medium uppercase tracking-wide text-accent-fg">
          ◷ Plan — nothing has happened yet
        </div>
      )}
      <Markdown>{message.content}</Markdown>
      {/* A truncated reply and a short one look identical in the text. The
          assistant's session still holds the rest, so saying so is what makes
          "carry on" an obvious next message rather than a guess. */}
      {message.stopped && (
        <div className="mt-1.5 border-t border-border pt-1 text-[10px] text-fg-muted">
          ■ You stopped this — the assistant had more to say.
        </div>
      )}
      {message.isPlan && <PlanActions message={message} onApprove={onApprove} />}
    </motion.div>
  );
}

/**
 * What you can do about a plan.
 *
 * Approve carries it out. "Edit first" opens the plan as text, because the
 * cheapest correction to a five-step plan is usually to delete step three —
 * and the alternative, describing the edit in prose and hoping, is how a
 * second planning turn gets paid for.
 *
 * Anything else — "no, do it differently" — is just the next message. That is
 * the advantage of a plan that lives in a conversation rather than in a parked
 * run, and it is why there is no Reject button: closing the plan without
 * saying why would throw away the one thing the assistant needs.
 */
function PlanActions({
  message,
  onApprove,
}: {
  message: ChatMessage;
  onApprove?: (messageId: string, edited?: string) => void;
}) {
  const [editing, setEditing] = useState<string | null>(null);

  if (message.planOutcome === "approved") {
    return (
      <div className="mt-2 border-t border-accent/20 pt-1.5 text-[11px] text-fg-muted">
        ✓ Approved — carried out below.
      </div>
    );
  }
  if (message.planOutcome === "superseded") {
    return (
      <div className="mt-2 border-t border-accent/20 pt-1.5 text-[11px] text-fg-muted">
        Replaced by a later plan.
      </div>
    );
  }
  if (!onApprove) return null;

  if (editing !== null) {
    return (
      <div className="mt-2 border-t border-accent/20 pt-2">
        <textarea
          autoFocus
          value={editing}
          onChange={(e) => setEditing(e.target.value)}
          rows={Math.min(16, Math.max(4, editing.split("\n").length))}
          className="ring-focus w-full resize-y rounded-lg border border-border bg-panel p-2 font-mono text-[11px] outline-none focus:border-accent"
        />
        <div className="mt-1.5 flex gap-1.5">
          <Button variant="primary" size="xs" onClick={() => onApprove(message.id, editing)}>
            Approve this version
          </Button>
          <Button variant="secondary" size="xs" onClick={() => setEditing(null)}>
            Cancel
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="mt-2 flex items-center gap-1.5 border-t border-accent/20 pt-1.5">
      <Button variant="primary" size="xs" onClick={() => onApprove(message.id)}>
        Approve &amp; run
      </Button>
      <Button variant="secondary" size="xs" onClick={() => setEditing(message.content)}>
        Edit first
      </Button>
      <span className="text-[10px] text-fg-muted">or just say what to change</span>
    </div>
  );
}

/**
 * The message as sent, with `@Name` drawn as a chip.
 *
 * Not decoration: the mention is what decides who does the work, and the only
 * way to tell a mention that bound from a name that merely looks like one is to
 * show the difference. A name with no agent behind it stays plain text —
 * exactly what the server did with it.
 */
function WithMentions({ text, agentNames }: { text: string; agentNames: string[] }) {
  const spans = agentSpans(text, agentNames);
  if (!spans.length) return <>{text}</>;

  const parts: React.ReactNode[] = [];
  let at = 0;
  spans.forEach((span, i) => {
    if (span.start > at) parts.push(text.slice(at, span.start));
    parts.push(
      <span
        key={i}
        className="rounded bg-[color-mix(in_oklab,var(--color-on-accent)_25%,transparent)] px-1 font-medium"
        title={`Assigned to ${span.name}`}
      >
        {text.slice(span.start, span.end)}
      </span>,
    );
    at = span.end;
  });
  if (at < text.length) parts.push(text.slice(at));
  return <>{parts}</>;
}

function ToolChip({ name: recorded, input }: { name: string; input: unknown }) {
  const name = toolName(recorded);
  const label = (() => {
    const args = (input ?? {}) as Record<string, unknown>;
    if (name === "mcp__eren__create_task") {
      const who = typeof args.agent_name === "string" ? ` — ${args.agent_name}` : "";
      return `Creating task: ${args.title ?? ""}${who}`;
    }
    if (name === "mcp__eren__start_task") return "Starting task";
    if (name === "mcp__eren__list_tasks") return "Checking the board";
    if (name === "mcp__eren__get_task_status") return "Checking task status";
    if (name === "mcp__eren__list_agents") return "Browsing agents";
    if (name === "mcp__eren__cancel_task") return "Stopping the task";
    if (name === "mcp__eren__get_diff")
      return typeof args.path === "string" ? `Reading the diff: ${args.path}` : "Reading the diff";
    if (name === "mcp__eren__get_spend") return "Checking what this has cost";
    if (name === "mcp__eren__list_skills") return "Browsing skills";
    if (name === "mcp__eren__move_task") return `Filing the card in ${args.column ?? "a column"}`;
    if (name === "Read") return `Reading ${args.file_path ?? "a file"}`;
    if (name === "Grep") return "Searching the codebase";
    if (name === "Glob") return "Listing files";
    return name;
  })();
  return (
    <motion.div
      initial={{ opacity: 0, scale: 0.96 }}
      animate={{ opacity: 1, scale: 1 }}
      className="self-start rounded-full border border-border bg-panel px-3 py-1 text-xs text-fg-muted"
    >
      ⚙ {label}
    </motion.div>
  );
}

function Thinking() {
  return (
    <div className="flex gap-1 self-start rounded-2xl bg-panel-2 px-3 py-2.5">
      {[0, 1, 2].map((i) => (
        <motion.span
          key={i}
          className="h-1.5 w-1.5 rounded-full bg-fg-muted"
          animate={{ opacity: [0.3, 1, 0.3] }}
          transition={{ repeat: Infinity, duration: 1.2, delay: i * 0.2 }}
        />
      ))}
    </div>
  );
}
