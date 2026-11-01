import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@octra/ui";
import { useState, type FormEvent } from "react";

import { isTauri } from "../../lib/control-api";
import { useControlSettings } from "../../lib/store";

const inputClass =
  "mt-1 w-full rounded-md border border-border bg-background px-3 py-2 text-sm outline-none transition-colors focus:border-primary";

export function SettingsPage() {
  const queryClient = useQueryClient();
  const baseUrl = useControlSettings((state) => state.baseUrl);
  const token = useControlSettings((state) => state.token);
  const setBaseUrl = useControlSettings((state) => state.setBaseUrl);
  const setToken = useControlSettings((state) => state.setToken);

  const [draftBaseUrl, setDraftBaseUrl] = useState(baseUrl);
  const [draftToken, setDraftToken] = useState(token);
  const desktop = isTauri();

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBaseUrl(draftBaseUrl.trim() || "http://127.0.0.1:8081");
    setToken(draftToken.trim());
    void queryClient.invalidateQueries({ queryKey: ["health"] });
  }

  return (
    <div className="space-y-8">
      <header>
        <h1 className="text-2xl font-semibold">Settings</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          Local runtime and control plane connection.
        </p>
      </header>

      <section className="rounded-lg border border-border bg-card p-6">
        <h2 className="text-sm font-medium">Control plane</h2>

        {desktop ? (
          <p className="mt-3 text-sm text-muted-foreground">
            The desktop host reads credentials from{" "}
            <code className="rounded bg-background px-1.5 py-0.5">
              $OCTRA_HOME/control.token
            </code>{" "}
            and the daemon address from{" "}
            <code className="rounded bg-background px-1.5 py-0.5">$OCTRA_HOME/config.toml</code>.
          </p>
        ) : (
          <form className="mt-4 space-y-4" onSubmit={handleSubmit}>
            <label className="block text-sm">
              <span className="text-muted-foreground">Base URL</span>
              <input
                className={inputClass}
                value={draftBaseUrl}
                onChange={(event) => setDraftBaseUrl(event.target.value)}
                placeholder="http://127.0.0.1:8081"
              />
            </label>

            <label className="block text-sm">
              <span className="text-muted-foreground">Bearer token</span>
              <input
                className={inputClass}
                type="password"
                value={draftToken}
                onChange={(event) => setDraftToken(event.target.value)}
                placeholder="contents of $OCTRA_HOME/control.token"
              />
            </label>

            <Button type="submit">Save connection</Button>
          </form>
        )}
      </section>

      <section className="rounded-lg border border-border bg-card p-6">
        <h2 className="text-sm font-medium">Local data</h2>
        <p className="mt-3 text-sm text-muted-foreground">
          SQLite, configuration, and the control token live under{" "}
          <code className="rounded bg-background px-1.5 py-0.5">$OCTRA_HOME</code>, which defaults
          to <code className="rounded bg-background px-1.5 py-0.5">~/.octra</code>.
        </p>
      </section>
    </div>
  );
}
