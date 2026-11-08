import type {
  AiModelInput,
  AiModelKind,
  AiModelTargetInput,
  AiOAuthFlow,
  AiOAuthStatus,
  AiPolicyAttachmentInput,
  AiPolicyInput,
  AiPolicyScope,
  AiPolicyType,
  AiProviderAccountInput,
  AiProviderInput,
} from "@octra/types";
import { Badge, Button, Input, Switch } from "@octra/ui";
import { useState, type FormEvent, type ReactNode } from "react";

import { controlClient } from "../../lib/control-api";

const MODEL_KINDS: ReadonlyArray<{ value: AiModelKind; label: string }> = [
  { value: "model", label: "Model" },
  { value: "group", label: "Group" },
];

const POLICY_TYPES: ReadonlyArray<{ value: AiPolicyType; label: string }> = [
  { value: "allowlist", label: "Allowlist" },
  { value: "rate_limit", label: "Rate limit" },
  { value: "budget_guard", label: "Budget guard" },
  { value: "pii_redaction", label: "PII redaction" },
  { value: "prompt_guard", label: "Prompt guard" },
  { value: "prompt_compress", label: "Prompt compress" },
  { value: "semantic_cache", label: "Semantic cache" },
  { value: "model_map", label: "Model map" },
  { value: "param_override", label: "Param override" },
  { value: "system_prompt", label: "System prompt" },
  { value: "request_audit", label: "Request audit" },
  { value: "think_tags", label: "Think tags" },
];

const CREDENTIAL_KINDS: ReadonlyArray<{ value: string; label: string }> = [
  { value: "static", label: "Static" },
  { value: "oauth", label: "OAuth" },
];

const QUOTA_UNITS: ReadonlyArray<{ value: string; label: string }> = [
  { value: "requests", label: "Requests" },
  { value: "tokens", label: "Tokens" },
  { value: "cost_micros", label: "Cost (micros)" },
];

const QUOTA_PERIODS: ReadonlyArray<{ value: string; label: string }> = [
  { value: "none", label: "None" },
  { value: "minute", label: "Minute" },
  { value: "hour", label: "Hour" },
  { value: "day", label: "Day" },
  { value: "month", label: "Month" },
];

const TARGET_KINDS = [
  { value: "provider", label: "Provider" },
  { value: "nested", label: "Nested model" },
] as const;

type TargetKind = (typeof TARGET_KINDS)[number]["value"];

const ATTACHMENT_SCOPES: ReadonlyArray<{ value: AiPolicyScope; label: string }> = [
  { value: "global", label: "Global" },
  { value: "provider", label: "Provider" },
  { value: "model", label: "Model" },
  { value: "target", label: "Target" },
  { value: "consumer_group", label: "Consumer group" },
  { value: "consumer", label: "Consumer" },
  { value: "virtual_key", label: "Virtual key" },
];

export interface SelectorOption {
  id: string;
  name: string;
}

export interface TypedFormProps {
  title: string;
  initial: unknown;
  pending: boolean;
  onSave: (body: unknown) => void;
  onCancel: () => void;
  onOAuthChanged?: () => void;
  providers?: SelectorOption[];
  models?: SelectorOption[];
  accounts?: SelectorOption[];
  targets?: Array<{ id: string }>;
}

export function ProviderForm({ title, initial, pending, onSave, onCancel }: TypedFormProps) {
  const source = asRecord(initial);
  const [name, setName] = useState(readString(source, "name", "openai"));
  const [protocol, setProtocol] = useState(readString(source, "protocol", "openai"));
  const [baseUrl, setBaseUrl] = useState(readString(source, "base_url"));
  const [authRef, setAuthRef] = useState(readString(source, "auth_ref"));
  const [capabilities, setCapabilities] = useState(readList(source, "capabilities", ["chat"]));
  const [config, setConfig] = useState(readJson(source, "config", {}));
  const [enabled, setEnabled] = useState(readBoolean(source, "enabled", true));
  const [errors, setErrors] = useState<Record<string, string>>({});

  function submit() {
    const nextErrors: Record<string, string> = {};
    const trimmedName = name.trim();
    const trimmedProtocol = protocol.trim();
    if (!trimmedName) {
      nextErrors.name = "Name is required.";
    }
    if (!trimmedProtocol) {
      nextErrors.protocol = "Protocol is required.";
    }
    const configValue = parseJsonText(config, "config", nextErrors);
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    const body: AiProviderInput = {
      name: trimmedName,
      protocol: trimmedProtocol,
      base_url: baseUrl.trim(),
      auth_ref: authRef.trim() === "" ? null : authRef.trim(),
      capabilities: splitList(capabilities),
      config: configValue,
      enabled,
    };
    onSave(body);
  }

  return (
    <TypedFormShell title={title} pending={pending} onCancel={onCancel} onSubmit={submit}>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="openai"
            aria-invalid={Boolean(errors.name)}
          />
        </Field>
        <Field label="Protocol" required error={errors.protocol}>
          <Input
            value={protocol}
            onChange={(event) => setProtocol(event.target.value)}
            placeholder="openai"
            aria-invalid={Boolean(errors.protocol)}
          />
        </Field>
        <Field label="Base URL">
          <Input
            value={baseUrl}
            onChange={(event) => setBaseUrl(event.target.value)}
            placeholder="https://api.openai.com"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Auth ref">
          <Input
            value={authRef}
            onChange={(event) => setAuthRef(event.target.value)}
            placeholder="env:OPENAI_API_KEY"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Capabilities" hint="Comma-separated, for example chat, embeddings.">
          <Input
            value={capabilities}
            onChange={(event) => setCapabilities(event.target.value)}
            placeholder="chat"
          />
        </Field>
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Provider enabled" />
          </div>
        </Field>
      </div>
      <Field
        label="Config (JSON)"
        hint="Advanced: provider-specific configuration object."
        error={errors.config}
      >
        <JsonTextarea value={config} onChange={setConfig} rows={6} />
      </Field>
    </TypedFormShell>
  );
}

