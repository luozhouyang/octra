import type { ConsumerPolicy } from "@octra/types";
import { Button, Input } from "@octra/ui";
import { Trash2 } from "lucide-react";
import { useState, type FormEvent, type ReactNode } from "react";

export type ConsumerPolicyScope = "global" | "consumer";

export interface ConsumerPolicyInput {
  name: string;
  scope: ConsumerPolicyScope;
  consumer_id: string | null;
  document: Record<string, unknown>;
}

export interface ConsumerPolicyFormProps {
  initial: ConsumerPolicy | null;
  saving: boolean;
  deleting: boolean;
  onSave: (body: ConsumerPolicyInput) => void;
  onCancel: () => void;
  onDelete?: () => void;
}

interface DocumentParseResult {
  document: Record<string, unknown> | null;
  error: string | null;
}

export function ConsumerPolicyForm({
  initial,
  saving,
  deleting,
  onSave,
  onCancel,
  onDelete,
}: ConsumerPolicyFormProps) {
  const initialDocument = asRecord(initial?.document);
  const [name, setName] = useState(() => initial?.name ?? "");
  const [scope, setScope] = useState<ConsumerPolicyScope>(() => readScope(initial?.scope));
  const [consumerId, setConsumerId] = useState(() =>
    initial?.scope === "consumer" ? (initial.consumer_id ?? "") : "",
  );
  const [allowedModels, setAllowedModels] = useState(() =>
    readAllowlist(initialDocument, "allowed_models"),
  );
  const [allowedProviders, setAllowedProviders] = useState(() =>
    readAllowlist(initialDocument, "allowed_providers"),
  );
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [advancedDirty, setAdvancedDirty] = useState(false);
  const [advancedText, setAdvancedText] = useState("");
  const [errors, setErrors] = useState<Record<string, string>>({});

  const typedDocument = buildDocument(
    stripAllowlists(initial?.document),
    allowedModels,
    allowedProviders,
  );
  const advancedParsed: DocumentParseResult = advancedDirty
    ? parseDocumentObject(advancedText)
    : { document: typedDocument, error: null };
  const advancedValue = advancedDirty ? advancedText : JSON.stringify(typedDocument, null, 2);
  const pending = saving || deleting;
  const documentInvalid = advancedDirty && advancedParsed.error !== null;

  function changeScope(next: ConsumerPolicyScope) {
    setScope(next);
    if (next === "global") {
      setConsumerId("");
    }
  }

  function resetAdvanced() {
    setAdvancedDirty(false);
    setAdvancedText("");
  }

  function submit() {
    const nextErrors: Record<string, string> = {};
    const trimmedName = name.trim();
    if (!trimmedName) {
      nextErrors.name = "Name is required.";
    }
    const trimmedConsumerId = consumerId.trim();
    if (scope === "consumer" && !trimmedConsumerId) {
      nextErrors.consumer_id = "Consumer id is required.";
    }
    if (Object.keys(nextErrors).length > 0 || advancedParsed.document === null) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    onSave({
      name: trimmedName,
      scope,
      consumer_id: scope === "consumer" ? trimmedConsumerId : null,
      document: advancedParsed.document,
    });
  }

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    submit();
  }

  return (
    <form onSubmit={handleSubmit} className="flex min-w-0 flex-col gap-3">
      <span className="text-[11px] font-medium text-foreground">
        {initial ? `Edit policy ${initial.name}` : "New policy"}
      </span>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="default"
            aria-invalid={Boolean(errors.name)}
          />
        </Field>
        <Field label="Scope">
          <Select
            value={scope}
            onChange={(value) => changeScope(value === "consumer" ? "consumer" : "global")}
          >
            <option value="global">Global</option>
            <option value="consumer">Consumer</option>
          </Select>
        </Field>
        {scope === "consumer" ? (
          <Field
            label="Consumer id"
            required
            error={errors.consumer_id}
            hint="Required when the policy applies to a single consumer."
          >
            <Input
              value={consumerId}
              onChange={(event) => setConsumerId(event.target.value)}
              placeholder="consumer-id"
              className="font-mono text-[11px]"
              aria-invalid={Boolean(errors.consumer_id)}
            />
          </Field>
        ) : null}
      </div>
      <Field
        label="Allowed models"
        hint="Comma-separated model ids; leave empty to inherit, or use * to allow everything."
      >
        <Input
          value={allowedModels}
          onChange={(event) => setAllowedModels(event.target.value)}
          placeholder="gpt-4o-mini, *"
          className="font-mono text-[11px]"
        />
      </Field>
      <Field
        label="Allowed providers"
        hint="Comma-separated provider ids; leave empty to inherit, or use * to allow everything."
      >
        <Input
          value={allowedProviders}
          onChange={(event) => setAllowedProviders(event.target.value)}
          placeholder="openai, *"
          className="font-mono text-[11px]"
        />
      </Field>
      <details
        className="rounded-lg border border-border bg-surface/50 p-3"
        open={advancedOpen}
        onToggle={(event) => setAdvancedOpen(event.currentTarget.open)}
      >
        <summary className="cursor-pointer select-none text-[10.5px] text-muted-foreground">
          Advanced document JSON
          <span className="ml-2 text-[9.5px] text-muted-foreground">
            {advancedDirty ? "edited; saved as the policy document" : "synced from typed fields"}
          </span>
        </summary>
        <div className="mt-2 flex flex-col gap-2">
          <textarea
            value={advancedValue}
            onChange={(event) => {
              setAdvancedText(event.target.value);
              setAdvancedDirty(true);
            }}
            spellCheck={false}
            rows={8}
            aria-invalid={advancedParsed.error !== null}
            className="min-h-[160px] w-full resize-y rounded-lg border border-border bg-surface p-3 font-mono text-[10.5px] leading-relaxed text-secondary outline-none transition-colors placeholder:text-muted-foreground focus:border-primary/40 focus:ring-2 focus:ring-primary/15"
          />
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="text-[9.5px] text-muted-foreground">
              Editing this document overrides the typed allowlists on save. Name, scope and consumer
              id still come from the fields above.
            </span>
            <Button
              size="sm"
              variant="ghost"
              type="button"
              disabled={!advancedDirty}
              onClick={resetAdvanced}
            >
              Reset from typed fields
            </Button>
          </div>
          {advancedParsed.error ? (
            <span className="text-[10px] text-danger">{advancedParsed.error}</span>
          ) : null}
        </div>
      </details>
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" type="submit" disabled={pending || documentInvalid}>
          {saving ? "Saving…" : "Save"}
        </Button>
        <Button size="sm" variant="secondary" type="button" disabled={pending} onClick={onCancel}>
          Cancel
        </Button>
        <Button
          size="sm"
          variant="danger"
          type="button"
          className="ml-auto"
          disabled={pending || initial === null || !onDelete}
          onClick={onDelete}
        >
          <Trash2 className="h-3.5 w-3.5" />
          {deleting ? "Deleting…" : "Delete"}
        </Button>
      </div>
    </form>
  );
}

