import type { ConsumerGroup, McpAuthStrategy, McpServer, McpTool } from "@octra/types";
import { Button, Input, Switch } from "@octra/ui";
import { useState, type FormEvent, type ReactNode } from "react";

export type McpFormKind = "server" | "tool" | "key" | "acl" | "strategy";

export interface McpTypedFormProps {
  kind: McpFormKind;
  initial: unknown;
  servers: McpServer[];
  tools: McpTool[];
  consumerGroups: ConsumerGroup[];
  authStrategies: McpAuthStrategy[];
  pending: boolean;
  onSave: (body: Record<string, unknown>) => void;
  onCancel: () => void;
  onDelete?: () => void;
}

export function McpTypedForm(props: McpTypedFormProps) {
  switch (props.kind) {
    case "server":
      return <ServerForm {...props} />;
    case "tool":
      return <ToolForm {...props} />;
    case "key":
      return <KeyForm {...props} />;
    case "acl":
      return <AclForm {...props} />;
    case "strategy":
      return <StrategyForm {...props} />;
  }
}

function ServerForm({
  initial,
  servers,
  authStrategies,
  pending,
  onSave,
  onCancel,
  onDelete,
}: McpTypedFormProps) {
  const source = asRecord(initial);
  const [name, setName] = useState(() => readString(source, "name"));
  const [mode, setMode] = useState(() => readServerMode(source.type));
  const [sources, setSources] = useState(() => readSources(source.sources));
  const [routePath, setRoutePath] = useState(() => readString(source, "route_path"));
  const [authStrategyId, setAuthStrategyId] = useState(() =>
    readString(source, "auth_strategy_id"),
  );
  const [aclAllowWhen, setAclAllowWhen] = useState(() => readLines(source.acl_allow_when));
  const [aclDenyWhen, setAclDenyWhen] = useState(() => readLines(source.acl_deny_when));
  const [transport, setTransport] = useState(() => readTransport(source.transport));
  const [command, setCommand] = useState(() => readString(source, "command"));
  const [url, setUrl] = useState(() => readString(source, "url"));
  const [enabled, setEnabled] = useState(() => readBoolean(source, "enabled", true));
  const [errors, setErrors] = useState<Record<string, string>>({});

  const conversion = mode === "conversion-listener" || mode === "conversion-only";
  const stdio = !conversion && transport === "stdio";
  const body: Record<string, unknown> = {
    name: name.trim(),
    type: mode,
    sources: mode === "listener" ? splitSources(sources) : [],
    route_path: routePath.trim() === "" ? null : routePath.trim(),
    auth_strategy_id: authStrategyId === "" ? null : authStrategyId,
    acl_allow_when: splitLines(aclAllowWhen),
    acl_deny_when: splitLines(aclDenyWhen),
    transport,
    command: stdio && command.trim() !== "" ? command.trim() : null,
    url: !stdio && !conversion && url.trim() !== "" ? url.trim() : null,
    enabled,
    config: readObject(source, "config"),
    headers: readObject(source, "headers"),
  };
  const advanced = useAdvancedJson(body);

  function submit() {
    const nextErrors: Record<string, string> = {};
    if (advanced.dirty) {
      const parsed = parseAdvanced(advanced.text, nextErrors);
      if (!parsed) {
        setErrors(nextErrors);
        return;
      }
      setErrors({});
      onSave(parsed);
      return;
    }
    if (name.trim() === "") {
      nextErrors.name = "Name is required.";
    }
    if (routePath.trim() !== "" && !routePath.trim().startsWith("/")) {
      nextErrors.route_path = "Route path must start with /.";
    }
    if (conversion) {
      const config = readObject(source, "config");
      if (!isRecord(config.openapi)) {
        nextErrors.openapi = "Conversion modes require config.openapi (document or document_url).";
      }
    } else if (stdio) {
      if (command.trim() === "") {
        nextErrors.command = "stdio servers require a command.";
      }
    } else {
      const trimmedUrl = url.trim();
      if (trimmedUrl === "") {
        nextErrors.url = "HTTP servers require a URL.";
      } else if (!/^https?:\/\//i.test(trimmedUrl)) {
        nextErrors.url = "URL must start with http:// or https://.";
      }
    }
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    onSave(body);
  }

  return (
    <FormShell
      pending={pending}
      errors={errors}
      onCancel={onCancel}
      onDelete={onDelete}
      onSubmit={submit}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="filesystem"
            aria-invalid={Boolean(errors.name)}
          />
        </Field>
        <Field label="Transport">
          <Select value={transport} onChange={(value) => setTransport(readTransport(value))}>
            <option value="stdio">stdio</option>
            <option value="http">http</option>
          </Select>
        </Field>
        <Field
          label="Mode"
          hint="Kong-shaped server type. Conversion modes read config.openapi."
        >
          <Select value={mode} onChange={setMode}>
            <option value="passthrough-listener">passthrough-listener</option>
            <option value="listener">listener</option>
            <option value="upstream-server">upstream-server</option>
            <option value="conversion-listener">conversion-listener</option>
            <option value="conversion-only">conversion-only</option>
          </Select>
        </Field>
        <Field
          label="Route path"
          hint="Optional; defaults to /mcp/<name>. Must start with /."
          error={errors.route_path}
        >
          <Input
            value={routePath}
            onChange={(event) => setRoutePath(event.target.value)}
            placeholder="/mcp/filesystem"
            className="font-mono text-[11px]"
            aria-invalid={Boolean(errors.route_path)}
          />
        </Field>
        <Field label="Auth strategy" hint="Empty uses the default strategy or MCP API keys.">
          <Select value={authStrategyId} onChange={setAuthStrategyId}>
            <option value="">Default / MCP API keys</option>
            {authStrategyId !== "" &&
            !authStrategies.some((strategy) => strategy.id === authStrategyId) ? (
              <option value={authStrategyId}>{authStrategyId} (missing)</option>
            ) : null}
            {authStrategies.map((strategy) => (
              <option key={strategy.id} value={strategy.id}>
                {strategy.name} · {strategy.kind}
                {strategy.is_default ? " · default" : ""}
              </option>
            ))}
          </Select>
        </Field>
        {mode === "listener" ? (
          <Field
            label="Sources"
            hint={`Comma-separated server names. Empty means all upstream/passthrough servers. Available: ${
              servers.map((server) => server.name).join(", ") || "none"
            }`}
          >
            <Input
              value={sources}
              onChange={(event) => setSources(event.target.value)}
              placeholder="filesystem, github"
              className="font-mono text-[11px]"
            />
          </Field>
        ) : null}
        {!conversion && stdio ? (
          <Field
            label="Command"
            required
            error={errors.command}
            hint="Executable and arguments for the stdio process."
          >
            <Input
              value={command}
              onChange={(event) => setCommand(event.target.value)}
              placeholder="npx -y @modelcontextprotocol/server-filesystem"
              className="font-mono text-[11px]"
              aria-invalid={Boolean(errors.command)}
            />
          </Field>
        ) : !conversion ? (
          <Field
            label="URL"
            required
            error={errors.url}
            hint="HTTP or HTTPS endpoint for the MCP server."
          >
            <Input
              value={url}
              onChange={(event) => setUrl(event.target.value)}
              placeholder="https://mcp.example.com"
              className="font-mono text-[11px]"
              aria-invalid={Boolean(errors.url)}
            />
          </Field>
        ) : null}
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Server enabled" />
          </div>
        </Field>
        <Field
          label="CEL allow_when"
          hint="One expression per line. Deny wins; per-tool ACL rows override."
        >
          <textarea
            value={aclAllowWhen}
            onChange={(event) => setAclAllowWhen(event.target.value)}
            rows={3}
            spellCheck={false}
            placeholder={"consumer_group.names.contains('internal-teams')"}
            className="min-h-[70px] w-full resize-y rounded-lg border border-border bg-surface p-2 font-mono text-[10.5px] text-secondary outline-none focus:border-primary/40 focus:ring-2 focus:ring-primary/15"
          />
        </Field>
        <Field label="CEL deny_when" hint="One expression per line. Evaluated before allow_when.">
          <textarea
            value={aclDenyWhen}
            onChange={(event) => setAclDenyWhen(event.target.value)}
            rows={3}
            spellCheck={false}
            placeholder={"consumer.id == 'blocked-user'"}
            className="min-h-[70px] w-full resize-y rounded-lg border border-border bg-surface p-2 font-mono text-[10.5px] text-secondary outline-none focus:border-primary/40 focus:ring-2 focus:ring-primary/15"
          />
        </Field>
      </div>
      {errors.openapi ? (
        <span className="text-[10px] text-danger">{errors.openapi}</span>
      ) : null}
      <p className="text-[9.5px] text-muted-foreground">
        stdio servers need command; HTTP servers need url. Listener sources are matched by server
        name or id and normalize to names. Conversion modes need config.openapi in the advanced
        body; generated HTTP tools are managed by refresh.
      </p>
      <AdvancedJsonField
        state={advanced}
        error={advanced.dirty ? errors.advanced : undefined}
        hint="headers and config stay in the advanced body."
      />
    </FormShell>
  );
}

