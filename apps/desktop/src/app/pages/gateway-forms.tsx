import type { Service, Upstream } from "@octra/types";
import { Button, Input, Switch } from "@octra/ui";
import { Trash2 } from "lucide-react";
import { useState, type FormEvent, type ReactNode } from "react";

export type GatewayEntityKind = "service" | "route" | "upstream" | "target";

export interface GatewayTypedFormProps {
  kind: GatewayEntityKind;
  initial: unknown;
  services: Service[];
  upstreams: Upstream[];
  pending: boolean;
  onSave: (body: Record<string, unknown>) => void;
  onCancel: () => void;
  onDelete?: () => void;
}

const SERVICE_PROTOCOLS = ["http", "https", "grpc", "grpcs"] as const;

const ROUTE_PROTOCOLS = [
  "http",
  "https",
  "grpc",
  "grpcs",
  "ws",
  "wss",
  "tcp",
  "tls",
  "udp",
  "udt",
] as const;

const ROUTE_METHODS = [
  "GET",
  "HEAD",
  "POST",
  "PUT",
  "PATCH",
  "DELETE",
  "OPTIONS",
  "CONNECT",
  "TRACE",
] as const;

const ALGORITHMS = [
  { value: "round-robin", label: "Round robin" },
  { value: "consistent-hashing", label: "Consistent hashing" },
  { value: "least-connections", label: "Least connections" },
  { value: "latency", label: "Latency" },
  { value: "weighted-round-robin", label: "Weighted round robin" },
] as const;

const HASH_ON_VALUES = [
  "none",
  "consumer",
  "ip",
  "header",
  "cookie",
  "path",
  "query_arg",
  "uri_capture",
] as const;

export function GatewayTypedForm(props: GatewayTypedFormProps) {
  switch (props.kind) {
    case "service":
      return <ServiceForm {...props} />;
    case "route":
      return <RouteForm {...props} />;
    case "upstream":
      return <UpstreamForm {...props} />;
    case "target":
      return <TargetForm {...props} />;
  }
}

