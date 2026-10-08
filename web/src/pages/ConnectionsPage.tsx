import { useCallback, useEffect, useState } from "react";
import { motion } from "framer-motion";
import { api, GitHubConnect, GitHubStatus, McpServer, McpTestResult } from "../lib/api";
import { useWorkspace } from "../lib/workspace";
import { Page, PageHead } from "../components/ui/Surface";
import { Icon } from "../components/ui/Icon";
import { Button, buttonClasses } from "../components/ui/Button";
import { Dialog } from "../components/ui/Dialog";
import { safeHttpUrl } from "../lib/url";

/**
 * MCP servers the user connects.
 *
 * Every agent could previously do exactly three things — read files, write
 * files, run bash — because the only MCP server in play was Eren's own.
 * This is where that stops being the ceiling: connect a browser, a database,
 * an issue tracker, then tick it on for the agents that should have it.
 *
 * Nothing here touches credentials. Eren spawns the official CLI and hands
 * it a `--mcp-config` file, which is the same thing you'd write by hand.
 */
/**
 * GitHub, which is a connection but not an MCP server.
 *
 * It sits above the server list rather than in it because there is nothing to
 * configure — Eren drives the `gh` CLI you already have, so the only question
 * is whether it is installed and logged in. There is no field to fill in and no
 * token to paste, which is the point.
 *
 * Re-checked on every visit, because `gh auth login` happens in a terminal
 * while Eren is running, and telling someone to go and run it is most of what
 * this card is for.
 */
