import { useCallback, useEffect, useState } from "react";
import { Copy, KeyRound, UserRound } from "lucide-react";
import { api, type User } from "../lib/api";
import { useAuth } from "../lib/auth";
import { Page } from "../components/ui/Surface";
import { EmptyState, PageHeader, Table } from "../components/ui/Layout";
import { Badge } from "../components/ui/Badge";
import { Button } from "../components/ui/Button";
import { Dialog } from "../components/ui/Dialog";
import { Switch } from "../components/ui/Field";
import { toast } from "../components/ui/Toast";

/**
 * The admin's list of accounts: who exists, a password reset, disabling an
 * account, and whether anyone may sign up. Each account sees only its own
 * workspaces — the admin's included — so this page lists people, not their
 * work.
 */
export default function UsersPage() {
  const { accounts, isAdmin, user: me } = useAuth();
  const [users, setUsers] = useState<User[] | null>(null);
  const [signupOpen, setSignupOpen] = useState<boolean | null>(null);
  const [reset, setReset] = useState<{ user: User; password: string } | null>(null);
  const [confirm, setConfirm] = useState<User | null>(null);

  const load = useCallback(async () => {
    try {
      const [{ users }, status] = await Promise.all([api.users(), api.authStatus()]);
      setUsers(users);
      setSignupOpen(status.signup);
    } catch (e) {
      setUsers([]);
      toast("Could not load the accounts", { tone: "danger", body: e instanceof Error ? e.message : undefined });
    }
  }, []);

  useEffect(() => {
    if (accounts && isAdmin) void load();
  }, [accounts, isAdmin, load]);

  if (!accounts) {
    return (
      <Page>
        <PageHeader title="Users" />
        <EmptyState
          icon={<UserRound className="size-4" />}
          title="Accounts are off"
          hint="Everyone who can reach this Eren uses it as one person. To give each person an account and workspaces of their own, run `eren admin create --username <name>` on this machine (in Docker: docker compose exec eren eren admin create --username <name>)."
        />
      </Page>
    );
  }

  if (!isAdmin) {
    return (
      <Page>
        <PageHeader title="Users" />
        <EmptyState icon={<UserRound className="size-4" />} title="Only the admin manages accounts" />
      </Page>
    );
  }

  const doReset = async (u: User) => {
    setConfirm(null);
    try {
      const { password } = await api.resetPassword(u.id);
      setReset({ user: u, password });
      void load();
    } catch (e) {
      toast("Could not reset the password", { tone: "danger", body: e instanceof Error ? e.message : undefined });
    }
  };

  const toggleDisabled = async (u: User) => {
    try {
      await api.setUserDisabled(u.id, !u.disabled);
      void load();
    } catch (e) {
      toast("Could not change the account", { tone: "danger", body: e instanceof Error ? e.message : undefined });
    }
  };

  const toggleSignup = async (open: boolean) => {
    setSignupOpen(open);
    try {
      await api.setSignupOpen(open);
    } catch (e) {
      setSignupOpen(!open);
      toast("Could not change sign-up", { tone: "danger", body: e instanceof Error ? e.message : undefined });
    }
  };

  return (
    <Page>
      <PageHeader
        title="Users"
        description="Everyone with an account here. Each person sees only their own workspaces. Resetting a password signs that person out and gives them a temporary one to replace."
      />

      <div className="mb-5 flex items-center justify-between gap-4 rounded-lg border border-border bg-panel px-4 py-3">
        <div className="min-w-0">
          <div className="text-[13px] font-medium text-fg">Allow sign-up</div>
          <p className="mt-0.5 text-xs leading-relaxed text-fg-muted">
            Off until you open it. While open, anyone who can reach this address can create an account; close it again once everyone who should have one does.
          </p>
        </div>
        {signupOpen !== null && <Switch checked={signupOpen} onChange={(v) => void toggleSignup(v)} label="Allow sign-up" />}
      </div>

      {users === null ? (
        <div className="text-[13px] text-fg-muted">Loading…</div>
      ) : (
        <Table>
          <thead>
            <tr>
              <th>Username</th>
              <th>Joined</th>
              <th>Status</th>
              <th className="text-right">
                <span className="sr-only">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {users.map((u) => (
              <tr key={u.id}>
                <td>
                  <span className="font-medium text-fg">{u.username}</span>
                  {u.id === me?.id && <span className="ml-1.5 text-xs text-fg-muted">(you)</span>}
                </td>
                <td className="tabular text-fg-muted">{new Date(u.createdAt).toLocaleDateString()}</td>
                <td>
                  <div className="flex flex-wrap gap-1">
                    {u.isAdmin && <Badge tone="accent">Admin</Badge>}
                    {u.disabled ? <Badge tone="danger">Disabled</Badge> : <Badge tone="success">Active</Badge>}
                    {u.mustChangePassword && <Badge tone="warning">Temporary password</Badge>}
                  </div>
                </td>
                <td>
                  <div className="flex justify-end gap-1.5">
                    {!u.isAdmin && (
                      <>
                        <Button size="xs" onClick={() => setConfirm(u)}>
                          <KeyRound className="size-3.5" />
                          Reset password
                        </Button>
                        <Button size="xs" variant={u.disabled ? "secondary" : "ghost"} onClick={() => void toggleDisabled(u)}>
                          {u.disabled ? "Enable" : "Disable"}
                        </Button>
                      </>
                    )}
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </Table>
      )}

      <Dialog
        open={!!confirm}
        onOpenChange={(o) => !o && setConfirm(null)}
        title={`Reset the password for ${confirm?.username ?? ""}?`}
        description="They are signed out everywhere, and get a temporary password that only works to choose a new one."
        footer={
          <>
            <Button onClick={() => setConfirm(null)}>Cancel</Button>
            <Button variant="danger" onClick={() => confirm && void doReset(confirm)}>
              Reset password
            </Button>
          </>
        }
      />

      <Dialog
        open={!!reset}
        onOpenChange={(o) => !o && setReset(null)}
        dismissible={false}
        title={`Temporary password for ${reset?.user.username ?? ""}`}
        description="Shown once. Give it to them; they choose their own at their next sign-in."
        footer={<Button variant="primary" onClick={() => setReset(null)}>Done</Button>}
      >
        <div className="flex items-center gap-2">
          <code className="min-w-0 flex-1 select-all truncate rounded-md border border-border bg-panel-2 px-2.5 py-1.5 font-mono text-[13px]">
            {reset?.password}
          </code>
          <Button
            size="sm"
            onClick={() => {
              if (!reset) return;
              void navigator.clipboard?.writeText(reset.password).then(
                () => toast("Copied", { tone: "success" }),
                () => toast("Could not copy; select it instead", { tone: "warning" }),
              );
            }}
          >
            <Copy className="size-3.5" />
            Copy
          </Button>
        </div>
      </Dialog>
    </Page>
  );
}