function Field({
  label,
  hint,
  error,
  required,
  children,
}: {
  label: string;
  hint?: string;
  error?: string;
  required?: boolean;
  children: ReactNode;
}) {
  return (
    <label className="flex min-w-0 flex-col gap-1">
      <span className="flex items-center gap-1 text-[10.5px] text-muted-foreground">
        {label}
        {required ? <span className="text-danger">*</span> : null}
      </span>
      {children}
      {error ? (
        <span className="text-[10px] text-danger">{error}</span>
      ) : hint ? (
        <span className="text-[9.5px] text-muted-foreground">{hint}</span>
      ) : null}
    </label>
  );
}

function Select({
  value,
  onChange,
  disabled,
  children,
}: {
  value: string;
  onChange: (value: string) => void;
  disabled?: boolean;
  children: ReactNode;
}) {
  return (
    <select
      value={value}
      onChange={(event) => onChange(event.target.value)}
      disabled={disabled}
      className="h-9 w-full rounded-lg border border-border bg-surface px-2 font-mono text-[11px] text-secondary outline-none transition-colors focus:border-primary/40 focus:ring-2 focus:ring-primary/15 disabled:opacity-45"
    >
      {children}
    </select>
  );
}

function readScope(value: unknown): ConsumerPolicyScope {
  return value === "consumer" ? "consumer" : "global";
}

function readAllowlist(document: Record<string, unknown>, key: string): string {
  const value = document[key];
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string").join(", ")
    : "";
}

function stripAllowlists(document: unknown): Record<string, unknown> {
  const preserved = { ...asRecord(document) };
  delete preserved.allowed_models;
  delete preserved.allowed_providers;
  return preserved;
}

function buildDocument(
  preserved: Record<string, unknown>,
  allowedModels: string,
  allowedProviders: string,
): Record<string, unknown> {
  const models = normalizeList(allowedModels);
  const providers = normalizeList(allowedProviders);
  return {
    ...preserved,
    ...(models.length > 0 ? { allowed_models: models } : {}),
    ...(providers.length > 0 ? { allowed_providers: providers } : {}),
  };
}

function normalizeList(value: string): string[] {
  const seen = new Set<string>();
  const items: string[] = [];
  for (const part of value.split(",")) {
    const item = part.trim();
    if (item === "" || seen.has(item)) {
      continue;
    }
    seen.add(item);
    items.push(item);
  }
  return items;
}

function parseDocumentObject(text: string): DocumentParseResult {
  const trimmed = text.trim();
  if (trimmed === "") {
    return { document: null, error: "Document is required." };
  }
  let value: unknown;
  try {
    value = JSON.parse(trimmed);
  } catch (error) {
    return {
      document: null,
      error: `Invalid JSON: ${error instanceof Error ? error.message : "unable to parse"}`,
    };
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return { document: null, error: "Document must be a JSON object." };
  }
  return { document: value as Record<string, unknown>, error: null };
}

function asRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}