function GitHubCard() {
  const [state, setState] = useState<GitHubStatus | null>(null);
  const [flow, setFlow] = useState<GitHubConnect | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [scopes, setScopes] = useState<{
    required: string[];
    optional: { name: string; what: string }[];
  } | null>(null);
  /** Nothing beyond gh's minimum unless asked for. */
  const [extra, setExtra] = useState<string[]>([]);

  const refresh = useCallback(
    () => api.github().then(setState).catch(() => {}),
    [],
  );
  useEffect(() => {
    refresh();
    api.githubScopes().then(setScopes).catch(() => {});
  }, [refresh]);

  // Poll only while a flow is open. It finishes when the person finishes it in
  // their browser, which nothing here can hurry along.
  useEffect(() => {
    if (!flow) return;
    const t = setInterval(async () => {
      const p = await api.githubConnectStatus(flow.id).catch(() => null);
      if (!p || p.state === "waiting") return;
      clearInterval(t);
      setFlow(null);
      if (p.state === "failed") setError(p.reason);
      else refresh();
    }, 2000);
    return () => clearInterval(t);
  }, [flow, refresh]);

  const connect = async () => {
    setBusy(true);
    setError(null);
    try {
      setFlow(await api.connectGitHub(extra));
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  };

  if (!state) return null;

  const account = state.accounts.find((a) => a.active) ?? state.accounts[0];
  const problem = account?.problem;

  return (
    <div className="mt-6 max-w-4xl rounded-xl border border-border bg-panel p-4">
      <div className="flex flex-wrap items-baseline gap-2">
        <span className="text-sm font-semibold">GitHub</span>
        {state.usable ? (
          <span className="rounded-full bg-tier-easy-soft px-2 py-0.5 text-[11px] text-tier-easy">
            ✓ {account?.login} on {account?.host}
          </span>
        ) : (
          <span className="rounded-full bg-panel-2 px-2 py-0.5 text-[11px] text-fg-muted">
            {state.installed ? "not logged in" : "gh not installed"}
          </span>
        )}
      </div>

      <p className="mt-1.5 max-w-xl text-xs text-fg-muted">
        {state.usable
          ? "Clone a repo, open a pull request from a finished task, and pull issues in as cards. Eren runs your own gh CLI and never sees a token."
          : state.installed
            ? "eren drives the gh CLI you already have, so there is no token to paste here — it just needs to be logged in."
            : "Install the GitHub CLI and log in, and cloning, pull requests and issue import become available. Eren never handles a token of its own."}
      </p>

      {/* `gh`'s own words. "Not logged in" alone would send someone to re-auth
          without saying that their token was revoked rather than missing. */}
      {problem && (
        <p className="mt-1.5 text-xs text-danger">
          {account?.login} on {account?.host}: {problem}
        </p>
      )}

      {error && (
        <p className="mt-1.5 text-xs text-danger">{error}</p>
      )}

      {/* Nothing to offer without the binary: this is a package to install,
          not a button to press. */}
      {!state.usable && !state.installed && (
        <code className="mt-2 inline-block rounded-md bg-panel-2 px-2 py-1 text-[11px]">
          brew install gh
        </code>
      )}

      {!state.usable && state.installed && !flow && (
        <div className="mt-2">
          {/* Said before the button, not after. What a sign-in will be able to
              reach is the thing worth knowing while you can still decline. */}
          {scopes && (
            <div className="mb-2 rounded-lg border border-border bg-panel-2 p-2.5 text-[11px] leading-relaxed text-fg-muted">
              <div>
                Signs in as you.{" "}
                <span className="text-fg">
                  Organisations are a separate choice on GitHub's own page
                </span>{" "}
                — it lists each one with its own Grant button, and granting none
                leaves this personal.
              </div>
              <div className="mt-1">
                gh requires{" "}
                {scopes.required.map((r, i) => (
                  <span key={r}>
                    {i > 0 && ", "}
                    <code className="text-[11px]">{r}</code>
                  </span>
                ))}{" "}
                and will not go below that. Eren asks for nothing more unless
                you tick it.
              </div>
              {scopes.optional.map((o) => (
                <label
                  key={o.name}
                  className="mt-1.5 flex cursor-pointer items-start gap-1.5"
                >
                  <input
                    type="checkbox"
                    checked={extra.includes(o.name)}
                    onChange={(e) =>
                      setExtra((x) =>
                        e.target.checked
                          ? [...x, o.name]
                          : x.filter((n) => n !== o.name),
                      )
                    }
                    className="mt-0.5 accent-[var(--color-accent)]"
                  />
                  <span>
                    <code className="text-[11px]">{o.name}</code> — {o.what}
                  </span>
                </label>
              ))}
            </div>
          )}
          <Button variant="primary" size="sm" onClick={connect} disabled={busy}>
            {busy ? "Starting…" : "Connect GitHub"}
          </Button>
        </div>
      )}

      {flow && (
        <div className="mt-2 rounded-xl border border-border bg-panel-2 p-3">
          <div className="text-xs text-fg-muted">
            Enter this code on GitHub. Eren never sees the token — GitHub
            gives it straight to your <code className="text-[11px]">gh</code>.
          </div>
          <div className="mt-2 flex flex-wrap items-center gap-2">
            <code className="rounded-md bg-panel px-2.5 py-1.5 font-mono text-sm tracking-widest">
              {flow.code}
            </code>
            <Button
              variant="secondary"
              size="xs"
              onClick={() => {
                navigator.clipboard?.writeText(flow.code);
                setCopied(true);
              }}
            >
              {copied ? "copied" : "copy"}
            </Button>
            <a
              href={safeHttpUrl(flow.url)}
              target="_blank"
              rel="noreferrer"
              className={buttonClasses({ variant: "primary", size: "xs" })}
            >
              Open GitHub
            </a>
            <Button
              variant="ghost"
              size="xs"
              onClick={() => {
                api.cancelGitHubConnect(flow.id);
                setFlow(null);
              }}
            >
              cancel
            </Button>
          </div>
          <div className="mt-1.5 text-[11px] text-fg-muted">
            Waiting for you to finish in the browser… GitHub will list your
            organisations separately — skip them to keep this personal.
          </div>
        </div>
      )}
    </div>
  );
}

export default function ConnectionsPage() {
  const { active } = useWorkspace();
  const [servers, setServers] = useState<McpServer[]>([]);
  const [editing, setEditing] = useState<McpServer | "new" | null>(null);

  const load = useCallback(() => {
    if (!active) return;
    api.mcpServers(active.id).then((r) => setServers(r.servers)).catch(() => {});
  }, [active]);

  useEffect(load, [load]);

  return (
    <Page>
      <PageHead
        title="Connections"
        subtitle="MCP servers give your agents tools beyond reading, writing, and running commands. Connect one here, then switch it on for the agents that should have it."
        actions={
          <Button
            variant="primary"
            size="md"
            onClick={() => setEditing("new")}
            icon={<Icon name="plus" size={15} strokeWidth={2.5} />}
          >
            Connect a server
          </Button>
        }
      />

      <GitHubCard />

      <div className="mt-6 grid max-w-4xl gap-3">
        {servers.map((s) => (
          <ServerCard key={s.id} server={s} onEdit={() => setEditing(s)} onChanged={load} />
        ))}
        {servers.length === 0 && (
          <div className="rounded-xl border border-dashed border-border p-8 text-center">
            <div className="text-sm text-fg-muted">Nothing connected yet.</div>
            <div className="mx-auto mt-3 max-w-md text-left text-xs text-fg-muted">
              A few that work well:
              <ul className="mt-2 space-y-1.5">
                <li>
                  <span className="font-medium text-fg">Playwright</span> —{" "}
                  <code className="rounded bg-panel-2 px-1">
                    npx -y @playwright/mcp
                  </code>{" "}
                  lets a QA agent actually open the page it's testing.
                </li>
                <li>
                  <span className="font-medium text-fg">Postgres</span> — a read-only
                  connection so an agent designs against the real schema instead of
                  guessing from migrations.
                </li>
                <li>
                  <span className="font-medium text-fg">Your issue tracker</span> — so
                  a task can read the ticket it's implementing.
                </li>
              </ul>
            </div>
          </div>
        )}
      </div>

      {editing && (
        <ServerEditor
          workspaceId={active?.id ?? ""}
          server={editing === "new" ? null : editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            load();
          }}
        />
      )}
    </Page>
  );
}