function ServiceForm({ initial, pending, onSave, onCancel, onDelete }: GatewayTypedFormProps) {
  const source = asRecord(initial);
  const [name, setName] = useState(readString(source, "name"));
  const [protocol, setProtocol] = useState(readOption(source.protocol, SERVICE_PROTOCOLS, "http"));
  const [host, setHost] = useState(readString(source, "host"));
  const [port, setPort] = useState(readNumberText(source, "port", 80));
  const [path, setPath] = useState(readString(source, "path"));
  const [retries, setRetries] = useState(readNumberText(source, "retries", 5));
  const [connectTimeout, setConnectTimeout] = useState(
    readNumberText(source, "connect_timeout", 60_000),
  );
  const [writeTimeout, setWriteTimeout] = useState(readNumberText(source, "write_timeout", 60_000));
  const [readTimeout, setReadTimeout] = useState(readNumberText(source, "read_timeout", 60_000));
  const [enabled, setEnabled] = useState(readBoolean(source, "enabled", true));
  const [description, setDescription] = useState(readString(source, "description"));
  const [tags, setTags] = useState(readList(source, "tags"));
  const [errors, setErrors] = useState<Record<string, string>>({});

  const typed = serviceBody({
    name,
    protocol,
    host,
    port: numericDraft(port),
    path,
    retries: numericDraft(retries),
    connectTimeout: numericDraft(connectTimeout),
    writeTimeout: numericDraft(writeTimeout),
    readTimeout: numericDraft(readTimeout),
    enabled,
    description,
    tags,
  });
  const advanced = useAdvancedJson(typed);

  function submit() {
    if (advanced.dirty) {
      if (!advanced.body) {
        return;
      }
      setErrors({});
      onSave(advanced.body);
      return;
    }
    const nextErrors: Record<string, string> = {};
    const trimmedName = name.trim();
    if (!trimmedName) {
      nextErrors.name = "Name is required.";
    }
    const portValue = parseInteger(port, "port", "Port", nextErrors, { min: 1, max: 65535 });
    const retriesValue = parseInteger(retries, "retries", "Retries", nextErrors, { min: 0 });
    const connectValue = parseInteger(
      connectTimeout,
      "connect_timeout",
      "Connect timeout",
      nextErrors,
      {
        min: 1,
      },
    );
    const writeValue = parseInteger(writeTimeout, "write_timeout", "Write timeout", nextErrors, {
      min: 1,
    });
    const readValue = parseInteger(readTimeout, "read_timeout", "Read timeout", nextErrors, {
      min: 1,
    });
    if (
      Object.keys(nextErrors).length > 0 ||
      portValue === null ||
      retriesValue === null ||
      connectValue === null ||
      writeValue === null ||
      readValue === null
    ) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    onSave(
      serviceBody({
        name,
        protocol,
        host,
        port: portValue,
        path,
        retries: retriesValue,
        connectTimeout: connectValue,
        writeTimeout: writeValue,
        readTimeout: readValue,
        enabled,
        description,
        tags,
      }),
    );
  }

  return (
    <TypedFormShell
      pending={pending}
      onCancel={onCancel}
      onDelete={onDelete}
      onSubmit={submit}
      errors={errors}
      jsonError={advanced.error}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="example-service"
            aria-invalid={Boolean(errors.name)}
          />
        </Field>
        <Field label="Protocol">
          <Select
            value={protocol}
            onChange={(value) => setProtocol(readOption(value, SERVICE_PROTOCOLS, "http"))}
          >
            {SERVICE_PROTOCOLS.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Host" hint="Defaults to the service name when left empty.">
          <Input
            value={host}
            onChange={(event) => setHost(event.target.value)}
            placeholder="api.internal"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Port" error={errors.port}>
          <Input
            type="number"
            min={1}
            max={65535}
            step={1}
            value={port}
            onChange={(event) => setPort(event.target.value)}
            placeholder="80"
            aria-invalid={Boolean(errors.port)}
          />
        </Field>
        <Field label="Path" hint="Optional upstream base path.">
          <Input
            value={path}
            onChange={(event) => setPath(event.target.value)}
            placeholder="/v1"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Retries" error={errors.retries}>
          <Input
            type="number"
            min={0}
            step={1}
            value={retries}
            onChange={(event) => setRetries(event.target.value)}
            placeholder="5"
            aria-invalid={Boolean(errors.retries)}
          />
        </Field>
        <Field label="Connect timeout (ms)" error={errors.connect_timeout}>
          <Input
            type="number"
            min={1}
            step={1}
            value={connectTimeout}
            onChange={(event) => setConnectTimeout(event.target.value)}
            placeholder="60000"
            aria-invalid={Boolean(errors.connect_timeout)}
          />
        </Field>
        <Field label="Write timeout (ms)" error={errors.write_timeout}>
          <Input
            type="number"
            min={1}
            step={1}
            value={writeTimeout}
            onChange={(event) => setWriteTimeout(event.target.value)}
            placeholder="60000"
            aria-invalid={Boolean(errors.write_timeout)}
          />
        </Field>
        <Field label="Read timeout (ms)" error={errors.read_timeout}>
          <Input
            type="number"
            min={1}
            step={1}
            value={readTimeout}
            onChange={(event) => setReadTimeout(event.target.value)}
            placeholder="60000"
            aria-invalid={Boolean(errors.read_timeout)}
          />
        </Field>
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Service enabled" />
          </div>
        </Field>
        <Field label="Tags" hint="Comma-separated tags.">
          <Input
            value={tags}
            onChange={(event) => setTags(event.target.value)}
            placeholder="edge, public"
          />
        </Field>
        <Field label="Description">
          <Input
            value={description}
            onChange={(event) => setDescription(event.target.value)}
            placeholder="Public edge service"
          />
        </Field>
      </div>
      <AdvancedJsonPanel advanced={advanced} />
    </TypedFormShell>
  );
}

function RouteForm({
  initial,
  services,
  pending,
  onSave,
  onCancel,
  onDelete,
}: GatewayTypedFormProps) {
  const source = asRecord(initial);
  const [name, setName] = useState(readString(source, "name"));
  const [serviceId, setServiceId] = useState(
    readString(source, "service_id") || readRelationId(source, "service"),
  );
  const [protocols, setProtocols] = useState(readList(source, "protocols", ["http", "https"]));
  const [methods, setMethods] = useState(readList(source, "methods"));
  const [hosts, setHosts] = useState(readList(source, "hosts"));
  const [paths, setPaths] = useState(readList(source, "paths"));
  const [stripPath, setStripPath] = useState(readBoolean(source, "strip_path", true));
  const [preserveHost, setPreserveHost] = useState(readBoolean(source, "preserve_host", false));
  const [regexPriority, setRegexPriority] = useState(readNumberText(source, "regex_priority", 0));
  const [enabled, setEnabled] = useState(readBoolean(source, "enabled", true));
  const [tags, setTags] = useState(readList(source, "tags"));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const headers = isRecord(source.headers) ? source.headers : {};

  const typed = routeBody({
    name,
    serviceId,
    protocols,
    methods,
    hosts,
    paths,
    headers,
    stripPath,
    preserveHost,
    regexPriority: numericDraft(regexPriority),
    enabled,
    tags,
  });
  const advanced = useAdvancedJson(typed);

  function submit() {
    if (advanced.dirty) {
      if (!advanced.body) {
        return;
      }
      setErrors({});
      onSave(advanced.body);
      return;
    }
    const nextErrors: Record<string, string> = {};
    const protocolList = splitList(protocols).map((value) => value.toLowerCase());
    for (const protocol of protocolList) {
      if (!ROUTE_PROTOCOLS.includes(protocol as (typeof ROUTE_PROTOCOLS)[number])) {
        nextErrors.protocols = `Unknown protocol '${protocol}'.`;
        break;
      }
    }
    const methodList = splitList(methods).map((value) => value.toUpperCase());
    for (const method of methodList) {
      if (!ROUTE_METHODS.includes(method as (typeof ROUTE_METHODS)[number])) {
        nextErrors.methods = `Unknown HTTP method '${method}'.`;
        break;
      }
    }
    const pathList = splitList(paths);
    if (pathList.some((value) => !value.startsWith("/") && !value.startsWith("~"))) {
      nextErrors.paths = "Paths must start with '/' or '~'.";
    }
    const regexPriorityValue = parseInteger(
      regexPriority,
      "regex_priority",
      "Regex priority",
      nextErrors,
    );
    if (Object.keys(nextErrors).length > 0 || regexPriorityValue === null) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    onSave(
      routeBody({
        name,
        serviceId,
        protocols,
        methods,
        hosts,
        paths,
        headers,
        stripPath,
        preserveHost,
        regexPriority: regexPriorityValue,
        enabled,
        tags,
      }),
    );
  }

  return (
    <TypedFormShell
      pending={pending}
      onCancel={onCancel}
      onDelete={onDelete}
      onSubmit={submit}
      errors={errors}
      jsonError={advanced.error}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" hint="Optional route name.">
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="public-api"
          />
        </Field>
        <Field label="Service">
          <Select value={serviceId} onChange={setServiceId}>
            <option value="">No service</option>
            {serviceId !== "" && !services.some((service) => service.id === serviceId) ? (
              <option value={serviceId}>{serviceId}</option>
            ) : null}
            {services.map((service) => (
              <option key={service.id} value={service.id}>
                {service.name} · {service.id}
              </option>
            ))}
          </Select>
        </Field>
        <Field
          label="Protocols"
          error={errors.protocols}
          hint="Comma-separated protocols; empty matches every scheme."
        >
          <Input
            value={protocols}
            onChange={(event) => setProtocols(event.target.value)}
            placeholder="http, https"
            className="font-mono text-[11px]"
            aria-invalid={Boolean(errors.protocols)}
          />
        </Field>
        <Field
          label="Methods"
          hint="Comma-separated methods; empty matches any method."
          error={errors.methods}
        >
          <Input
            value={methods}
            onChange={(event) => setMethods(event.target.value)}
            placeholder="GET, POST"
            className="font-mono text-[11px]"
            aria-invalid={Boolean(errors.methods)}
          />
        </Field>
        <Field label="Hosts" hint="Comma-separated hosts; empty matches any host.">
          <Input
            value={hosts}
            onChange={(event) => setHosts(event.target.value)}
            placeholder="api.example.com"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field
          label="Paths"
          hint="Comma-separated paths starting with / or ~."
          error={errors.paths}
        >
          <Input
            value={paths}
            onChange={(event) => setPaths(event.target.value)}
            placeholder="/v1, /health"
            className="font-mono text-[11px]"
            aria-invalid={Boolean(errors.paths)}
          />
        </Field>
        <Field label="Regex priority" error={errors.regex_priority}>
          <Input
            type="number"
            step={1}
            value={regexPriority}
            onChange={(event) => setRegexPriority(event.target.value)}
            placeholder="0"
            aria-invalid={Boolean(errors.regex_priority)}
          />
        </Field>
        <Field label="Tags" hint="Comma-separated tags.">
          <Input
            value={tags}
            onChange={(event) => setTags(event.target.value)}
            placeholder="edge, public"
          />
        </Field>
        <Field label="Strip path">
          <div className="flex h-9 items-center">
            <Switch checked={stripPath} onCheckedChange={setStripPath} label="Strip path" />
          </div>
        </Field>
        <Field label="Preserve host">
          <div className="flex h-9 items-center">
            <Switch
              checked={preserveHost}
              onCheckedChange={setPreserveHost}
              label="Preserve host"
            />
          </div>
        </Field>
        <Field label="Enabled">
          <div className="flex h-9 items-center">
            <Switch checked={enabled} onCheckedChange={setEnabled} label="Route enabled" />
          </div>
        </Field>
      </div>
      <AdvancedJsonPanel advanced={advanced} />
    </TypedFormShell>
  );
}

function UpstreamForm({ initial, pending, onSave, onCancel, onDelete }: GatewayTypedFormProps) {
  const source = asRecord(initial);
  const [name, setName] = useState(readString(source, "name"));
  const [algorithm, setAlgorithm] = useState(readAlgorithm(source.algorithm));
  const [hashOn, setHashOn] = useState(readHashOn(source.hash_on, "none"));
  const [hashFallback, setHashFallback] = useState(readHashOn(source.hash_fallback, "none"));
  const [hashOnHeader, setHashOnHeader] = useState(
    readString(source, "hash_on_header", "X-Consumer-ID"),
  );
  const [hashFallbackHeader, setHashFallbackHeader] = useState(
    readString(source, "hash_fallback_header", "X-Consumer-ID"),
  );
  const [hashOnCookie, setHashOnCookie] = useState(readString(source, "hash_on_cookie", "kong"));
  const [hashOnCookiePath, setHashOnCookiePath] = useState(
    readString(source, "hash_on_cookie_path", "/"),
  );
  const [hashOnQueryArg, setHashOnQueryArg] = useState(readString(source, "hash_on_query_arg"));
  const [hashFallbackQueryArg, setHashFallbackQueryArg] = useState(
    readString(source, "hash_fallback_query_arg"),
  );
  const [hashOnUriCapture, setHashOnUriCapture] = useState(
    readString(source, "hash_on_uri_capture"),
  );
  const [hashFallbackUriCapture, setHashFallbackUriCapture] = useState(
    readString(source, "hash_fallback_uri_capture"),
  );
  const [slots, setSlots] = useState(readNumberText(source, "slots", 10_000));
  const [hostHeader, setHostHeader] = useState(readString(source, "host_header"));
  const [tags, setTags] = useState(readList(source, "tags"));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const healthchecks = isRecord(source.healthchecks) ? source.healthchecks : {};

  const typed = upstreamBody({
    name,
    algorithm,
    hashOn,
    hashFallback,
    hashOnHeader,
    hashFallbackHeader,
    hashOnCookie,
    hashOnCookiePath,
    hashOnQueryArg,
    hashFallbackQueryArg,
    hashOnUriCapture,
    hashFallbackUriCapture,
    slots: numericDraft(slots),
    healthchecks,
    hostHeader,
    tags,
  });
  const advanced = useAdvancedJson(typed);

  function submit() {
    if (advanced.dirty) {
      if (!advanced.body) {
        return;
      }
      setErrors({});
      onSave(advanced.body);
      return;
    }
    const nextErrors: Record<string, string> = {};
    const trimmedName = name.trim();
    if (!trimmedName) {
      nextErrors.name = "Name is required.";
    }
    const slotsValue = parseInteger(slots, "slots", "Slots", nextErrors, { min: 1 });
    if (Object.keys(nextErrors).length > 0 || slotsValue === null) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    onSave(
      upstreamBody({
        name,
        algorithm,
        hashOn,
        hashFallback,
        hashOnHeader,
        hashFallbackHeader,
        hashOnCookie,
        hashOnCookiePath,
        hashOnQueryArg,
        hashFallbackQueryArg,
        hashOnUriCapture,
        hashFallbackUriCapture,
        slots: slotsValue,
        healthchecks,
        hostHeader,
        tags,
      }),
    );
  }

  return (
    <TypedFormShell
      pending={pending}
      onCancel={onCancel}
      onDelete={onDelete}
      onSubmit={submit}
      errors={errors}
      jsonError={advanced.error}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" required error={errors.name}>
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="primary-pool"
            aria-invalid={Boolean(errors.name)}
          />
        </Field>
        <Field label="Algorithm">
          <Select value={algorithm} onChange={(value) => setAlgorithm(readAlgorithm(value))}>
            {ALGORITHMS.map((item) => (
              <option key={item.value} value={item.value}>
                {item.label}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Hash on">
          <Select value={hashOn} onChange={(value) => setHashOn(readHashOn(value, "none"))}>
            {HASH_ON_VALUES.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Hash fallback">
          <Select
            value={hashFallback}
            onChange={(value) => setHashFallback(readHashOn(value, "none"))}
          >
            {HASH_ON_VALUES.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Hash on header">
          <Input
            value={hashOnHeader}
            onChange={(event) => setHashOnHeader(event.target.value)}
            placeholder="X-Consumer-ID"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Hash fallback header">
          <Input
            value={hashFallbackHeader}
            onChange={(event) => setHashFallbackHeader(event.target.value)}
            placeholder="X-Consumer-ID"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Hash on cookie">
          <Input
            value={hashOnCookie}
            onChange={(event) => setHashOnCookie(event.target.value)}
            placeholder="kong"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Hash on cookie path">
          <Input
            value={hashOnCookiePath}
            onChange={(event) => setHashOnCookiePath(event.target.value)}
            placeholder="/"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Hash on query arg">
          <Input
            value={hashOnQueryArg}
            onChange={(event) => setHashOnQueryArg(event.target.value)}
            placeholder="session"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Hash fallback query arg">
          <Input
            value={hashFallbackQueryArg}
            onChange={(event) => setHashFallbackQueryArg(event.target.value)}
            placeholder="session"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Hash on URI capture">
          <Input
            value={hashOnUriCapture}
            onChange={(event) => setHashOnUriCapture(event.target.value)}
            placeholder="tenant"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Hash fallback URI capture">
          <Input
            value={hashFallbackUriCapture}
            onChange={(event) => setHashFallbackUriCapture(event.target.value)}
            placeholder="tenant"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Slots" error={errors.slots}>
          <Input
            type="number"
            min={1}
            step={1}
            value={slots}
            onChange={(event) => setSlots(event.target.value)}
            placeholder="10000"
            aria-invalid={Boolean(errors.slots)}
          />
        </Field>
        <Field label="Host header" hint="Optional upstream Host header override.">
          <Input
            value={hostHeader}
            onChange={(event) => setHostHeader(event.target.value)}
            placeholder="api.internal"
            className="font-mono text-[11px]"
          />
        </Field>
        <Field label="Tags" hint="Comma-separated tags.">
          <Input
            value={tags}
            onChange={(event) => setTags(event.target.value)}
            placeholder="edge"
          />
        </Field>
      </div>
      <AdvancedJsonPanel advanced={advanced} />
    </TypedFormShell>
  );
}

function TargetForm({
  initial,
  upstreams,
  pending,
  onSave,
  onCancel,
  onDelete,
}: GatewayTypedFormProps) {
  const source = asRecord(initial);
  const [upstreamId, setUpstreamId] = useState(
    readString(source, "upstream_id") || readRelationId(source, "upstream"),
  );
  const [target, setTarget] = useState(readString(source, "target"));
  const [weight, setWeight] = useState(readNumberText(source, "weight", 100));
  const [tags, setTags] = useState(readList(source, "tags"));
  const [errors, setErrors] = useState<Record<string, string>>({});

  const typed = targetBody({
    upstreamId,
    target,
    weight: numericDraft(weight),
    tags,
  });
  const advanced = useAdvancedJson(typed);

  function submit() {
    if (advanced.dirty) {
      if (!advanced.body) {
        return;
      }
      setErrors({});
      onSave(advanced.body);
      return;
    }
    const nextErrors: Record<string, string> = {};
    if (!upstreamId) {
      nextErrors.upstream_id = "Upstream is required.";
    }
    const trimmedTarget = target.trim();
    if (!trimmedTarget) {
      nextErrors.target = "Target is required.";
    } else if (/\s/.test(trimmedTarget)) {
      nextErrors.target = "Target must not contain whitespace.";
    } else if (trimmedTarget.includes("://")) {
      nextErrors.target = "Target must not include a scheme.";
    }
    const weightValue = parseInteger(weight, "weight", "Weight", nextErrors, {
      min: 0,
      max: 1000,
    });
    if (Object.keys(nextErrors).length > 0 || weightValue === null) {
      setErrors(nextErrors);
      return;
    }
    setErrors({});
    onSave(
      targetBody({
        upstreamId,
        target,
        weight: weightValue,
        tags,
      }),
    );
  }

  return (
    <TypedFormShell
      pending={pending}
      onCancel={onCancel}
      onDelete={onDelete}
      onSubmit={submit}
      errors={errors}
      jsonError={advanced.error}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Upstream" required error={errors.upstream_id}>
          <Select value={upstreamId} onChange={setUpstreamId}>
            <option value="">Select upstream…</option>
            {upstreamId !== "" && !upstreams.some((upstream) => upstream.id === upstreamId) ? (
              <option value={upstreamId}>{upstreamId}</option>
            ) : null}
            {upstreams.map((upstream) => (
              <option key={upstream.id} value={upstream.id}>
                {upstream.name} · {upstream.id}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Target" required error={errors.target} hint="host:port, without a scheme.">
          <Input
            value={target}
            onChange={(event) => setTarget(event.target.value)}
            placeholder="10.0.0.12:8080"
            className="font-mono text-[11px]"
            aria-invalid={Boolean(errors.target)}
          />
        </Field>
        <Field label="Weight" error={errors.weight}>
          <Input
            type="number"
            min={0}
            max={1000}
            step={1}
            value={weight}
            onChange={(event) => setWeight(event.target.value)}
            placeholder="100"
            aria-invalid={Boolean(errors.weight)}
          />
        </Field>
        <Field label="Tags" hint="Comma-separated tags.">
          <Input
            value={tags}
            onChange={(event) => setTags(event.target.value)}
            placeholder="blue"
          />
        </Field>
      </div>
      <AdvancedJsonPanel advanced={advanced} />
    </TypedFormShell>
  );
}

function serviceBody(input: {
  name: string;
  protocol: string;
  host: string;
  port: number;
  path: string;
  retries: number;
  connectTimeout: number;
  writeTimeout: number;
  readTimeout: number;
  enabled: boolean;
  description: string;
  tags: string;
}): Record<string, unknown> {
  const name = input.name.trim();
  const host = input.host.trim();
  const path = input.path.trim();
  return {
    name,
    protocol: input.protocol,
    host: host || name,
    port: input.port,
    path: path === "" ? null : path,
    retries: input.retries,
    connect_timeout: input.connectTimeout,
    write_timeout: input.writeTimeout,
    read_timeout: input.readTimeout,
    enabled: input.enabled,
    description: input.description.trim(),
    tags: splitList(input.tags),
  };
}

function routeBody(input: {
  name: string;
  serviceId: string;
  protocols: string;
  methods: string;
  hosts: string;
  paths: string;
  headers: Record<string, unknown>;
  stripPath: boolean;
  preserveHost: boolean;
  regexPriority: number;
  enabled: boolean;
  tags: string;
}): Record<string, unknown> {
  const name = input.name.trim();
  return {
    name: name === "" ? null : name,
    service_id: input.serviceId === "" ? null : input.serviceId,
    protocols: splitList(input.protocols).map((value) => value.toLowerCase()),
    methods: splitList(input.methods).map((value) => value.toUpperCase()),
    hosts: splitList(input.hosts),
    paths: splitList(input.paths),
    headers: input.headers,
    strip_path: input.stripPath,
    preserve_host: input.preserveHost,
    regex_priority: input.regexPriority,
    enabled: input.enabled,
    tags: splitList(input.tags),
  };
}

function upstreamBody(input: {
  name: string;
  algorithm: string;
  hashOn: string;
  hashFallback: string;
  hashOnHeader: string;
  hashFallbackHeader: string;
  hashOnCookie: string;
  hashOnCookiePath: string;
  hashOnQueryArg: string;
  hashFallbackQueryArg: string;
  hashOnUriCapture: string;
  hashFallbackUriCapture: string;
  slots: number;
  healthchecks: Record<string, unknown>;
  hostHeader: string;
  tags: string;
}): Record<string, unknown> {
  const hostHeader = input.hostHeader.trim();
  return {
    name: input.name.trim(),
    algorithm: input.algorithm,
    hash_on: input.hashOn,
    hash_fallback: input.hashFallback,
    hash_on_header: input.hashOnHeader.trim(),
    hash_fallback_header: input.hashFallbackHeader.trim(),
    hash_on_cookie: input.hashOnCookie.trim(),
    hash_on_cookie_path: input.hashOnCookiePath.trim(),
    hash_on_query_arg: input.hashOnQueryArg.trim(),
    hash_fallback_query_arg: input.hashFallbackQueryArg.trim(),
    hash_on_uri_capture: input.hashOnUriCapture.trim(),
    hash_fallback_uri_capture: input.hashFallbackUriCapture.trim(),
    slots: input.slots,
    healthchecks: input.healthchecks,
    host_header: hostHeader === "" ? null : hostHeader,
    tags: splitList(input.tags),
  };
}

function targetBody(input: {
  upstreamId: string;
  target: string;
  weight: number;
  tags: string;
}): Record<string, unknown> {
  return {
    upstream_id: input.upstreamId,
    target: input.target.trim(),
    weight: input.weight,
    tags: splitList(input.tags),
  };
}

function TypedFormShell({
  pending,
  onCancel,
  onDelete,
  onSubmit,
  errors,
  jsonError,
  children,
}: {
  pending: boolean;
  onCancel: () => void;
  onDelete?: () => void;
  onSubmit: () => void;
  errors: Record<string, string>;
  jsonError: string | null;
  children: ReactNode;
}) {
  const messages = Object.values(errors);
  if (jsonError) {
    messages.push(jsonError);
  }

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    onSubmit();
  }

  return (
    <form onSubmit={handleSubmit} className="flex flex-col gap-3">
      {children}
      {messages.length > 0 ? (
        <div className="rounded-lg border border-danger/30 bg-danger/10 px-3 py-2 text-[10px] leading-relaxed text-danger">
          {messages.join(" ")}
        </div>
      ) : null}
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" type="submit" disabled={pending}>
          {pending ? "Saving…" : "Save"}
        </Button>
        <Button size="sm" variant="secondary" disabled={pending} onClick={onCancel}>
          Cancel
        </Button>
        {onDelete ? (
          <Button
            size="sm"
            variant="danger"
            className="ml-auto"
            disabled={pending}
            onClick={onDelete}
          >
            <Trash2 className="h-3.5 w-3.5" />
            Delete
          </Button>
        ) : null}
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

interface AdvancedJsonState {
  open: boolean;
  setOpen: (open: boolean) => void;
  dirty: boolean;
  value: string;
  change: (value: string) => void;
  reset: () => void;
  body: Record<string, unknown> | null;
  error: string | null;
}

function useAdvancedJson(typedBody: Record<string, unknown>): AdvancedJsonState {
  const [open, setOpen] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [text, setText] = useState("");
  const serialized = JSON.stringify(typedBody, null, 2);
  const parsed = dirty ? parseJsonObjectText(text) : { body: typedBody, error: null };
  return {
    open,
    setOpen,
    dirty,
    value: dirty ? text : serialized,
    change: (value) => {
      setText(value);
      setDirty(true);
    },
    reset: () => {
      setDirty(false);
      setText("");
    },
    body: parsed.body,
    error: parsed.error,
  };
}

function AdvancedJsonPanel({ advanced }: { advanced: AdvancedJsonState }) {
  return (
    <details
      className="rounded-lg border border-border bg-surface/50 p-3"
      open={advanced.open}
      onToggle={(event) => advanced.setOpen(event.currentTarget.open)}
    >
      <summary className="cursor-pointer select-none text-[10.5px] text-muted-foreground">
        Advanced JSON
        <span className="ml-2 text-[9.5px] text-muted-foreground">
          {advanced.dirty ? "edited; used as the full request body" : "synced from typed fields"}
        </span>
      </summary>
      <div className="mt-2 flex flex-col gap-2">
        <textarea
          value={advanced.value}
          onChange={(event) => advanced.change(event.target.value)}
          spellCheck={false}
          rows={8}
          aria-invalid={advanced.error !== null}
          className="min-h-[160px] w-full resize-y rounded-lg border border-border bg-surface p-3 font-mono text-[10.5px] leading-relaxed text-secondary outline-none transition-colors placeholder:text-muted-foreground focus:border-primary/40 focus:ring-2 focus:ring-primary/15"
        />
        <div className="flex flex-wrap items-center justify-between gap-2">
          <span className="text-[9.5px] text-muted-foreground">
            Editing this document overrides the typed fields on save.
          </span>
          {advanced.dirty ? (
            <Button size="sm" variant="ghost" onClick={advanced.reset}>
              Reset from form
            </Button>
          ) : null}
        </div>
        {advanced.error ? <span className="text-[10px] text-danger">{advanced.error}</span> : null}
      </div>
    </details>
  );
}

function asRecord(value: unknown): Record<string, unknown> {
  return isRecord(value) ? value : {};
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function readString(source: Record<string, unknown>, key: string, fallback = ""): string {
  const value = source[key];
  return typeof value === "string" ? value : fallback;
}

function readBoolean(source: Record<string, unknown>, key: string, fallback: boolean): boolean {
  const value = source[key];
  return typeof value === "boolean" ? value : fallback;
}

function readNumberText(source: Record<string, unknown>, key: string, fallback: number): string {
  const value = source[key];
  return typeof value === "number" && Number.isFinite(value) ? String(value) : String(fallback);
}

function readList(source: Record<string, unknown>, key: string, fallback: string[] = []): string {
  const value = source[key];
  if (Array.isArray(value)) {
    return value.filter((item): item is string => typeof item === "string").join(", ");
  }
  return fallback.join(", ");
}

function readRelationId(source: Record<string, unknown>, key: string): string {
  const relation = source[key];
  return isRecord(relation) ? readString(relation, "id") : "";
}

function readOption<T extends string>(value: unknown, options: readonly T[], fallback: T): T {
  return typeof value === "string" && (options as readonly string[]).includes(value)
    ? (value as T)
    : fallback;
}

function readAlgorithm(value: unknown): string {
  return ALGORITHMS.some((item) => item.value === value) ? (value as string) : "round-robin";
}

function readHashOn(value: unknown, fallback: (typeof HASH_ON_VALUES)[number]): string {
  return readOption(value, HASH_ON_VALUES, fallback);
}

function numericDraft(text: string): number {
  const value = Number(text.trim());
  return Number.isFinite(value) ? value : 0;
}

function splitList(value: string): string[] {
  return value
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

function parseInteger(
  text: string,
  field: string,
  label: string,
  errors: Record<string, string>,
  options: { min?: number; max?: number } = {},
): number | null {
  const trimmed = text.trim();
  if (trimmed === "") {
    errors[field] = `${label} is required.`;
    return null;
  }
  const value = Number(trimmed);
  if (!Number.isSafeInteger(value)) {
    errors[field] = `${label} must be an integer.`;
    return null;
  }
  if (options.min !== undefined && value < options.min) {
    errors[field] =
      options.max !== undefined
        ? `${label} must be between ${options.min} and ${options.max}.`
        : `${label} must be at least ${options.min}.`;
    return null;
  }
  if (options.max !== undefined && value > options.max) {
    errors[field] = `${label} must be between ${options.min ?? 0} and ${options.max}.`;
    return null;
  }
  return value;
}

function parseJsonObjectText(text: string): {
  body: Record<string, unknown> | null;
  error: string | null;
} {
  const trimmed = text.trim();
  if (trimmed === "") {
    return { body: null, error: "Advanced JSON body is required." };
  }
  let value: unknown;
  try {
    value = JSON.parse(trimmed);
  } catch (error) {
    return {
      body: null,
      error: `Invalid JSON: ${error instanceof Error ? error.message : "unable to parse"}`,
    };
  }
  if (!isRecord(value)) {
    return { body: null, error: "Advanced JSON body must be a JSON object." };
  }
  return { body: value, error: null };
}