function ToolForm({ initial, pending, onSave, onCancel, onDelete, servers }: McpTypedFormProps) {
  const source = asRecord(initial);
  const [serverId, setServerId] = useState(() => readString(source, "server_id"));
  const [name, setName] = useState(() => readString(source, "name"));
  const [description, setDescription] = useState(() => readString(source, "description"));
  const [enabled, setEnabled] = useState(() => readBoolean(source, "enabled", true));
  const [errors, setErrors] = useState<Record<string, string>>({});

  const body: Record<string, unknown> = {
    server_id: serverId.trim(),
    name: name.trim(),
    description: description.trim(),
    enabled,
  };
  if (Object.prototype.hasOwnProperty.call(source, "schema")) {
    body.schema = source.schema;
  }
  const advanced = useAdvancedJson(body);

  function submit() {
    const nextErrors: Record<string, string> = {};
    if (advanced.dirty) {
      const parsed = parseAdvanced(advanced.text, nextErrors);
      if (!parsed) {
        setErrors(nextErrors);
        return;
      }
      setErrors({});
      onSave(parsed);
      return;
    }
    if (serverId.trim() === "") {
      nextErrors.server_id = "Server is required.";
    }
    if (name.trim() === "") {
      nextErrors.name = "Name is required.";
    }
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    onSave(body);
  }

  return (
    <FormShell
      pending={pending}
      errors={errors}
      onCancel={onCancel}
      onDelete={onDelete}
      onSubmit={submit}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Server" required error={errors.server_id}>
          <Select value={serverId} onChange={setServerId}>
            <option value="">Select server…</option>
            {serverId !== "" && !servers.some((server) => server.id === serverId) ? (
              <option value={serverId}>{serverId} (missing)</option>
            ) : null}
            {servers.map((server) => (
              <option key={server.id} value={server.id}>
                {server.name} · {server.id}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="read_file"
            className="font-mono text-[11px]"
            aria-invalid={Boolean(errors.name)}
          />
        </Field>
        <Field label="Description">
          <Input
            value={description}
            onChange={(event) => setDescription(event.target.value)}
            placeholder="What the tool does"
          />
        </Field>
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Tool enabled" />
          </div>
        </Field>
      </div>
      <AdvancedJsonField
        state={advanced}
        error={advanced.dirty ? errors.advanced : undefined}
        hint="schema stays in the advanced body."
      />
    </FormShell>
  );
}

function KeyForm({ initial, pending, onSave, onCancel, onDelete }: McpTypedFormProps) {
  const source = asRecord(initial);
  const locked = readString(source, "id") !== "";
  const [name, setName] = useState(() => readString(source, "name"));
  const [consumerId, setConsumerId] = useState(() => readString(source, "consumer_id"));
  const [consumerName, setConsumerName] = useState(() => readString(source, "consumer_name"));
  const [expiresAt, setExpiresAt] = useState(() => readString(source, "expires_at"));
  const [errors, setErrors] = useState<Record<string, string>>({});

  const body: Record<string, unknown> = {
    name: name.trim(),
    consumer_id: consumerId.trim() === "" ? null : consumerId.trim(),
    consumer_name: consumerName.trim() === "" ? null : consumerName.trim(),
    expires_at: expiresAt.trim() === "" ? null : expiresAt.trim(),
  };
  const advanced = useAdvancedJson(body);

  function submit() {
    if (locked) {
      return;
    }
    const nextErrors: Record<string, string> = {};
    if (advanced.dirty) {
      const parsed = parseAdvanced(advanced.text, nextErrors);
      if (!parsed) {
        setErrors(nextErrors);
        return;
      }
      setErrors({});
      onSave(parsed);
      return;
    }
    if (name.trim() === "") {
      nextErrors.name = "Name is required.";
    }
    if (consumerId.trim() !== "" && consumerName.trim() !== "") {
      nextErrors.consumer_name = "Use either consumer id or consumer name, not both.";
    }
    const trimmedExpiry = expiresAt.trim();
    if (trimmedExpiry !== "" && !/^\d{4}-\d{2}-\d{2}$/.test(trimmedExpiry)) {
      nextErrors.expires_at = "Expiry must use YYYY-MM-DD.";
    }
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    onSave(body);
  }

  return (
    <FormShell
      pending={pending}
      saveDisabled={locked}
      errors={errors}
      onCancel={onCancel}
      onDelete={onDelete}
      onSubmit={submit}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="local-agent"
            disabled={locked}
            aria-invalid={Boolean(errors.name)}
          />
        </Field>
        <Field
          label="Expires at"
          hint="Optional YYYY-MM-DD; empty keys never expire."
          error={errors.expires_at}
        >
          <Input
            value={expiresAt}
            onChange={(event) => setExpiresAt(event.target.value)}
            placeholder="2030-01-01"
            disabled={locked}
            className="font-mono text-[11px]"
            aria-invalid={Boolean(errors.expires_at)}
          />
        </Field>
        <Field label="Consumer id" hint="Existing consumer id; use this or consumer name.">
          <Input
            value={consumerId}
            onChange={(event) => setConsumerId(event.target.value)}
            placeholder="consumer-…"
            disabled={locked}
            className="font-mono text-[11px]"
          />
        </Field>
        <Field
          label="Consumer name"
          hint="Creates a consumer when the name is new."
          error={errors.consumer_name}
        >
          <Input
            value={consumerName}
            onChange={(event) => setConsumerName(event.target.value)}
            placeholder="local-agent"
            disabled={locked}
            aria-invalid={Boolean(errors.consumer_name)}
          />
        </Field>
      </div>
      {locked ? (
        <p className="text-[10px] text-muted-foreground">
          Keys are immutable. Delete this key and create a new one to rotate it.
        </p>
      ) : null}
      <AdvancedJsonField
        state={advanced}
        error={advanced.dirty ? errors.advanced : undefined}
        hint="optional full request body."
      />
    </FormShell>
  );
}

type AclScope = "consumer" | "group" | "default";

function AclForm({
  initial,
  pending,
  onSave,
  onCancel,
  onDelete,
  tools,
  consumerGroups,
}: McpTypedFormProps) {
  const source = asRecord(initial);
  const [toolId, setToolId] = useState(() => readString(source, "tool_id"));
  const [scope, setScope] = useState<AclScope>(() => {
    if (readString(source, "consumer_id") !== "") {
      return "consumer";
    }
    if (readString(source, "consumer_group_id") !== "") {
      return "group";
    }
    return "default";
  });
  const [consumerId, setConsumerId] = useState(() => readString(source, "consumer_id"));
  const [consumerGroupId, setConsumerGroupId] = useState(() =>
    readString(source, "consumer_group_id"),
  );
  const [allowed, setAllowed] = useState(() => readBoolean(source, "allowed", true));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const editing = readString(source, "id") !== "";

  const body: Record<string, unknown> = {
    tool_id: toolId.trim(),
    allowed,
  };
  if (scope === "consumer") {
    body.consumer_id = consumerId.trim();
  } else if (scope === "group") {
    body.consumer_group_id = consumerGroupId.trim();
  }
  const advanced = useAdvancedJson(body);

  function submit() {
    const nextErrors: Record<string, string> = {};
    if (advanced.dirty) {
      const parsed = parseAdvanced(advanced.text, nextErrors);
      if (!parsed) {
        setErrors(nextErrors);
        return;
      }
      setErrors({});
      onSave(parsed);
      return;
    }
    if (toolId.trim() === "") {
      nextErrors.tool_id = "Tool is required.";
    }
    if (scope === "consumer" && consumerId.trim() === "") {
      nextErrors.consumer_id = "Consumer id is required for consumer scope.";
    }
    if (scope === "group" && consumerGroupId.trim() === "") {
      nextErrors.consumer_group_id = "Consumer group id is required for group scope.";
    }
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    onSave(body);
  }

  return (
    <FormShell
      pending={pending}
      errors={errors}
      onCancel={onCancel}
      onDelete={onDelete}
      onSubmit={submit}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Tool" required error={errors.tool_id}>
          <Select value={toolId} onChange={setToolId}>
            <option value="">Select tool…</option>
            {toolId !== "" && !tools.some((tool) => tool.id === toolId) ? (
              <option value={toolId}>{toolId} (missing)</option>
            ) : null}
            {tools.map((tool) => (
              <option key={tool.id} value={tool.id}>
                {tool.qualified_name} · {tool.id}
              </option>
            ))}
          </Select>
        </Field>
        <Field
          label="Scope"
          hint={editing ? "Scope is fixed after creation; only allowed can change." : undefined}
        >
          <Select
            value={scope}
            onChange={(value) => setScope(value as AclScope)}
            disabled={editing}
          >
            <option value="consumer">Consumer</option>
            <option value="group">Consumer group</option>
            <option value="default">Tool default</option>
          </Select>
        </Field>
        {scope === "consumer" ? (
          <Field label="Consumer id" required error={errors.consumer_id}>
            <Input
              value={consumerId}
              onChange={(event) => setConsumerId(event.target.value)}
              placeholder="consumer-…"
              className="font-mono text-[11px]"
              aria-invalid={Boolean(errors.consumer_id)}
            />
          </Field>
        ) : null}
        {scope === "group" ? (
          <Field
            label="Consumer group"
            required
            error={errors.consumer_group_id}
            hint={
              consumerGroups.length === 0
                ? "Group list unavailable; enter a consumer group id."
                : undefined
            }
          >
            {consumerGroups.length > 0 ? (
              <Select value={consumerGroupId} onChange={setConsumerGroupId}>
                <option value="">Select group…</option>
                {consumerGroupId !== "" &&
                !consumerGroups.some((group) => group.id === consumerGroupId) ? (
                  <option value={consumerGroupId}>{consumerGroupId} (missing)</option>
                ) : null}
                {consumerGroups.map((group) => (
                  <option key={group.id} value={group.id}>
                    {group.name} · {group.id}
                  </option>
                ))}
              </Select>
            ) : (
              <Input
                value={consumerGroupId}
                onChange={(event) => setConsumerGroupId(event.target.value)}
                placeholder="group-…"
                className="font-mono text-[11px]"
                aria-invalid={Boolean(errors.consumer_group_id)}
              />
            )}
          </Field>
        ) : null}
        <Field label="Allowed">
          <div className="flex h-9 items-center gap-2">
            <input
              type="checkbox"
              checked={allowed}
              onChange={(event) => setAllowed(event.target.checked)}
              className="h-4 w-4 rounded border-border accent-primary"
            />
            <span className="text-[11px] text-secondary">
              {allowed ? "Allow this scope" : "Deny this scope"}
            </span>
          </div>
        </Field>
      </div>
      <AdvancedJsonField
        state={advanced}
        error={advanced.dirty ? errors.advanced : undefined}
        hint="optional full request body."
      />
    </FormShell>
  );
}

function StrategyForm({ initial, pending, onSave, onCancel, onDelete }: McpTypedFormProps) {
  const source = asRecord(initial);
  const config = isRecord(source.config) ? source.config : {};
  const [name, setName] = useState(() => readString(source, "name"));
  const [issuer, setIssuer] = useState(() => readString(config, "issuer"));
  const [jwksUri, setJwksUri] = useState(() => readString(config, "jwks_uri"));
  const [audience, setAudience] = useState(() => readString(config, "audience"));
  const [scopes, setScopes] = useState(() => readCommaList(config.required_scopes));
  const [consumerClaim, setConsumerClaim] = useState(() =>
    readString(config, "consumer_claim", "sub"),
  );
  const [usernameClaim, setUsernameClaim] = useState(() =>
    readString(config, "username_claim"),
  );
  const [groupsClaim, setGroupsClaim] = useState(() => readString(config, "groups_claim"));
  const [algorithms, setAlgorithms] = useState(() => readCommaList(config.algorithms));
  const [cacheSecs, setCacheSecs] = useState(() => readNumberText(config, "jwks_cache_secs", 300));
  const [enabled, setEnabled] = useState(() => readBoolean(source, "enabled", true));
  const [isDefault, setIsDefault] = useState(() => readBoolean(source, "is_default", false));
  const [errors, setErrors] = useState<Record<string, string>>({});

  const body: Record<string, unknown> = {
    name: name.trim(),
    kind: "oidc",
    enabled,
    is_default: isDefault,
    config: {
      issuer: issuer.trim(),
      jwks_uri: jwksUri.trim(),
      audience: audience.trim() === "" ? null : audience.trim(),
      required_scopes: splitCommaList(scopes),
      consumer_claim: consumerClaim.trim() || "sub",
      username_claim: usernameClaim.trim() === "" ? null : usernameClaim.trim(),
      groups_claim: groupsClaim.trim() === "" ? null : groupsClaim.trim(),
      algorithms: splitCommaList(algorithms),
      jwks_cache_secs: Number(cacheSecs) || 300,
    },
  };
  const advanced = useAdvancedJson(body);

  function submit() {
    const nextErrors: Record<string, string> = {};
    if (advanced.dirty) {
      const parsed = parseAdvanced(advanced.text, nextErrors);
      if (!parsed) {
        setErrors(nextErrors);
        return;
      }
      setErrors({});
      onSave(parsed);
      return;
    }
    if (name.trim() === "") {
      nextErrors.name = "Name is required.";
    }
    if (!/^https?:\/\//i.test(issuer.trim())) {
      nextErrors.issuer = "Issuer must be an absolute http(s) URL.";
    }
    if (!/^https?:\/\//i.test(jwksUri.trim())) {
      nextErrors.jwks_uri = "JWKS URI must be an absolute http(s) URL.";
    }
    const seconds = Number(cacheSecs);
    if (!Number.isFinite(seconds) || seconds < 1 || seconds > 86400) {
      nextErrors.cache = "JWKS cache must be between 1 and 86400 seconds.";
    }
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    onSave(body);
  }

  return (
    <FormShell
      pending={pending}
      errors={errors}
      onCancel={onCancel}
      onDelete={onDelete}
      onSubmit={submit}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="corporate-oidc"
          />
        </Field>
        <Field label="Kind">
          <Select value="oidc" onChange={() => undefined}>
            <option value="oidc">oidc</option>
          </Select>
        </Field>
        <Field label="Issuer" required error={errors.issuer}>
          <Input
            value={issuer}
            onChange={(event) => setIssuer(event.target.value)}
            placeholder="https://idp.example.com"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="JWKS URI" required error={errors.jwks_uri}>
          <Input
            value={jwksUri}
            onChange={(event) => setJwksUri(event.target.value)}
            placeholder="https://idp.example.com/.well-known/jwks.json"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Audience">
          <Input
            value={audience}
            onChange={(event) => setAudience(event.target.value)}
            placeholder="octra-mcp"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Required scopes" hint="Comma-separated. Every scope must be present.">
          <Input
            value={scopes}
            onChange={(event) => setScopes(event.target.value)}
            placeholder="mcp.read, mcp.call"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Consumer claim">
          <Input
            value={consumerClaim}
            onChange={(event) => setConsumerClaim(event.target.value)}
            placeholder="sub"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Username claim">
          <Input
            value={usernameClaim}
            onChange={(event) => setUsernameClaim(event.target.value)}
            placeholder="preferred_username"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Groups claim">
          <Input
            value={groupsClaim}
            onChange={(event) => setGroupsClaim(event.target.value)}
            placeholder="groups"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Algorithms" hint="Comma-separated; empty uses RS256 and ES256.">
          <Input
            value={algorithms}
            onChange={(event) => setAlgorithms(event.target.value)}
            placeholder="RS256, ES256"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="JWKS cache seconds" error={errors.cache}>
          <Input
            type="number"
            min={1}
            max={86400}
            value={cacheSecs}
            onChange={(event) => setCacheSecs(event.target.value)}
          />
        </Field>
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Strategy enabled" />
          </div>
        </Field>
        <Field label="Default">
          <div className="flex h-9 items-center gap-2">
            <input
              type="checkbox"
              checked={isDefault}
              onChange={(event) => setIsDefault(event.target.checked)}
              className="h-4 w-4 rounded border-border accent-primary"
            />
            <span className="text-[11px] text-secondary">
              Use when a server does not select a strategy
            </span>
          </div>
        </Field>
      </div>
      <AdvancedJsonField
        state={advanced}
        error={advanced.dirty ? errors.advanced : undefined}
        hint="optional full request body."
      />
    </FormShell>
  );
}

function FormShell({
  pending,
  saveDisabled = false,
  errors,
  onCancel,
  onDelete,
  onSubmit,
  children,
}: {
  pending: boolean;
  saveDisabled?: boolean;
  errors: Record<string, string>;
  onCancel: () => void;
  onDelete?: () => void;
  onSubmit: () => void;
  children: ReactNode;
}) {
  const summary = [...new Set(Object.values(errors))];

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    onSubmit();
  }

  return (
    <form onSubmit={handleSubmit} className="flex flex-col gap-3">
      {summary.length > 0 ? (
        <div className="rounded-lg border border-danger/30 bg-danger/5 px-3 py-2 text-[10.5px] leading-relaxed text-danger">
          {summary.join(" ")}
        </div>
      ) : null}
      {children}
      <div className="flex flex-wrap items-center gap-2">
        <Button
          size="sm"
          type="submit"
          disabled={pending || saveDisabled}
          title={
            saveDisabled ? "API keys cannot be updated; delete and create a new key." : undefined
          }
        >
          {pending ? "Saving…" : "Save"}
        </Button>
        <Button size="sm" variant="secondary" type="button" disabled={pending} onClick={onCancel}>
          Cancel
        </Button>
        <Button
          size="sm"
          variant="danger"
          type="button"
          className="ml-auto"
          disabled={pending || !onDelete}
          onClick={onDelete}
        >
          Delete
        </Button>
      </div>
    </form>
  );
}

function AdvancedJsonField({
  state,
  error,
  hint,
}: {
  state: AdvancedJsonState;
  error?: string;
  hint: string;
}) {
  return (
    <div className="rounded-lg border border-border bg-surface/40">
      <div className="flex flex-wrap items-center justify-between gap-2 px-3 py-2">
        <button
          type="button"
          aria-expanded={state.open}
          onClick={state.toggle}
          className="text-left text-[10.5px] font-medium text-secondary transition-colors hover:text-foreground"
        >
          Advanced JSON
          <span className="ml-1 font-normal text-muted-foreground">· {hint}</span>
        </button>
        {state.dirty ? (
          <button
            type="button"
            onClick={state.reset}
            className="text-[10px] text-muted-foreground underline-offset-2 hover:text-secondary hover:underline"
          >
            Reset to typed fields
          </button>
        ) : null}
      </div>
      {state.open ? (
        <div className="flex flex-col gap-1.5 px-3 pb-3">
          <textarea
            value={state.text}
            onChange={(event) => state.edit(event.target.value)}
            spellCheck={false}
            rows={8}
            aria-invalid={Boolean(error)}
            className="min-h-[150px] w-full resize-y rounded-lg border border-border bg-surface p-3 font-mono text-[10.5px] leading-relaxed text-secondary outline-none transition-colors placeholder:text-muted-foreground focus:border-primary/40 focus:ring-2 focus:ring-primary/15"
          />
          {error ? (
            <span className="text-[10px] text-danger">{error}</span>
          ) : (
            <span className="text-[9.5px] text-muted-foreground">
              When edited, this object is sent as the entire request body.
            </span>
          )}
        </div>
      ) : null}
    </div>
  );
}

interface AdvancedJsonState {
  open: boolean;
  dirty: boolean;
  text: string;
  toggle: () => void;
  edit: (value: string) => void;
  reset: () => void;
}

function useAdvancedJson(body: Record<string, unknown>): AdvancedJsonState {
  const [open, setOpen] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [draft, setDraft] = useState("");

  return {
    open,
    dirty,
    text: dirty ? draft : JSON.stringify(body, null, 2),
    toggle: () => setOpen((value) => !value),
    edit: (value) => {
      setDirty(true);
      setDraft(value);
    },
    reset: () => {
      setDirty(false);
      setDraft("");
    },
  };
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

function parseAdvanced(
  text: string,
  errors: Record<string, string>,
): Record<string, unknown> | null {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (error) {
    errors.advanced = `Invalid JSON: ${error instanceof Error ? error.message : "unable to parse"}`;
    return null;
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    errors.advanced = "Advanced JSON must be a JSON object.";
    return null;
  }
  return value as Record<string, unknown>;
}

function asRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function readString(
  source: Record<string, unknown>,
  key: string,
  fallback = "",
): string {
  const value = source[key];
  return typeof value === "string" ? value : fallback;
}

function readNumberText(source: Record<string, unknown>, key: string, fallback: number): string {
  const value = source[key];
  return typeof value === "number" && Number.isFinite(value) ? String(value) : String(fallback);
}

function readCommaList(value: unknown): string {
  return Array.isArray(value)
    ? value.filter((entry): entry is string => typeof entry === "string").join(", ")
    : "";
}

function splitCommaList(value: string): string[] {
  return [
    ...new Set(
      value
        .split(",")
        .map((entry) => entry.trim())
        .filter(Boolean),
    ),
  ];
}

function readBoolean(source: Record<string, unknown>, key: string, fallback: boolean): boolean {
  const value = source[key];
  return typeof value === "boolean" ? value : fallback;
}

function readObject(source: Record<string, unknown>, key: string): Record<string, unknown> {
  const value = source[key];
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function readTransport(value: unknown): "stdio" | "http" {
  return value === "http" ? "http" : "stdio";
}

function readServerMode(value: unknown): string {
  if (
    value === "listener" ||
    value === "upstream-server" ||
    value === "conversion-listener" ||
    value === "conversion-only"
  ) {
    return value;
  }
  return "passthrough-listener";
}

function readLines(value: unknown): string {
  return Array.isArray(value)
    ? value.filter((entry): entry is string => typeof entry === "string").join("\n")
    : "";
}

function splitLines(value: string): string[] {
  return [...new Set(value.split("\n").map((entry) => entry.trim()).filter(Boolean))];
}

function readSources(value: unknown): string {
  return Array.isArray(value)
    ? value.filter((entry): entry is string => typeof entry === "string").join(", ")
    : "";
}

function splitSources(value: string): string[] {
  return [
    ...new Set(
      value
        .split(",")
        .map((entry) => entry.trim())
        .filter(Boolean),
    ),
  ];
}