function ServerCard({
  server,
  onEdit,
  onChanged,
}: {
  server: McpServer;
  onEdit: () => void;
  onChanged: () => void;
}) {
  const [test, setTest] = useState<McpTestResult | null>(null);
  const [testing, setTesting] = useState(false);

  const runTest = async () => {
    setTesting(true);
    setTest(null);
    try {
      setTest(await api.testMcpServer(server.id));
    } catch (e) {
      setTest({ ok: false, error: String(e).replace(/^Error:\s*/, "") });
    } finally {
      setTesting(false);
    }
  };

  return (
    <motion.div
      layout
      className="card-shadow rounded-xl border border-border bg-panel p-4"
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="text-sm font-semibold">{server.name}</span>
            <span className="rounded-full bg-panel-2 px-2 py-0.5 text-[11px] text-fg-muted">
              {server.transport}
            </span>
            {!server.enabled && (
              <span className="rounded-full bg-panel-2 px-2 py-0.5 text-[11px] text-fg-muted">
                off
              </span>
            )}
          </div>
          <div className="mt-1 truncate font-mono text-xs text-fg-muted">
            {server.transport === "stdio"
              ? [server.command, ...server.args].join(" ")
              : server.url}
          </div>
          <div className="mt-1 text-[11px] text-fg-muted">
            Tools appear to agents as{" "}
            <code className="rounded bg-panel-2 px-1">{server.toolPrefix}__*</code>
          </div>
        </div>
        <div className="flex shrink-0 gap-2">
          <Button variant="secondary" size="sm" onClick={runTest} disabled={testing}>
            {testing ? "Connecting…" : "Test"}
          </Button>
          <Button variant="secondary" size="sm" onClick={onEdit}>
            Edit
          </Button>
          <Button
            variant="danger"
            size="sm"
            onClick={async () => {
              await api.deleteMcpServer(server.id);
              onChanged();
            }}
          >
            Remove
          </Button>
        </div>
      </div>

      {test && (
        <div
          className={`mt-3 rounded-lg px-3 py-2 text-xs ${
            test.ok
              ? "bg-tier-easy-soft text-tier-easy"
              : "bg-danger-subtle text-danger-fg"
          }`}
        >
          {test.ok ? (
            test.tools.length > 0 ? (
              <>
                <span className="font-medium">
                  Connected — {test.tools.length} tool
                  {test.tools.length === 1 ? "" : "s"}:
                </span>{" "}
                {test.tools.join(", ")}
              </>
            ) : (
              <span className="font-medium">Connected.</span>
            )
          ) : (
            test.error
          )}
        </div>
      )}
    </motion.div>
  );
}

