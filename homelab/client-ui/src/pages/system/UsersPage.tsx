import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Field, Input } from "@/components/ui/input";
import { Banner, Skeleton } from "@/components/ui/feedback";
import { Row, RowAction, Section } from "@/components/ui/list";
import { api } from "@/lib/api";
import { useApi } from "@/lib/useResource";

export interface UserList {
  me: string | null;
  users: string[];
}

export const USERNAME = /^[a-z0-9][a-z0-9_-]{0,31}$/;
export const MIN_PASSWORD = 8;

export function passwordProblem(username: string, password: string) {
  if (!USERNAME.test(username)) {
    return "Use 1 to 32 lowercase letters, digits, - or _, starting with a letter or digit.";
  }
  if (password.length < MIN_PASSWORD) {
    return `The password needs at least ${MIN_PASSWORD} characters.`;
  }
  return null;
}

function PasswordForm({
  username,
  onDone,
  onCancel,
}: {
  username?: string;
  onDone: () => void;
  onCancel?: () => void;
}) {
  const [name, setName] = useState(username ?? "");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const problem = passwordProblem(name, password);
    if (problem) {
      setError(problem);
      return;
    }
    setSaving(true);
    setError(null);
    try {
      await api.put(`/api/users/${encodeURIComponent(name)}`, { password });
      setPassword("");
      if (!username) setName("");
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not save.");
    } finally {
      setSaving(false);
    }
  }

  return (
    <form onSubmit={submit} className="mt-4 space-y-4">
      {!username && (
        <Field label="Username" htmlFor="new-username">
          <Input
            id="new-username"
            value={name}
            onChange={(e) => setName(e.target.value.trim().toLowerCase())}
            autoComplete="off"
          />
        </Field>
      )}
      <Field
        label={username ? `New password for ${username}` : "Password"}
        htmlFor={username ? `password-${username}` : "new-password"}
        error={error}
        help="Nothing is sent by email: give it to them yourself."
      >
        <Input
          id={username ? `password-${username}` : "new-password"}
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoComplete="new-password"
        />
      </Field>
      <div className="flex gap-2">
        <Button type="submit" loading={saving} disabled={!name || !password}>
          {username ? "Change password" : "Add"}
        </Button>
        {onCancel && (
          <Button type="button" variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
        )}
      </div>
    </form>
  );
}

export function UsersPage() {
  const list = useApi<UserList>("users", "/api/users");
  const [editing, setEditing] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function remove(name: string) {
    setError(null);
    try {
      await api.del(`/api/users/${encodeURIComponent(name)}`);
      await list.refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not remove.");
    }
  }

  return (
    <>
      <p className="px-1 text-sm text-fg-muted">
        Everyone here can sign in to this page on any of your machines and
        manage everything. Changing someone&apos;s password or removing them
        signs them out everywhere.
      </p>
      {error && <Banner tone="error" title={error} className="mt-4" />}
      <Section title="People who can sign in">
        {list.data === undefined ? (
          <div className="px-5 py-4">
            <Skeleton className="h-5 w-40" />
          </div>
        ) : (
          list.data.users.map((name) => (
            <Row
              key={name}
              label={name}
              detail={name === list.data?.me ? "You" : undefined}
              trailing={
                <div className="flex gap-1">
                  <RowAction
                    onClick={() => setEditing(editing === name ? null : name)}
                  >
                    Change password
                  </RowAction>
                  {list.data && list.data.users.length > 1 && (
                    <RowAction onClick={() => void remove(name)}>
                      Remove
                    </RowAction>
                  )}
                </div>
              }
            >
              {editing === name && (
                <PasswordForm
                  username={name}
                  onDone={() => {
                    setEditing(null);
                    void list.refresh();
                  }}
                  onCancel={() => setEditing(null)}
                />
              )}
            </Row>
          ))
        )}
      </Section>
      <Card className="mt-8 p-5">
        <div className="text-sm font-medium text-fg">Add someone</div>
        <PasswordForm onDone={() => void list.refresh()} />
      </Card>
    </>
  );
}