export function ModelForm({ title, initial, pending, onSave, onCancel }: TypedFormProps) {
  const source = asRecord(initial);
  const balancer = asRecord(source.balancer);
  const [name, setName] = useState(readString(source, "name", "chat"));
  const [kind, setKind] = useState(readModelKind(source.kind));
  const [capabilities, setCapabilities] = useState(readList(source, "capabilities", ["chat"]));
  const [formats, setFormats] = useState(readList(source, "formats", ["openai"]));
  const [routePaths, setRoutePaths] = useState(
    readList(source, "route_paths", ["/v1/chat/completions"]),
  );
  const [strategy, setStrategy] = useState(readString(balancer, "strategy", "priority"));
  const [retries, setRetries] = useState(readNumber(balancer, "retries", 0));
  const [failoverCriteria, setFailoverCriteria] = useState(readList(balancer, "failover_criteria"));
  const [enabled, setEnabled] = useState(readBoolean(source, "enabled", true));
  const [errors, setErrors] = useState<Record<string, string>>({});

  function submit() {
    const nextErrors: Record<string, string> = {};
    const trimmedName = name.trim();
    const trimmedStrategy = strategy.trim();
    if (!trimmedName) {
      nextErrors.name = "Name is required.";
    }
    if (!trimmedStrategy) {
      nextErrors.strategy = "Balancer strategy is required.";
    }
    const retriesValue = retries.trim() === "" ? 0 : Number(retries);
    if (!Number.isInteger(retriesValue) || retriesValue < 0 || retriesValue > 4_294_967_295) {
      nextErrors.retries = "Retries must be a non-negative integer.";
    }
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    const body: AiModelInput = {
      name: trimmedName,
      kind,
      capabilities: splitList(capabilities),
      formats: splitList(formats),
      route_paths: splitList(routePaths),
      balancer: {
        strategy: trimmedStrategy,
        retries: retriesValue,
        failover_criteria: splitList(failoverCriteria),
      },
      enabled,
    };
    onSave(body);
  }

  return (
    <TypedFormShell title={title} pending={pending} onCancel={onCancel} onSubmit={submit}>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="chat"
            aria-invalid={Boolean(errors.name)}
          />
        </Field>
        <Field label="Kind">
          <Select value={kind} onChange={(value) => setKind(value === "group" ? "group" : "model")}>
            {MODEL_KINDS.map((item) => (
              <option key={item.value} value={item.value}>
                {item.label}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Capabilities" hint="Comma-separated capabilities such as chat.">
          <Input
            value={capabilities}
            onChange={(event) => setCapabilities(event.target.value)}
            placeholder="chat"
          />
        </Field>
        <Field label="Formats" hint="Comma-separated wire formats such as openai.">
          <Input
            value={formats}
            onChange={(event) => setFormats(event.target.value)}
            placeholder="openai"
          />
        </Field>
        <Field label="Route paths" hint="Comma-separated paths such as /v1/chat/completions.">
          <Input
            value={routePaths}
            onChange={(event) => setRoutePaths(event.target.value)}
            placeholder="/v1/chat/completions"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Balancer strategy" required error={errors.strategy}>
          <Input
            value={strategy}
            onChange={(event) => setStrategy(event.target.value)}
            placeholder="priority"
            aria-invalid={Boolean(errors.strategy)}
          />
        </Field>
        <Field label="Balancer retries" error={errors.retries}>
          <Input
            type="number"
            min={0}
            step={1}
            value={retries}
            onChange={(event) => setRetries(event.target.value)}
            placeholder="0"
            aria-invalid={Boolean(errors.retries)}
          />
        </Field>
        <Field
          label="Failover criteria"
          hint="Comma-separated criteria such as timeout, status_5xx."
        >
          <Input
            value={failoverCriteria}
            onChange={(event) => setFailoverCriteria(event.target.value)}
            placeholder="timeout, status_5xx"
          />
        </Field>
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Model enabled" />
          </div>
        </Field>
      </div>
    </TypedFormShell>
  );
}

export function PolicyForm({ title, initial, pending, onSave, onCancel }: TypedFormProps) {
  const source = asRecord(initial);
  const [name, setName] = useState(readString(source, "name", "rate-limit"));
  const [policyType, setPolicyType] = useState(readPolicyType(source.policy_type));
  const [config, setConfig] = useState(
    readJson(source, "config", { minute: 60, limit_by: "virtual_key" }),
  );
  const [enabled, setEnabled] = useState(readBoolean(source, "enabled", true));
  const [errors, setErrors] = useState<Record<string, string>>({});

  function submit() {
    const nextErrors: Record<string, string> = {};
    const trimmedName = name.trim();
    if (!trimmedName) {
      nextErrors.name = "Name is required.";
    }
    const configValue = parseJsonText(config, "config", nextErrors);
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    const body: AiPolicyInput = {
      name: trimmedName,
      policy_type: policyType,
      config: configValue,
      enabled,
    };
    onSave(body);
  }

  return (
    <TypedFormShell title={title} pending={pending} onCancel={onCancel} onSubmit={submit}>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="rate-limit"
            aria-invalid={Boolean(errors.name)}
          />
        </Field>
        <Field label="Policy type">
          <Select value={policyType} onChange={(value) => setPolicyType(readPolicyType(value))}>
            {POLICY_TYPES.map((item) => (
              <option key={item.value} value={item.value}>
                {item.label}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Policy enabled" />
          </div>
        </Field>
      </div>
      <Field
        label="Config (JSON)"
        hint="Advanced: policy-specific configuration object."
        error={errors.config}
      >
        <JsonTextarea value={config} onChange={setConfig} rows={8} />
      </Field>
    </TypedFormShell>
  );
}

export function AccountForm({
  title,
  initial,
  pending,
  onSave,
  onCancel,
  onOAuthChanged,
}: TypedFormProps) {
  const source = asRecord(initial);
  const accountId = typeof source.id === "string" ? source.id : "";
  const [name, setName] = useState(readString(source, "name", "default"));
  const [credentialKind, setCredentialKind] = useState(
    readOption(source.credential_kind, CREDENTIAL_KINDS, "static"),
  );
  const [secretRef, setSecretRef] = useState(readString(source, "secret_ref"));
  const [oauthRef, setOauthRef] = useState(readString(source, "oauth_ref"));
  const [weight, setWeight] = useState(readNumber(source, "weight", 1));
  const [priority, setPriority] = useState(readNumber(source, "priority", 0));
  const [quotaLimit, setQuotaLimit] = useState(readOptionalNumber(source, "quota_limit"));
  const [quotaUnit, setQuotaUnit] = useState(
    readOption(source.quota_unit, QUOTA_UNITS, "requests"),
  );
  const [quotaPeriod, setQuotaPeriod] = useState(
    readOption(source.quota_period, QUOTA_PERIODS, "none"),
  );
  const [quotaUsed, setQuotaUsed] = useState(readNumber(source, "quota_used", 0));
  const [enabled, setEnabled] = useState(readBoolean(source, "enabled", true));
  const [config, setConfig] = useState(readJson(source, "config", {}));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [oauthStatus, setOauthStatus] = useState(readString(source, "oauth_status", "none"));
  const [oauthExpiresAt, setOauthExpiresAt] = useState(
    readNullableString(source, "oauth_expires_at"),
  );
  const [oauthFlow, setOauthFlow] = useState<AiOAuthFlow | null>(null);
  const [oauthCode, setOauthCode] = useState("");
  const [oauthAction, setOauthAction] = useState<string | null>(null);
  const [oauthError, setOauthError] = useState<string | null>(null);
  const oauthBusy = oauthAction !== null;
  const showOAuth = accountId !== "";

  function submit() {
    const nextErrors: Record<string, string> = {};
    const trimmedName = name.trim();
    if (!trimmedName) {
      nextErrors.name = "Name is required.";
    }
    const weightValue = parseCount(weight, "weight", nextErrors, 1);
    const priorityValue = parseCount(priority, "priority", nextErrors, 0);
    const quotaLimitValue = parseOptionalCount(quotaLimit, "quota_limit", nextErrors);
    const quotaUsedValue = parseCount(quotaUsed, "quota_used", nextErrors, 0);
    const configValue = parseJsonObjectText(config, "config", nextErrors);
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    const body: AiProviderAccountInput = {
      name: trimmedName,
      credential_kind: credentialKind,
      secret_ref: secretRef.trim() === "" ? null : secretRef.trim(),
      oauth_ref: oauthRef.trim() === "" ? null : oauthRef.trim(),
      weight: weightValue,
      priority: priorityValue,
      quota_limit: quotaLimitValue,
      quota_unit: quotaUnit,
      quota_period: quotaPeriod,
      quota_used: quotaUsedValue,
      config: configValue,
      enabled,
    };
    onSave(body);
  }

  function applyOAuthStatus(next: AiOAuthStatus) {
    setOauthStatus(next.oauth_status || "none");
    setOauthExpiresAt(next.oauth_expires_at);
  }

  async function startOAuth() {
    if (!accountId || oauthBusy) {
      return;
    }
    setOauthAction("start");
    setOauthError(null);
    try {
      const flow = await controlClient().aiAccountOAuthStart(accountId);
      setOauthFlow(flow);
      setOauthCode("");
      try {
        window.open(flow.authorize_url, "_blank", "noopener,noreferrer");
      } catch {
        // The authorize URL stays visible for manual copy when the popup is blocked.
      }
    } catch (error) {
      setOauthError(oauthErrorMessage(error));
    } finally {
      setOauthAction(null);
    }
  }

  async function completeOAuth() {
    if (!accountId || !oauthFlow || oauthBusy) {
      return;
    }
    const code = oauthCode.trim();
    if (code === "") {
      setOauthError("Authorization code is required.");
      return;
    }
    setOauthAction("complete");
    setOauthError(null);
    try {
      const next = await controlClient().aiAccountOAuthComplete(accountId, {
        code,
        state: oauthFlow.state,
      });
      applyOAuthStatus(next);
      setOauthFlow(null);
      setOauthCode("");
      onOAuthChanged?.();
    } catch (error) {
      setOauthError(oauthErrorMessage(error));
    } finally {
      setOauthAction(null);
    }
  }

  async function refreshOAuth() {
    if (!accountId || oauthBusy) {
      return;
    }
    setOauthAction("refresh");
    setOauthError(null);
    try {
      const next = await controlClient().aiAccountOAuthRefresh(accountId);
      applyOAuthStatus(next);
      onOAuthChanged?.();
    } catch (error) {
      setOauthError(oauthErrorMessage(error));
    } finally {
      setOauthAction(null);
    }
  }

  async function clearOAuth() {
    if (!accountId || oauthBusy) {
      return;
    }
    setOauthAction("clear");
    setOauthError(null);
    try {
      await controlClient().aiAccountOAuthClear(accountId);
      setOauthStatus("none");
      setOauthExpiresAt(null);
      setOauthFlow(null);
      setOauthCode("");
      onOAuthChanged?.();
    } catch (error) {
      setOauthError(oauthErrorMessage(error));
    } finally {
      setOauthAction(null);
    }
  }

  return (
    <TypedFormShell title={title} pending={pending} onCancel={onCancel} onSubmit={submit}>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="default"
            aria-invalid={Boolean(errors.name)}
          />
        </Field>
        <Field label="Credential kind">
          <Select value={credentialKind} onChange={setCredentialKind}>
            {CREDENTIAL_KINDS.map((item) => (
              <option key={item.value} value={item.value}>
                {item.label}
              </option>
            ))}
          </Select>
        </Field>
        <Field
          label="Secret ref"
          hint={credentialKind === "static" ? "Reference such as env:OPENAI_API_KEY." : undefined}
        >
          <Input
            value={secretRef}
            onChange={(event) => setSecretRef(event.target.value)}
            placeholder="env:OPENAI_API_KEY"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="OAuth ref" hint="Optional OAuth credential reference.">
          <Input
            value={oauthRef}
            onChange={(event) => setOauthRef(event.target.value)}
            placeholder="oauth:main"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Weight" error={errors.weight}>
          <Input
            type="number"
            min={0}
            step={1}
            value={weight}
            onChange={(event) => setWeight(event.target.value)}
            placeholder="1"
            aria-invalid={Boolean(errors.weight)}
          />
        </Field>
        <Field label="Priority" error={errors.priority}>
          <Input
            type="number"
            min={0}
            step={1}
            value={priority}
            onChange={(event) => setPriority(event.target.value)}
            placeholder="0"
            aria-invalid={Boolean(errors.priority)}
          />
        </Field>
        <Field label="Quota limit" hint="Leave empty for unlimited." error={errors.quota_limit}>
          <Input
            type="number"
            min={0}
            step={1}
            value={quotaLimit}
            onChange={(event) => setQuotaLimit(event.target.value)}
            placeholder="Unlimited"
            aria-invalid={Boolean(errors.quota_limit)}
          />
        </Field>
        <Field label="Quota unit">
          <Select value={quotaUnit} onChange={setQuotaUnit}>
            {QUOTA_UNITS.map((item) => (
              <option key={item.value} value={item.value}>
                {item.label}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Quota period">
          <Select value={quotaPeriod} onChange={setQuotaPeriod}>
            {QUOTA_PERIODS.map((item) => (
              <option key={item.value} value={item.value}>
                {item.label}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Quota used" error={errors.quota_used}>
          <Input
            type="number"
            min={0}
            step={1}
            value={quotaUsed}
            onChange={(event) => setQuotaUsed(event.target.value)}
            placeholder="0"
            aria-invalid={Boolean(errors.quota_used)}
          />
        </Field>
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Account enabled" />
          </div>
        </Field>
      </div>
      <Field
        label="Config (JSON)"
        hint="Advanced: account-specific configuration object."
        error={errors.config}
      >
        <JsonTextarea value={config} onChange={setConfig} rows={5} />
      </Field>
      {showOAuth ? (
        <div className="flex flex-col gap-2 rounded-lg border border-border bg-surface/60 p-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="flex flex-wrap items-center gap-2 text-[10.5px] text-muted-foreground">
              OAuth
              <Badge variant={oauthStatusVariant(oauthStatus)}>
                {oauthStatusLabel(oauthStatus)}
              </Badge>
              <span>{oauthExpiresAt ? `expires ${oauthExpiresAt}` : "no expiry recorded"}</span>
            </span>
            <div className="flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                variant="secondary"
                disabled={pending || oauthBusy}
                onClick={() => void startOAuth()}
              >
                {oauthAction === "start" ? "Starting…" : "Start OAuth"}
              </Button>
              <Button
                size="sm"
                variant="secondary"
                disabled={pending || oauthBusy}
                onClick={() => void refreshOAuth()}
              >
                {oauthAction === "refresh" ? "Refreshing…" : "Refresh"}
              </Button>
              <Button
                size="sm"
                variant="danger"
                disabled={pending || oauthBusy}
                onClick={() => void clearOAuth()}
              >
                {oauthAction === "clear" ? "Clearing…" : "Clear"}
              </Button>
            </div>
          </div>
          {oauthFlow ? (
            <div className="flex flex-col gap-2">
              <span className="text-[10px] text-muted-foreground">
                Authorize URL (copy manually if the browser did not open):
              </span>
              <Input
                readOnly
                value={oauthFlow.authorize_url}
                onFocus={(event) => event.currentTarget.select()}
                className="font-mono text-[10.5px]"
              />
              <div className="flex flex-col gap-2 sm:flex-row sm:items-end">
                <div className="min-w-0 flex-1">
                  <Field label="Authorization code">
                    <Input
                      value={oauthCode}
                      onChange={(event) => setOauthCode(event.target.value)}
                      placeholder="Paste the code from the provider"
                      autoComplete="off"
                      className="font-mono text-[11px]"
                    />
                  </Field>
                </div>
                <Button
                  size="sm"
                  disabled={pending || oauthBusy}
                  onClick={() => void completeOAuth()}
                >
                  {oauthAction === "complete" ? "Completing…" : "Complete"}
                </Button>
              </div>
            </div>
          ) : null}
          {oauthError ? <span className="text-[10px] text-danger">{oauthError}</span> : null}
        </div>
      ) : null}
    </TypedFormShell>
  );
}

export function TargetForm({
  title,
  initial,
  pending,
  onSave,
  onCancel,
  providers = [],
  models = [],
  accounts = [],
}: TypedFormProps) {
  const source = asRecord(initial);
  const [kind, setKind] = useState(readTargetKind(source));
  const [providerId, setProviderId] = useState(readString(source, "provider_id"));
  const [upstreamModel, setUpstreamModel] = useState(readString(source, "upstream_model"));
  const [accountId, setAccountId] = useState(readString(source, "account_id"));
  const [targetModelId, setTargetModelId] = useState(readString(source, "target_model_id"));
  const [weight, setWeight] = useState(readNumber(source, "weight", 1));
  const [priority, setPriority] = useState(readNumber(source, "priority", 0));
  const [position, setPosition] = useState(readNumber(source, "position", 0));
  const [semanticDescription, setSemanticDescription] = useState(
    readString(source, "semantic_description"),
  );
  const [enabled, setEnabled] = useState(readBoolean(source, "enabled", true));
  const [config, setConfig] = useState(readJson(source, "config", {}));
  const [contextWindow, setContextWindow] = useState(readOptionalNumber(source, "context_window"));
  const [maxOutputTokens, setMaxOutputTokens] = useState(
    readOptionalNumber(source, "max_output_tokens"),
  );
  const [capabilities, setCapabilities] = useState(readJson(source, "capabilities", {}));
  const [pricing, setPricing] = useState(readJson(source, "pricing", {}));
  const [errors, setErrors] = useState<Record<string, string>>({});

  function changeKind(next: TargetKind) {
    setKind(next);
    if (next === "provider") {
      setTargetModelId("");
    } else {
      setProviderId("");
      setUpstreamModel("");
      setAccountId("");
    }
  }

  function submit() {
    const nextErrors: Record<string, string> = {};
    const trimmedProviderId = providerId.trim();
    const trimmedUpstreamModel = upstreamModel.trim();
    const trimmedTargetModelId = targetModelId.trim();
    if (kind === "provider") {
      if (!trimmedProviderId) {
        nextErrors.provider_id = "Provider is required.";
      }
      if (!trimmedUpstreamModel) {
        nextErrors.upstream_model = "Upstream model is required.";
      }
    } else if (!trimmedTargetModelId) {
      nextErrors.target_model_id = "Target model is required.";
    }
    const weightValue = parseCount(weight, "weight", nextErrors, 1);
    const priorityValue = parseCount(priority, "priority", nextErrors, 0);
    const positionValue = parseCount(position, "position", nextErrors, 0);
    const contextWindowValue = parseOptionalCount(contextWindow, "context_window", nextErrors);
    const maxOutputTokensValue = parseOptionalCount(
      maxOutputTokens,
      "max_output_tokens",
      nextErrors,
    );
    const configValue = parseJsonObjectText(config, "config", nextErrors);
    const capabilitiesValue = parseJsonObjectText(capabilities, "capabilities", nextErrors);
    const pricingValue = parseJsonObjectText(pricing, "pricing", nextErrors);
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    const shared = {
      weight: weightValue,
      priority: priorityValue,
      position: positionValue,
      config: configValue,
      context_window: contextWindowValue,
      max_output_tokens: maxOutputTokensValue,
      capabilities: capabilitiesValue,
      pricing: pricingValue,
      semantic_description: semanticDescription.trim() === "" ? null : semanticDescription.trim(),
      enabled,
    };
    const body: AiModelTargetInput =
      kind === "provider"
        ? {
            target_model_id: null,
            provider_id: trimmedProviderId,
            upstream_model: trimmedUpstreamModel,
            account_id: accountId === "" ? null : accountId,
            ...shared,
          }
        : {
            target_model_id: trimmedTargetModelId,
            provider_id: null,
            upstream_model: null,
            account_id: null,
            ...shared,
          };
    onSave(body);
  }

  return (
    <TypedFormShell title={title} pending={pending} onCancel={onCancel} onSubmit={submit}>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Target kind">
          <Select
            value={kind}
            onChange={(value) => changeKind(value === "nested" ? "nested" : "provider")}
          >
            {TARGET_KINDS.map((item) => (
              <option key={item.value} value={item.value}>
                {item.label}
              </option>
            ))}
          </Select>
        </Field>
        {kind === "provider" ? (
          <>
            <Field label="Provider" required error={errors.provider_id}>
              <Select value={providerId} onChange={setProviderId}>
                <option value="">Select provider…</option>
                {providerId !== "" && !providers.some((item) => item.id === providerId) ? (
                  <option value={providerId}>{providerId}</option>
                ) : null}
                {providers.map((item) => (
                  <option key={item.id} value={item.id}>
                    {item.name}
                  </option>
                ))}
              </Select>
            </Field>
            <Field label="Upstream model" required error={errors.upstream_model}>
              <Input
                value={upstreamModel}
                onChange={(event) => setUpstreamModel(event.target.value)}
                placeholder="gpt-4o-mini"
                className="font-mono text-[11px]"
                aria-invalid={Boolean(errors.upstream_model)}
              />
            </Field>
            <Field label="Account" hint="Optional; accounts loaded for the selected provider.">
              <Select value={accountId} onChange={setAccountId}>
                <option value="">No account</option>
                {accountId !== "" && !accounts.some((item) => item.id === accountId) ? (
                  <option value={accountId}>{accountId}</option>
                ) : null}
                {accounts.map((item) => (
                  <option key={item.id} value={item.id}>
                    {item.name}
                  </option>
                ))}
              </Select>
            </Field>
          </>
        ) : (
          <Field label="Target model" required error={errors.target_model_id}>
            <Select value={targetModelId} onChange={setTargetModelId}>
              <option value="">Select model…</option>
              {targetModelId !== "" && !models.some((item) => item.id === targetModelId) ? (
                <option value={targetModelId}>{targetModelId}</option>
              ) : null}
              {models.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.name}
                </option>
              ))}
            </Select>
          </Field>
        )}
        <Field label="Weight" error={errors.weight}>
          <Input
            type="number"
            min={0}
            step={1}
            value={weight}
            onChange={(event) => setWeight(event.target.value)}
            placeholder="1"
            aria-invalid={Boolean(errors.weight)}
          />
        </Field>
        <Field label="Priority" error={errors.priority}>
          <Input
            type="number"
            min={0}
            step={1}
            value={priority}
            onChange={(event) => setPriority(event.target.value)}
            placeholder="0"
            aria-invalid={Boolean(errors.priority)}
          />
        </Field>
        <Field label="Position" error={errors.position}>
          <Input
            type="number"
            min={0}
            step={1}
            value={position}
            onChange={(event) => setPosition(event.target.value)}
            placeholder="0"
            aria-invalid={Boolean(errors.position)}
          />
        </Field>
        <Field
          label="Context window"
          hint="Optional upstream context window in tokens."
          error={errors.context_window}
        >
          <Input
            type="number"
            min={0}
            step={1}
            value={contextWindow}
            onChange={(event) => setContextWindow(event.target.value)}
            placeholder="Optional"
            aria-invalid={Boolean(errors.context_window)}
          />
        </Field>
        <Field
          label="Max output tokens"
          hint="Optional maximum output tokens for this upstream model."
          error={errors.max_output_tokens}
        >
          <Input
            type="number"
            min={0}
            step={1}
            value={maxOutputTokens}
            onChange={(event) => setMaxOutputTokens(event.target.value)}
            placeholder="Optional"
            aria-invalid={Boolean(errors.max_output_tokens)}
          />
        </Field>
        <Field
          label="Semantic description"
          hint="Describe when routing should prefer this target; used by the semantic strategy."
        >
          <Input
            value={semanticDescription}
            onChange={(event) => setSemanticDescription(event.target.value)}
            placeholder="Handles short factual questions"
          />
        </Field>
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Target enabled" />
          </div>
        </Field>
      </div>
      <Field
        label="Config (JSON)"
        hint="Advanced: target-specific configuration object."
        error={errors.config}
      >
        <JsonTextarea value={config} onChange={setConfig} rows={5} />
      </Field>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field
          label="Capabilities (JSON)"
          hint="Advanced: capability flags such as vision, tools, or reasoning."
          error={errors.capabilities}
        >
          <JsonTextarea value={capabilities} onChange={setCapabilities} rows={4} />
        </Field>
        <Field
          label="Pricing (JSON)"
          hint="Advanced: pricing document such as input_per_million and output_per_million."
          error={errors.pricing}
        >
          <JsonTextarea value={pricing} onChange={setPricing} rows={4} />
        </Field>
      </div>
    </TypedFormShell>
  );
}

export function AttachmentForm({ title, initial, pending, onSave, onCancel }: TypedFormProps) {
  const source = asRecord(initial);
  const [scope, setScope] = useState(readAttachmentScope(source.scope));
  const [scopeId, setScopeId] = useState(readString(source, "scope_id"));
  const [order, setOrder] = useState(readNumber(source, "order", 0));
  const [enabled, setEnabled] = useState(readBoolean(source, "enabled", true));
  const [errors, setErrors] = useState<Record<string, string>>({});

  function changeScope(next: AiPolicyScope) {
    setScope(next);
    setScopeId("");
  }

  function submit() {
    const nextErrors: Record<string, string> = {};
    const trimmedScopeId = scopeId.trim();
    if (scope !== "global" && !trimmedScopeId) {
      nextErrors.scope_id = "Scope id is required for this scope.";
    }
    const orderValue = parseCount(order, "order", nextErrors, 0);
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    const body: AiPolicyAttachmentInput = {
      scope,
      scope_id: scope === "global" ? null : trimmedScopeId,
      order: orderValue,
      enabled,
    };
    onSave(body);
  }

  return (
    <TypedFormShell title={title} pending={pending} onCancel={onCancel} onSubmit={submit}>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Scope">
          <Select value={scope} onChange={(value) => changeScope(readAttachmentScope(value))}>
            {ATTACHMENT_SCOPES.map((item) => (
              <option key={item.value} value={item.value}>
                {item.label}
              </option>
            ))}
          </Select>
        </Field>
        <Field
          label="Scope id"
          hint={scopeIdHint(scope)}
          error={errors.scope_id}
          required={scope !== "global"}
        >
          <Input
            value={scope === "global" ? "" : scopeId}
            onChange={(event) => setScopeId(event.target.value)}
            placeholder={scope === "global" ? "Not used for global scope" : "Scope id"}
            disabled={scope === "global"}
            className="font-mono text-[11px]"
            aria-invalid={Boolean(errors.scope_id)}
          />
        </Field>
        <Field label="Order" error={errors.order}>
          <Input
            type="number"
            min={0}
            step={1}
            value={order}
            onChange={(event) => setOrder(event.target.value)}
            placeholder="0"
            aria-invalid={Boolean(errors.order)}
          />
        </Field>
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Attachment enabled" />
          </div>
        </Field>
      </div>
    </TypedFormShell>
  );
}

function TypedFormShell({
  title,
  pending,
  onCancel,
  onSubmit,
  children,
}: {
  title: string;
  pending: boolean;
  onCancel: () => void;
  onSubmit: () => void;
  children: ReactNode;
}) {
  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    onSubmit();
  }

  return (
    <form
      onSubmit={handleSubmit}
      className="flex flex-col gap-3 border-b border-border bg-surface/40 p-4"
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="text-[11px] font-medium text-foreground">{title}</span>
        <div className="flex items-center gap-2">
          <Button size="sm" variant="secondary" disabled={pending} onClick={onCancel}>
            Cancel
          </Button>
          <Button size="sm" type="submit" disabled={pending}>
            {pending ? "Saving…" : "Save"}
          </Button>
        </div>
      </div>
      {children}
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

function JsonTextarea({
  value,
  onChange,
  rows,
}: {
  value: string;
  onChange: (value: string) => void;
  rows: number;
}) {
  return (
    <textarea
      value={value}
      onChange={(event) => onChange(event.target.value)}
      spellCheck={false}
      rows={rows}
      className="min-h-[120px] w-full resize-y rounded-lg border border-border bg-surface p-3 font-mono text-[10.5px] leading-relaxed text-secondary outline-none transition-colors placeholder:text-muted-foreground focus:border-primary/40 focus:ring-2 focus:ring-primary/15"
    />
  );
}

function asRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function readString(source: Record<string, unknown>, key: string, fallback = ""): string {
  const value = source[key];
  return typeof value === "string" ? value : fallback;
}

function readNullableString(source: Record<string, unknown>, key: string): string | null {
  const value = source[key];
  return typeof value === "string" && value !== "" ? value : null;
}

function readBoolean(source: Record<string, unknown>, key: string, fallback: boolean): boolean {
  const value = source[key];
  return typeof value === "boolean" ? value : fallback;
}

function readNumber(source: Record<string, unknown>, key: string, fallback: number): string {
  const value = source[key];
  return typeof value === "number" && Number.isFinite(value) ? String(value) : String(fallback);
}

function readOptionalNumber(source: Record<string, unknown>, key: string): string {
  const value = source[key];
  return typeof value === "number" && Number.isFinite(value) ? String(value) : "";
}

function readOption<T extends string>(
  value: unknown,
  options: ReadonlyArray<{ value: T }>,
  fallback: T,
): T {
  return options.some((item) => item.value === value) ? (value as T) : fallback;
}

function readList(source: Record<string, unknown>, key: string, fallback: string[] = []): string {
  const value = source[key];
  if (Array.isArray(value)) {
    return value.filter((item): item is string => typeof item === "string").join(", ");
  }
  return fallback.join(", ");
}

function readJson(source: Record<string, unknown>, key: string, fallback: unknown): string {
  const value = source[key];
  return JSON.stringify(value === undefined || value === null ? fallback : value, null, 2);
}

function readModelKind(value: unknown): AiModelKind {
  return value === "group" ? "group" : "model";
}

function readPolicyType(value: unknown): AiPolicyType {
  return readOption(value, POLICY_TYPES, "rate_limit");
}

function readTargetKind(source: Record<string, unknown>): TargetKind {
  const targetModelId = source.target_model_id;
  return typeof targetModelId === "string" && targetModelId !== "" ? "nested" : "provider";
}

function readAttachmentScope(value: unknown): AiPolicyScope {
  return readOption(value, ATTACHMENT_SCOPES, "global");
}

function scopeIdHint(scope: AiPolicyScope): string {
  switch (scope) {
    case "global":
      return "Global attachments ignore the scope id.";
    case "provider":
      return "Provider id.";
    case "model":
      return "Model id.";
    case "target":
      return "Target id.";
    case "consumer_group":
      return "Consumer group id.";
    case "consumer":
      return "Consumer id.";
    case "virtual_key":
      return "Virtual key id.";
  }
}

function parseCount(
  text: string,
  field: string,
  errors: Record<string, string>,
  fallback: number,
): number {
  const trimmed = text.trim();
  if (trimmed === "") {
    return fallback;
  }
  return parseCountValue(trimmed, field, errors) ?? fallback;
}

function parseOptionalCount(
  text: string,
  field: string,
  errors: Record<string, string>,
): number | null {
  const trimmed = text.trim();
  if (trimmed === "") {
    return null;
  }
  return parseCountValue(trimmed, field, errors);
}

function parseCountValue(
  trimmed: string,
  field: string,
  errors: Record<string, string>,
): number | null {
  const value = Number(trimmed);
  if (!Number.isInteger(value) || value < 0 || value > 4_294_967_295) {
    errors[field] = "Must be a non-negative integer.";
    return null;
  }
  return value;
}

function parseJsonObjectText(text: string, field: string, errors: Record<string, string>): unknown {
  const value = parseJsonText(text, field, errors);
  if (errors[field]) {
    return value;
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    errors[field] = "Must be a JSON object.";
    return {};
  }
  return value;
}

function splitList(value: string): string[] {
  return value
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

function oauthStatusVariant(status: string): "accent" | "neutral" | "warning" {
  switch (status) {
    case "authorized":
      return "accent";
    case "reauth_required":
      return "warning";
    default:
      return "neutral";
  }
}

function oauthStatusLabel(status: string): string {
  switch (status) {
    case "none":
      return "not connected";
    case "authorized":
      return "authorized";
    case "reauth_required":
      return "reauth required";
    default:
      return status || "unknown";
  }
}

function oauthErrorMessage(error: unknown): string {
  return error instanceof Error && error.message ? error.message : "OAuth request failed.";
}

function parseJsonText(text: string, field: string, errors: Record<string, string>): unknown {
  const trimmed = text.trim();
  if (trimmed === "") {
    return {};
  }
  try {
    return JSON.parse(trimmed);
  } catch (error) {
    errors[field] = `Invalid JSON: ${error instanceof Error ? error.message : "unable to parse"}`;
    return {};
  }
}