function ServerEditor({
  workspaceId,
  server,
  onClose,
  onSaved,
}: {
  workspaceId: string;
  server: McpServer | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [name, setName] = useState(server?.name ?? "");
  const [transport, setTransport] = useState(server?.transport ?? "stdio");
  // Edited as one line because that's how these are documented and copied.
  const [command, setCommand] = useState(
    server ? [server.command, ...server.args].filter(Boolean).join(" ") : "",
  );
  const [url, setUrl] = useState(server?.url ?? "");
  const [env, setEnv] = useState(
    Object.entries(server?.env ?? {})
      .map(([k, v]) => `${k}=${v}`)
      .join("\n"),
  );
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      const [cmd, ...args] = command.trim().split(/\s+/).filter(Boolean);
      const body = {
        workspace_id: workspaceId,
        name,
        transport,
        command: cmd ?? null,
        args,
        url: url.trim() || null,
        env: parseEnv(env),
      };
      if (server) await api.updateMcpServer(server.id, body);
      else await api.createMcpServer(body);
      onSaved();
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={server ? `Edit ${server.name}` : "Connect an MCP server"}
      width={512}
      footer={
        <>
          <Button variant="ghost" size="md" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" size="md" onClick={save} disabled={busy || !name.trim()}>
            {busy ? "Saving…" : "Save"}
          </Button>
        </>
      }
    >
        <label className="block text-xs font-medium text-fg-muted">Name</label>
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="playwright"
          className="mt-1 w-full rounded-lg border border-border bg-panel px-3 py-2 text-sm outline-none focus:border-accent"
        />
        <div className="mt-1 text-[11px] text-fg-muted">
          Becomes the tool prefix agents see. Spaces and punctuation become
          underscores.
        </div>

        <label className="mt-4 block text-xs font-medium text-fg-muted">How it runs</label>
        <div className="mt-1 flex gap-2">
          {(["stdio", "http", "sse"] as const).map((t) => (
            <button
              key={t}
              onClick={() => setTransport(t)}
              className={`rounded-lg border px-3 py-1.5 text-xs ${
                transport === t
                  ? "border-accent bg-accent-subtle text-accent-fg"
                  : "border-border hover:bg-panel-2"
              }`}
            >
              {t === "stdio" ? "Local command" : t.toUpperCase()}
            </button>
          ))}
        </div>

        {transport === "stdio" ? (
          <>
            <label className="mt-4 block text-xs font-medium text-fg-muted">Command</label>
            <input
              value={command}
              onChange={(e) => setCommand(e.target.value)}
              placeholder="npx -y @playwright/mcp"
              className="mt-1 w-full rounded-lg border border-border bg-panel px-3 py-2 font-mono text-sm outline-none focus:border-accent"
            />
            <label className="mt-4 block text-xs font-medium text-fg-muted">
              Environment (one KEY=value per line)
            </label>
            <textarea
              value={env}
              onChange={(e) => setEnv(e.target.value)}
              rows={3}
              placeholder="DATABASE_URL=postgres://localhost/app"
              className="mt-1 w-full resize-none rounded-lg border border-border bg-panel px-3 py-2 font-mono text-xs outline-none focus:border-accent"
            />
            <div className="mt-1 text-[11px] text-fg-muted">
              Anthropic API keys are refused here — Eren runs on your CLI's own
              login and never handles credentials.
            </div>
          </>
        ) : (
          <>
            <label className="mt-4 block text-xs font-medium text-fg-muted">URL</label>
            <input
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              placeholder="https://example.com/mcp"
              className="mt-1 w-full rounded-lg border border-border bg-panel px-3 py-2 font-mono text-sm outline-none focus:border-accent"
            />
          </>
        )}

        {error && (
          <div className="mt-3 rounded-lg bg-danger-subtle px-3 py-2 text-xs text-danger-fg">{error}</div>
        )}
    </Dialog>
  );
}

/** `KEY=value` lines to an object. Values may contain `=`; keys may not. */
function parseEnv(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const eq = trimmed.indexOf("=");
    if (eq <= 0) continue;
    out[trimmed.slice(0, eq).trim()] = trimmed.slice(eq + 1).trim();
  }
  return out;
}
