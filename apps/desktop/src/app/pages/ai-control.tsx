import { ControlApiError } from "@octra/api-client";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type {
  AiCompileReport,
  AiModelInput,
  AiModelTargetInput,
  AiPolicyAttachmentInput,
  AiPolicyInput,
  AiProviderAccountInput,
  AiProviderInput,
} from "@octra/types";
import {
  Badge,
  Button,
  Card,
  CardDescription,
  CardFooter,
  CardHeader,
  CardSeparator,
  CardTitle,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  cn,
} from "@octra/ui";
import {
  BrainCircuit,
  CheckCircle2,
  Pencil,
  Plus,
  RefreshCw,
  RotateCcw,
  ShieldCheck,
  Trash2,
} from "lucide-react";
import { useState } from "react";

import { PageHeader } from "../../components/page-header";
import { StatStrip, type StatItem } from "../../components/stat-strip";
import { controlClient } from "../../lib/control-api";
import { formatNumber } from "../../lib/format";
import { toneDot, toneText } from "../../lib/tone";
import {
  AccountForm,
  AttachmentForm,
  ModelForm,
  PolicyForm,
  ProviderForm,
  TargetForm,
} from "./ai-control-forms";

interface Notice {
  text: string;
  error: boolean;
}

type AiEntityKind = "provider" | "account" | "model" | "target" | "policy" | "attachment";

interface EditorState {
  kind: AiEntityKind;
  entityId: string | null;
  parentId: string | null;
  title: string;
  initial: unknown;
}

interface EntityRef {
  id: string;
}

export function AiControlPage() {
  const queryClient = useQueryClient();
  const [report, setReport] = useState<AiCompileReport | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [editor, setEditor] = useState<EditorState | null>(null);
  const [accountProviderId, setAccountProviderId] = useState("");
  const [targetModelId, setTargetModelId] = useState("");
  const [attachmentPolicyId, setAttachmentPolicyId] = useState("");

  const status = useQuery({
    queryKey: ["ai-status"],
    queryFn: () => controlClient().aiStatus(),
    refetchInterval: 10_000,
    retry: 1,
  });
  const providers = useQuery({
    queryKey: ["ai-providers"],
    queryFn: () => controlClient().aiProviders(),
    retry: 1,
  });
  const models = useQuery({
    queryKey: ["ai-models"],
    queryFn: () => controlClient().aiModels(),
    retry: 1,
  });
  const policies = useQuery({
    queryKey: ["ai-policies"],
    queryFn: () => controlClient().aiPolicies(),
    retry: 1,
  });
  const revisions = useQuery({
    queryKey: ["ai-revisions"],
    queryFn: () => controlClient().aiRevisions(),
    retry: 1,
  });

  const selectedProviderId = resolveSelection(accountProviderId, providers.data);
  const selectedModelId = resolveSelection(targetModelId, models.data);
  const selectedPolicyId = resolveSelection(attachmentPolicyId, policies.data);

  const accounts = useQuery({
    queryKey: ["ai-accounts", selectedProviderId],
    queryFn: () => controlClient().aiListAccounts(selectedProviderId),
    enabled: selectedProviderId !== "",
    retry: 1,
  });
  const selectedTargets = useQuery({
    queryKey: ["ai-targets", selectedModelId],
    queryFn: () => controlClient().aiTargets(selectedModelId),
    enabled: selectedModelId !== "",
    retry: 1,
  });
  const selectedAttachments = useQuery({
    queryKey: ["ai-attachments", selectedPolicyId],
    queryFn: () => controlClient().aiAttachments(selectedPolicyId),
    enabled: selectedPolicyId !== "",
    retry: 1,
  });

  const modelIds = (models.data ?? []).map((model) => model.id);
  const targetCounts = useQuery({
    queryKey: ["ai-target-counts", modelIds],
    queryFn: async () => {
      const lists = await Promise.all(modelIds.map((id) => controlClient().aiTargets(id)));
      return Object.fromEntries(
        modelIds.map((id, index) => [id, lists[index]?.length ?? 0] as const),
      ) as Record<string, number>;
    },
    enabled: modelIds.length > 0,
    retry: 1,
  });

  const policyIds = (policies.data ?? []).map((policy) => policy.id);
  const attachmentCounts = useQuery({
    queryKey: ["ai-attachment-counts", policyIds],
    queryFn: async () => {
      const lists = await Promise.all(policyIds.map((id) => controlClient().aiAttachments(id)));
      return Object.fromEntries(
        policyIds.map((id, index) => [id, lists[index]?.length ?? 0] as const),
      ) as Record<string, number>;
    },
    enabled: policyIds.length > 0,
    retry: 1,
  });

  function invalidate(keys: string[]) {
    for (const key of keys) {
      void queryClient.invalidateQueries({ queryKey: [key] });
    }
  }

  function invalidateAll() {
    invalidate([
      "ai-status",
      "ai-providers",
      "ai-accounts",
      "ai-models",
      "ai-targets",
      "ai-target-counts",
      "ai-policies",
      "ai-attachments",
      "ai-attachment-counts",
      "ai-revisions",
    ]);
  }

  function invalidateEntity(kind: AiEntityKind) {
    const keys = ["ai-status"];
    switch (kind) {
      case "provider":
        keys.push("ai-providers", "ai-accounts");
        break;
      case "account":
        keys.push("ai-accounts");
        break;
      case "model":
        keys.push("ai-models", "ai-targets", "ai-target-counts");
        break;
      case "target":
        keys.push("ai-targets", "ai-target-counts");
        break;
      case "policy":
        keys.push("ai-policies", "ai-attachments", "ai-attachment-counts");
        break;
      case "attachment":
        keys.push("ai-attachments", "ai-attachment-counts");
        break;
    }
    invalidate(keys);
  }

  function onMutationError(error: unknown) {
    const compileReport = compileReportFromError(error);
    if (compileReport) {
      setReport(compileReport);
    }
    setNotice({ text: errorMessage(error), error: true });
  }

  const validate = useMutation({
    mutationFn: () => controlClient().aiValidate(),
    onSuccess: (result) => {
      setReport(result);
      setNotice({
        text: result.ok
          ? "Control plane compiled successfully."
          : `Compile failed with ${result.errors.length} error(s).`,
        error: !result.ok,
      });
      invalidate(["ai-status"]);
    },
    onError: onMutationError,
  });

  const apply = useMutation({
    mutationFn: () => controlClient().aiApply(),
    onSuccess: (result) => {
      setReport(result.report);
      setNotice({
        text: `Revision ${result.revision_id} recorded; the runtime reloaded from AI-native entities.`,
        error: false,
      });
      invalidate(["ai-status", "ai-revisions"]);
    },
    onError: onMutationError,
  });

  const rollback = useMutation({
    mutationFn: (id: string) => controlClient().aiRollback(id),
    onSuccess: (result) => {
      setReport(result.report);
      setNotice({
        text: `Restored revision ${result.revision_id} and reloaded the runtime.`,
        error: false,
      });
      invalidateAll();
    },
    onError: onMutationError,
  });

  const save = useMutation({
    mutationFn: ({ editor: target, body }: { editor: EditorState; body: unknown }) =>
      saveEntity(target, body),
    onSuccess: (_result, variables) => {
      setEditor(null);
      setNotice({
        text: `${entityLabel(variables.editor.kind)} saved.`,
        error: false,
      });
      invalidateEntity(variables.editor.kind);
    },
    onError: onMutationError,
  });

  const remove = useMutation({
    mutationFn: (target: { kind: AiEntityKind; entityId: string }) => deleteEntity(target),
    onSuccess: (_result, target) => {
      setEditor((current) =>
        current?.kind === target.kind && current.entityId === target.entityId ? null : current,
      );
      setNotice({ text: `${entityLabel(target.kind)} deleted.`, error: false });
      invalidateEntity(target.kind);
    },
    onError: onMutationError,
  });

  const busy =
    validate.isPending ||
    apply.isPending ||
    rollback.isPending ||
    save.isPending ||
    remove.isPending;

  function openCreate(kind: AiEntityKind) {
    setEditor({
      kind,
      entityId: null,
      parentId: selectedParent(kind, selectedProviderId, selectedModelId, selectedPolicyId),
      title: `New ${entityLabel(kind).toLowerCase()}`,
      initial: defaultBody(kind),
    });
  }

  function openEdit(kind: AiEntityKind, entity: EntityRef) {
    setEditor({
      kind,
      entityId: entity.id,
      parentId: selectedParent(kind, selectedProviderId, selectedModelId, selectedPolicyId),
      title: `Edit ${entityLabel(kind).toLowerCase()} ${entity.id}`,
      initial: entity,
    });
  }

  function deleteWithConfirm(kind: AiEntityKind, id: string, label: string) {
    const confirmed = window.confirm(
      `Delete ${entityLabel(kind).toLowerCase()} "${label}"? This cannot be undone.`,
    );
    if (confirmed) {
      remove.mutate({ kind, entityId: id });
    }
  }

  function editorFor(kind: AiEntityKind) {
    if (!editor || editor.kind !== kind) {
      return null;
    }
    const key = `${editor.kind}-${editor.entityId ?? "new"}-${editor.parentId ?? ""}`;
    const shared = {
      title: editor.title,
      pending: save.isPending,
      onCancel: () => setEditor(null),
      onSave: (body: unknown) => save.mutate({ editor, body }),
    };
    switch (kind) {
      case "provider":
        return <ProviderForm key={key} {...shared} initial={editor.initial} />;
      case "account":
        return (
          <AccountForm
            key={key}
            {...shared}
            initial={editor.initial}
            providers={providers.data ?? []}
            onOAuthChanged={() => invalidate(["ai-accounts", "ai-status"])}
          />
        );
      case "model":
        return <ModelForm key={key} {...shared} initial={editor.initial} />;
      case "target":
        return (
          <TargetForm
            key={key}
            {...shared}
            initial={editor.initial}
            providers={providers.data ?? []}
            models={(models.data ?? []).filter((model) => model.id !== selectedModelId)}
            accounts={accounts.data ?? []}
          />
        );
      case "policy":
        return <PolicyForm key={key} {...shared} initial={editor.initial} />;
      case "attachment":
        return <AttachmentForm key={key} {...shared} initial={editor.initial} />;
    }
  }

  const counts = status.data?.counts;
  const lastCompile = status.data?.last_compile ?? null;
  const revisionCount = status.data?.revisions ?? revisions.data?.length ?? 0;
  const snapshotCount = revisions.data?.filter((revision) => revision.has_snapshot).length ?? 0;
  const enabledProviders = providers.data?.filter((provider) => provider.enabled).length ?? 0;

  const stats: StatItem[] = [
    {
      label: "Entities",
      value: `${counts?.providers ?? 0} / ${counts?.models ?? 0} / ${counts?.targets ?? 0}`,
      meta: "providers · models · targets",
      tone: "sky",
    },
    {
      label: "Last compile",
      value: lastCompile
        ? lastCompile.ok
          ? "ok"
          : `${lastCompile.errors.length} errors`
        : "not run",
      meta: `${counts?.policies ?? 0} policies · ${counts?.attachments ?? 0} attachments`,
      tone: lastCompile ? (lastCompile.ok ? "primary" : "danger") : "muted",
    },
    {
      label: "Revisions",
      value: formatNumber(revisionCount),
      meta: `${snapshotCount} with rollback snapshots`,
      tone: "violet",
    },
  ];

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Control / AI"
        title="AI Control"
        description="Operate the AI-native LLM control plane. Providers, models, targets, and policies compile into the routing configuration the gateway serves."
        actions={
          <>
            <Button
              variant="secondary"
              onClick={() => invalidateAll()}
              disabled={busy}
              title="Refetch every AI control-plane view"
            >
              <RefreshCw className="h-3.5 w-3.5" />
              Refresh
            </Button>
            <Button variant="secondary" onClick={() => validate.mutate()} disabled={busy}>
              <ShieldCheck className="h-3.5 w-3.5" />
              Validate
            </Button>
            <Button onClick={() => apply.mutate()} disabled={busy}>
              <CheckCircle2 className="h-3.5 w-3.5" />
              Apply
            </Button>
          </>
        }
      />

      <StatStrip items={stats} />

      {status.isPending ? (
        <Card className="px-5 py-3 text-[11px] text-muted-foreground">
          Loading the AI control plane…
        </Card>
      ) : null}
      {status.isError ? (
        <Card className="px-5 py-3 text-[11px] text-danger">{errorMessage(status.error)}</Card>
      ) : null}

      {report ? <CompileReportCard report={report} /> : null}

      {notice ? (
        <Card
          className={cn("px-5 py-3 text-[11px]", notice.error ? "text-danger" : "text-secondary")}
        >
          {notice.text}
        </Card>
      ) : null}

      <Card className="flex flex-col">
        <CardHeader>
          <div className="flex flex-col gap-1">
            <div className="flex items-center gap-2">
              <BrainCircuit className="h-4 w-4 text-muted-foreground" strokeWidth={1.5} />
              <CardTitle>Providers</CardTitle>
            </div>
            <CardDescription>Upstream endpoints stored in the ai_providers table</CardDescription>
          </div>
          <div className="flex items-center gap-2">
            <Badge variant="neutral">{providers.data?.length ?? 0}</Badge>
            <Button size="sm" variant="secondary" onClick={() => openCreate("provider")}>
              <Plus className="h-3.5 w-3.5" />
              New provider
            </Button>
          </div>
        </CardHeader>
        {editorFor("provider")}
        <CardSeparator />
        <Table>
          <TableHeader>
            <TableRow className="hover:bg-transparent">
              <TableHead>Name</TableHead>
              <TableHead>Protocol</TableHead>
              <TableHead>Base URL</TableHead>
              <TableHead>Enabled</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(providers.data ?? []).map((provider) => (
              <TableRow key={provider.id}>
                <TableCell>
                  <div className="flex flex-col">
                    <span className="text-[12px]">{provider.name}</span>
                    <span className="font-mono text-[10px] text-muted-foreground">
                      {provider.id}
                    </span>
                  </div>
                </TableCell>
                <TableCell>
                  <span className="font-mono text-[10.5px] text-secondary">
                    {provider.protocol}
                  </span>
                </TableCell>
                <TableCell>
                  <span className="font-mono text-[10.5px] text-secondary">
                    {provider.base_url}
                  </span>
                </TableCell>
                <TableCell>
                  <EnabledState enabled={provider.enabled} />
                </TableCell>
                <TableCell className="text-right">
                  <div className="flex justify-end gap-1.5">
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={busy}
                      onClick={() => openEdit("provider", provider)}
                    >
                      <Pencil className="h-3.5 w-3.5" />
                      Edit
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="text-danger hover:text-danger"
                      disabled={busy}
                      onClick={() => deleteWithConfirm("provider", provider.id, provider.name)}
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                      Delete
                    </Button>
                  </div>
                </TableCell>
              </TableRow>
            ))}
            <TableStateRow
              colSpan={5}
              pending={providers.isPending}
              error={providers.isError ? errorMessage(providers.error) : null}
              empty={(providers.data?.length ?? 0) === 0 && !providers.isPending}
              emptyLabel="No AI providers configured."
              pendingLabel="Loading providers…"
            />
          </TableBody>
        </Table>
        <div className="flex-1" />
        <CardFooter>
          <span className="text-[11px] text-muted-foreground">
            {providers.data?.length ?? 0} providers · {enabledProviders} enabled
          </span>
          <span className="font-mono text-[10.5px] text-muted-foreground">
            {counts?.accounts ?? 0} accounts
          </span>
        </CardFooter>
      </Card>

      <Card className="flex flex-col">
        <CardHeader>
          <div className="flex flex-col gap-1">
            <CardTitle>Accounts</CardTitle>
            <CardDescription>Credentials bound to the selected provider</CardDescription>
          </div>
          <div className="flex flex-wrap items-center justify-end gap-2">
            <select
              value={selectedProviderId}
              onChange={(event) => {
                setAccountProviderId(event.target.value);
                if (editor?.kind === "account") {
                  setEditor(null);
                }
              }}
              className="h-8 rounded-md border border-border bg-surface px-2 font-mono text-[10.5px] text-foreground"
              disabled={!providers.data || providers.data.length === 0}
            >
              {(providers.data ?? []).map((provider) => (
                <option key={provider.id} value={provider.id}>
                  {provider.name}
                </option>
              ))}
            </select>
            <Badge variant="neutral">{accounts.data?.length ?? 0}</Badge>
            <Button
              size="sm"
              variant="secondary"
              disabled={busy || selectedProviderId === ""}
              onClick={() => openCreate("account")}
            >
              <Plus className="h-3.5 w-3.5" />
              New account
            </Button>
          </div>
        </CardHeader>
        {editorFor("account")}
        <CardSeparator />
        <Table>
          <TableHeader>
            <TableRow className="hover:bg-transparent">
              <TableHead>Name</TableHead>
              <TableHead>Credential</TableHead>
              <TableHead>Quota</TableHead>
              <TableHead>Enabled</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(accounts.data ?? []).map((account) => (
              <TableRow key={account.id}>
                <TableCell>
                  <div className="flex flex-col">
                    <span className="text-[12px]">{account.name}</span>
                    <span className="font-mono text-[10px] text-muted-foreground">
                      {account.id}
                    </span>
                  </div>
                </TableCell>
                <TableCell>
                  <span className="font-mono text-[10.5px] text-secondary">
                    {account.credential_kind}
                  </span>
                </TableCell>
                <TableCell>
                  <span className="text-[10.5px] text-secondary">{quotaLabel(account)}</span>
                </TableCell>
                <TableCell>
                  <EnabledState enabled={account.enabled} />
                </TableCell>
                <TableCell className="text-right">
                  <div className="flex justify-end gap-1.5">
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={busy}
                      onClick={() => openEdit("account", account)}
                    >
                      <Pencil className="h-3.5 w-3.5" />
                      Edit
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="text-danger hover:text-danger"
                      disabled={busy}
                      onClick={() => deleteWithConfirm("account", account.id, account.name)}
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                      Delete
                    </Button>
                  </div>
                </TableCell>
              </TableRow>
            ))}
            <TableStateRow
              colSpan={5}
              pending={selectedProviderId !== "" && accounts.isPending}
              error={
                selectedProviderId !== "" && accounts.isError ? errorMessage(accounts.error) : null
              }
              empty={
                selectedProviderId === "" ||
                ((accounts.data?.length ?? 0) === 0 && !accounts.isPending)
              }
              emptyLabel={
                selectedProviderId === ""
                  ? "Select a provider to list accounts."
                  : "No accounts configured for this provider."
              }
              pendingLabel="Loading accounts…"
            />
          </TableBody>
        </Table>
        <div className="flex-1" />
        <CardFooter>
          <span className="text-[11px] text-muted-foreground">
            {accounts.data?.length ?? 0} accounts for the selected provider
          </span>
          <span className="font-mono text-[10.5px] text-muted-foreground">
            {counts?.accounts ?? 0} total
          </span>
        </CardFooter>
      </Card>

      <div className="grid gap-4 xl:grid-cols-2">
        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Models</CardTitle>
              <CardDescription>Concrete models and routing groups from ai_models</CardDescription>
            </div>
            <div className="flex items-center gap-2">
              <Badge variant="neutral">{models.data?.length ?? 0}</Badge>
              <Button size="sm" variant="secondary" onClick={() => openCreate("model")}>
                <Plus className="h-3.5 w-3.5" />
                New model
              </Button>
            </div>
          </CardHeader>
          {editorFor("model")}
          <CardSeparator />
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead>Name</TableHead>
                <TableHead>Kind</TableHead>
                <TableHead>Capabilities</TableHead>
                <TableHead>Formats</TableHead>
                <TableHead className="text-right">Targets</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(models.data ?? []).map((model) => (
                <TableRow key={model.id}>
                  <TableCell>
                    <div className="flex flex-col">
                      <span className="text-[12px]">{model.name}</span>
                      <span className="font-mono text-[10px] text-muted-foreground">
                        {model.id}
                      </span>
                    </div>
                  </TableCell>
                  <TableCell>
                    <Badge variant={model.kind === "group" ? "violet" : "neutral"}>
                      {model.kind}
                    </Badge>
                  </TableCell>
                  <TableCell>
                    <span className="text-[10.5px] text-secondary">
                      {model.capabilities.length > 0 ? model.capabilities.join(", ") : "—"}
                    </span>
                  </TableCell>
                  <TableCell>
                    <span className="text-[10.5px] text-secondary">
                      {model.formats.length > 0 ? model.formats.join(", ") : "—"}
                    </span>
                  </TableCell>
                  <TableCell className="text-right font-mono text-[11px]">
                    {targetCounts.data?.[model.id] === undefined
                      ? "—"
                      : formatNumber(targetCounts.data[model.id] ?? 0)}
                  </TableCell>
                  <TableCell className="text-right">
                    <div className="flex justify-end gap-1.5">
                      <Button
                        size="sm"
                        variant="secondary"
                        disabled={busy}
                        onClick={() => openEdit("model", model)}
                      >
                        <Pencil className="h-3.5 w-3.5" />
                        Edit
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="text-danger hover:text-danger"
                        disabled={busy}
                        onClick={() => deleteWithConfirm("model", model.id, model.name)}
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                        Delete
                      </Button>
                    </div>
                  </TableCell>
                </TableRow>
              ))}
              <TableStateRow
                colSpan={6}
                pending={models.isPending}
                error={models.isError ? errorMessage(models.error) : null}
                empty={(models.data?.length ?? 0) === 0 && !models.isPending}
                emptyLabel="No AI models configured."
                pendingLabel="Loading models…"
              />
            </TableBody>
          </Table>
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">
              {models.data?.length ?? 0} models · {counts?.targets ?? 0} targets
            </span>
            <span className="font-mono text-[10.5px] text-muted-foreground">
              {models.data?.filter((model) => model.enabled).length ?? 0} enabled
            </span>
          </CardFooter>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Policies</CardTitle>
              <CardDescription>Reusable policy definitions and attachment counts</CardDescription>
            </div>
            <div className="flex items-center gap-2">
              <Badge variant="neutral">{policies.data?.length ?? 0}</Badge>
              <Button size="sm" variant="secondary" onClick={() => openCreate("policy")}>
                <Plus className="h-3.5 w-3.5" />
                New policy
              </Button>
            </div>
          </CardHeader>
          {editorFor("policy")}
          <CardSeparator />
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead>Name</TableHead>
                <TableHead>Type</TableHead>
                <TableHead>Enabled</TableHead>
                <TableHead className="text-right">Attachments</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(policies.data ?? []).map((policy) => (
                <TableRow key={policy.id}>
                  <TableCell>
                    <div className="flex flex-col">
                      <span className="text-[12px]">{policy.name}</span>
                      <span className="font-mono text-[10px] text-muted-foreground">
                        {policy.id}
                      </span>
                    </div>
                  </TableCell>
                  <TableCell>
                    <span className="font-mono text-[10.5px] text-secondary">
                      {policy.policy_type}
                    </span>
                  </TableCell>
                  <TableCell>
                    <EnabledState enabled={policy.enabled} />
                  </TableCell>
                  <TableCell className="text-right font-mono text-[11px]">
                    {attachmentCounts.data?.[policy.id] === undefined
                      ? "—"
                      : formatNumber(attachmentCounts.data[policy.id] ?? 0)}
                  </TableCell>
                  <TableCell className="text-right">
                    <div className="flex justify-end gap-1.5">
                      <Button
                        size="sm"
                        variant="secondary"
                        disabled={busy}
                        onClick={() => openEdit("policy", policy)}
                      >
                        <Pencil className="h-3.5 w-3.5" />
                        Edit
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="text-danger hover:text-danger"
                        disabled={busy}
                        onClick={() => deleteWithConfirm("policy", policy.id, policy.name)}
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                        Delete
                      </Button>
                    </div>
                  </TableCell>
                </TableRow>
              ))}
              <TableStateRow
                colSpan={5}
                pending={policies.isPending}
                error={policies.isError ? errorMessage(policies.error) : null}
                empty={(policies.data?.length ?? 0) === 0 && !policies.isPending}
                emptyLabel="No AI policies configured."
                pendingLabel="Loading policies…"
              />
            </TableBody>
          </Table>
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">
              {policies.data?.length ?? 0} policies · {counts?.attachments ?? 0} attachments
            </span>
            <span className="font-mono text-[10.5px] text-muted-foreground">
              {policies.data?.filter((policy) => policy.enabled).length ?? 0} enabled
            </span>
          </CardFooter>
        </Card>
      </div>

      <Card className="flex flex-col">
        <CardHeader>
          <div className="flex flex-col gap-1">
            <CardTitle>Targets</CardTitle>
            <CardDescription>
              Provider and nested-model expansions for the selected model
            </CardDescription>
          </div>
          <div className="flex flex-wrap items-center justify-end gap-2">
            <select
              value={selectedModelId}
              onChange={(event) => {
                setTargetModelId(event.target.value);
                if (editor?.kind === "target") {
                  setEditor(null);
                }
              }}
              className="h-8 rounded-md border border-border bg-surface px-2 font-mono text-[10.5px] text-foreground"
              disabled={!models.data || models.data.length === 0}
            >
              {(models.data ?? []).map((model) => (
                <option key={model.id} value={model.id}>
                  {model.name}
                </option>
              ))}
            </select>
            <Badge variant="neutral">{selectedTargets.data?.length ?? 0}</Badge>
            <Button
              size="sm"
              variant="secondary"
              disabled={busy || selectedModelId === ""}
              onClick={() => openCreate("target")}
            >
              <Plus className="h-3.5 w-3.5" />
              New target
            </Button>
          </div>
        </CardHeader>
        {editorFor("target")}
        <CardSeparator />
        <Table>
          <TableHeader>
            <TableRow className="hover:bg-transparent">
              <TableHead>Target</TableHead>
              <TableHead>Provider</TableHead>
              <TableHead>Upstream model</TableHead>
              <TableHead>Weight / priority</TableHead>
              <TableHead>Enabled</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(selectedTargets.data ?? []).map((target) => (
              <TableRow key={target.id}>
                <TableCell>
                  <div className="flex flex-col">
                    <span className="text-[12px]">{target.target_model_id ?? target.id}</span>
                    <span className="font-mono text-[10px] text-muted-foreground">{target.id}</span>
                  </div>
                </TableCell>
                <TableCell>
                  <div className="flex flex-col">
                    <span className="font-mono text-[10.5px] text-secondary">
                      {target.provider_id ?? "—"}
                    </span>
                    <span className="font-mono text-[10px] text-muted-foreground">
                      {target.account_id ?? "no account"}
                    </span>
                  </div>
                </TableCell>
                <TableCell>
                  <span className="font-mono text-[10.5px] text-secondary">
                    {target.upstream_model ?? "—"}
                  </span>
                </TableCell>
                <TableCell>
                  <span className="font-mono text-[10.5px] text-secondary">
                    {target.weight} / {target.priority}
                  </span>
                </TableCell>
                <TableCell>
                  <EnabledState enabled={target.enabled} />
                </TableCell>
                <TableCell className="text-right">
                  <div className="flex justify-end gap-1.5">
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={busy}
                      onClick={() => openEdit("target", target)}
                    >
                      <Pencil className="h-3.5 w-3.5" />
                      Edit
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="text-danger hover:text-danger"
                      disabled={busy}
                      onClick={() => deleteWithConfirm("target", target.id, target.id)}
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                      Delete
                    </Button>
                  </div>
                </TableCell>
              </TableRow>
            ))}
            <TableStateRow
              colSpan={6}
              pending={selectedModelId !== "" && selectedTargets.isPending}
              error={
                selectedModelId !== "" && selectedTargets.isError
                  ? errorMessage(selectedTargets.error)
                  : null
              }
              empty={
                selectedModelId === "" ||
                ((selectedTargets.data?.length ?? 0) === 0 && !selectedTargets.isPending)
              }
              emptyLabel={
                selectedModelId === ""
                  ? "Select a model to list targets."
                  : "No targets configured for this model."
              }
              pendingLabel="Loading targets…"
            />
          </TableBody>
        </Table>
        <div className="flex-1" />
        <CardFooter>
          <span className="text-[11px] text-muted-foreground">
            {selectedTargets.data?.length ?? 0} targets for the selected model
          </span>
          <span className="font-mono text-[10.5px] text-muted-foreground">
            {counts?.targets ?? 0} total
          </span>
        </CardFooter>
      </Card>

      <Card className="flex flex-col">
        <CardHeader>
          <div className="flex flex-col gap-1">
            <CardTitle>Attachments</CardTitle>
            <CardDescription>Scopes where the selected policy applies</CardDescription>
          </div>
          <div className="flex flex-wrap items-center justify-end gap-2">
            <select
              value={selectedPolicyId}
              onChange={(event) => {
                setAttachmentPolicyId(event.target.value);
                if (editor?.kind === "attachment") {
                  setEditor(null);
                }
              }}
              className="h-8 rounded-md border border-border bg-surface px-2 font-mono text-[10.5px] text-foreground"
              disabled={!policies.data || policies.data.length === 0}
            >
              {(policies.data ?? []).map((policy) => (
                <option key={policy.id} value={policy.id}>
                  {policy.name}
                </option>
              ))}
            </select>
            <Badge variant="neutral">{selectedAttachments.data?.length ?? 0}</Badge>
            <Button
              size="sm"
              variant="secondary"
              disabled={busy || selectedPolicyId === ""}
              onClick={() => openCreate("attachment")}
            >
              <Plus className="h-3.5 w-3.5" />
              New attachment
            </Button>
          </div>
        </CardHeader>
        {editorFor("attachment")}
        <CardSeparator />
        <Table>
          <TableHeader>
            <TableRow className="hover:bg-transparent">
              <TableHead>Scope</TableHead>
              <TableHead>Scope ID</TableHead>
              <TableHead>Order</TableHead>
              <TableHead>Enabled</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(selectedAttachments.data ?? []).map((attachment) => (
              <TableRow key={attachment.id}>
                <TableCell>
                  <div className="flex flex-col">
                    <span className="font-mono text-[10.5px] text-secondary">
                      {attachment.scope}
                    </span>
                    <span className="font-mono text-[10px] text-muted-foreground">
                      {attachment.id}
                    </span>
                  </div>
                </TableCell>
                <TableCell>
                  <span className="font-mono text-[10.5px] text-secondary">
                    {attachment.scope_id ?? "—"}
                  </span>
                </TableCell>
                <TableCell>
                  <span className="font-mono text-[10.5px] text-secondary">{attachment.order}</span>
                </TableCell>
                <TableCell>
                  <EnabledState enabled={attachment.enabled} />
                </TableCell>
                <TableCell className="text-right">
                  <div className="flex justify-end gap-1.5">
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={busy}
                      onClick={() => openEdit("attachment", attachment)}
                    >
                      <Pencil className="h-3.5 w-3.5" />
                      Edit
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="text-danger hover:text-danger"
                      disabled={busy}
                      onClick={() => deleteWithConfirm("attachment", attachment.id, attachment.id)}
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                      Delete
                    </Button>
                  </div>
                </TableCell>
              </TableRow>
            ))}
            <TableStateRow
              colSpan={5}
              pending={selectedPolicyId !== "" && selectedAttachments.isPending}
              error={
                selectedPolicyId !== "" && selectedAttachments.isError
                  ? errorMessage(selectedAttachments.error)
                  : null
              }
              empty={
                selectedPolicyId === "" ||
                ((selectedAttachments.data?.length ?? 0) === 0 && !selectedAttachments.isPending)
              }
              emptyLabel={
                selectedPolicyId === ""
                  ? "Select a policy to list attachments."
                  : "No attachments configured for this policy."
              }
              pendingLabel="Loading attachments…"
            />
          </TableBody>
        </Table>
        <div className="flex-1" />
        <CardFooter>
          <span className="text-[11px] text-muted-foreground">
            {selectedAttachments.data?.length ?? 0} attachments for the selected policy
          </span>
          <span className="font-mono text-[10.5px] text-muted-foreground">
            {counts?.attachments ?? 0} total
          </span>
        </CardFooter>
      </Card>

      <Card className="flex flex-col">
        <CardHeader>
          <div className="flex flex-col gap-1">
            <div className="flex items-center gap-2">
              <RotateCcw className="h-4 w-4 text-muted-foreground" strokeWidth={1.5} />
              <CardTitle>Revisions</CardTitle>
            </div>
            <CardDescription>
              Applied snapshots of the control plane; rollback restores a captured revision
            </CardDescription>
          </div>
          <Badge variant="neutral">{revisions.data?.length ?? 0}</Badge>
        </CardHeader>
        <CardSeparator />
        <Table>
          <TableHeader>
            <TableRow className="hover:bg-transparent">
              <TableHead>Revision</TableHead>
              <TableHead>Created</TableHead>
              <TableHead>Actor</TableHead>
              <TableHead>Status</TableHead>
              <TableHead>Snapshot</TableHead>
              <TableHead className="text-right">Action</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(revisions.data ?? []).map((revision) => (
              <TableRow key={revision.id}>
                <TableCell>
                  <span className="font-mono text-[10.5px]">{revision.id}</span>
                </TableCell>
                <TableCell>
                  <span className="font-mono text-[10.5px] text-secondary">
                    {formatEpochSeconds(revision.created_at)}
                  </span>
                </TableCell>
                <TableCell>
                  <span className="text-[11px] text-secondary">{revision.actor}</span>
                </TableCell>
                <TableCell>
                  <Badge variant={revision.status === "applied" ? "accent" : "neutral"}>
                    {revision.status}
                  </Badge>
                </TableCell>
                <TableCell>
                  {revision.has_snapshot ? (
                    <Badge variant="violet">snapshot</Badge>
                  ) : (
                    <span className="text-[10.5px] text-muted-foreground">none</span>
                  )}
                </TableCell>
                <TableCell className="text-right">
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={busy || !revision.has_snapshot}
                    onClick={() => rollback.mutate(revision.id)}
                  >
                    <RotateCcw className="h-3.5 w-3.5" />
                    Rollback
                  </Button>
                </TableCell>
              </TableRow>
            ))}
            <TableStateRow
              colSpan={6}
              pending={revisions.isPending}
              error={revisions.isError ? errorMessage(revisions.error) : null}
              empty={(revisions.data?.length ?? 0) === 0 && !revisions.isPending}
              emptyLabel="No revisions recorded. Apply the control plane to create one."
              pendingLabel="Loading revisions…"
            />
          </TableBody>
        </Table>
        <div className="flex-1" />
        <CardFooter>
          <span className="text-[11px] text-muted-foreground">
            {revisions.data?.length ?? 0} revisions · {snapshotCount} snapshots
          </span>
          <span className="font-mono text-[10.5px] text-muted-foreground">
            rollback replaces the live control plane
          </span>
        </CardFooter>
      </Card>
    </div>
  );
}

function CompileReportCard({ report }: { report: AiCompileReport }) {
  return (
    <Card className="flex flex-col">
      <CardHeader>
        <div className="flex flex-col gap-1">
          <div className="flex items-center gap-2">
            <ShieldCheck className="h-4 w-4 text-muted-foreground" strokeWidth={1.5} />
            <CardTitle>Compile report</CardTitle>
          </div>
          <CardDescription>
            {report.counts.providers} providers · {report.counts.accounts} accounts ·{" "}
            {report.counts.models} models · {report.counts.targets} targets ·{" "}
            {report.counts.policies} policies · {report.counts.attachments} attachments
          </CardDescription>
        </div>
        <Badge variant={report.ok ? "accent" : "danger"}>
          {report.ok ? "ok" : `${report.errors.length} errors`}
        </Badge>
      </CardHeader>
      {report.errors.length > 0 ? (
        <>
          <CardSeparator />
          <ul className="flex flex-col gap-1.5 p-4">
            {report.errors.map((error, index) => (
              <li
                key={`${error.code}-${index}`}
                className="flex flex-col gap-0.5 border-b border-border pb-1.5 last:border-0 last:pb-0"
              >
                <span className="font-mono text-[10.5px] text-danger">{error.code}</span>
                <span className="text-[11px] text-secondary">{error.message}</span>
              </li>
            ))}
          </ul>
        </>
      ) : null}
    </Card>
  );
}

function EnabledState({ enabled }: { enabled: boolean }) {
  return (
    <span className="flex items-center gap-1.5">
      <span
        className={cn("h-[5px] w-[5px] rounded-full", enabled ? toneDot.primary : toneDot.muted)}
      />
      <span className={cn("text-[11px]", enabled ? toneText.primary : toneText.muted)}>
        {enabled ? "Enabled" : "Disabled"}
      </span>
    </span>
  );
}

function TableStateRow({
  colSpan,
  pending,
  error,
  empty,
  pendingLabel,
  emptyLabel,
}: {
  colSpan: number;
  pending: boolean;
  error: string | null;
  empty: boolean;
  pendingLabel: string;
  emptyLabel: string;
}) {
  if (pending) {
    return (
      <TableRow className="hover:bg-transparent">
        <TableCell colSpan={colSpan} className="py-4 text-[11.5px] text-muted-foreground">
          {pendingLabel}
        </TableCell>
      </TableRow>
    );
  }
  if (error) {
    return (
      <TableRow className="hover:bg-transparent">
        <TableCell colSpan={colSpan} className="py-4 text-[11.5px] text-danger">
          {error}
        </TableCell>
      </TableRow>
    );
  }
  if (empty) {
    return (
      <TableRow className="hover:bg-transparent">
        <TableCell colSpan={colSpan} className="py-4 text-[11.5px] text-muted-foreground">
          {emptyLabel}
        </TableCell>
      </TableRow>
    );
  }
  return null;
}

function resolveSelection<T extends { id: string }>(preferred: string, items: T[] | undefined) {
  if (preferred && (items ?? []).some((item) => item.id === preferred)) {
    return preferred;
  }
  return items?.[0]?.id ?? "";
}

function selectedParent(
  kind: AiEntityKind,
  providerId: string,
  modelId: string,
  policyId: string,
): string | null {
  switch (kind) {
    case "account":
      return providerId || null;
    case "target":
      return modelId || null;
    case "attachment":
      return policyId || null;
    default:
      return null;
  }
}

function entityLabel(kind: AiEntityKind): string {
  switch (kind) {
    case "provider":
      return "Provider";
    case "account":
      return "Account";
    case "model":
      return "Model";
    case "target":
      return "Target";
    case "policy":
      return "Policy";
    case "attachment":
      return "Attachment";
  }
}

function defaultBody(kind: AiEntityKind): unknown {
  switch (kind) {
    case "provider":
      return defaultProviderBody();
    case "account":
      return defaultAccountBody();
    case "model":
      return defaultModelBody();
    case "target":
      return defaultTargetBody();
    case "policy":
      return defaultPolicyBody();
    case "attachment":
      return defaultAttachmentBody();
  }
}

function defaultProviderBody(): AiProviderInput {
  return {
    name: "openai",
    protocol: "openai",
    base_url: "https://api.openai.com",
    auth_ref: null,
    capabilities: ["chat"],
    config: {},
    enabled: true,
  };
}

function defaultAccountBody(): AiProviderAccountInput {
  return {
    name: "default",
    credential_kind: "static",
    secret_ref: null,
    oauth_ref: null,
    weight: 1,
    priority: 0,
    quota_limit: null,
    quota_unit: "requests",
    quota_period: "none",
    quota_used: 0,
    cooldown_until: null,
    config: {},
    enabled: true,
  };
}

function defaultModelBody(): AiModelInput {
  return {
    name: "chat",
    kind: "model",
    capabilities: ["chat"],
    formats: ["openai"],
    route_paths: ["/v1/chat/completions"],
    balancer: { strategy: "priority", retries: 0, failover_criteria: [] },
    enabled: true,
  };
}

function defaultTargetBody(): AiModelTargetInput {
  return {
    target_model_id: null,
    provider_id: null,
    upstream_model: null,
    account_id: null,
    weight: 1,
    priority: 0,
    position: 0,
    config: {},
    semantic_description: null,
    enabled: true,
  };
}

function defaultPolicyBody(): AiPolicyInput {
  return {
    name: "rate-limit",
    policy_type: "rate_limit",
    config: { minute: 60, limit_by: "virtual_key" },
    enabled: true,
  };
}

function defaultAttachmentBody(): AiPolicyAttachmentInput {
  return {
    scope: "global",
    scope_id: null,
    order: 0,
    enabled: true,
  };
}

async function saveEntity(editor: EditorState, body: unknown): Promise<unknown> {
  const client = controlClient();
  switch (editor.kind) {
    case "provider":
      return editor.entityId
        ? client.aiUpdateProvider(editor.entityId, body)
        : client.aiCreateProvider(body);
    case "account": {
      const providerId = requireParent(editor);
      return editor.entityId
        ? client.aiUpdateAccount(editor.entityId, body)
        : client.aiCreateAccount(providerId, body);
    }
    case "model":
      return editor.entityId
        ? client.aiUpdateModel(editor.entityId, body)
        : client.aiCreateModel(body);
    case "target": {
      const modelId = requireParent(editor);
      return editor.entityId
        ? client.aiUpdateTarget(editor.entityId, body)
        : client.aiCreateTarget(modelId, body);
    }
    case "policy":
      return editor.entityId
        ? client.aiUpdatePolicy(editor.entityId, body)
        : client.aiCreatePolicy(body);
    case "attachment": {
      const policyId = requireParent(editor);
      return editor.entityId
        ? client.aiUpdateAttachment(editor.entityId, body)
        : client.aiCreateAttachment(policyId, body);
    }
  }
}

async function deleteEntity(target: { kind: AiEntityKind; entityId: string }): Promise<void> {
  const client = controlClient();
  switch (target.kind) {
    case "provider":
      return client.aiDeleteProvider(target.entityId);
    case "account":
      return client.aiDeleteAccount(target.entityId);
    case "model":
      return client.aiDeleteModel(target.entityId);
    case "target":
      return client.aiDeleteTarget(target.entityId);
    case "policy":
      return client.aiDeletePolicy(target.entityId);
    case "attachment":
      return client.aiDeleteAttachment(target.entityId);
  }
}

function requireParent(editor: EditorState): string {
  if (!editor.parentId) {
    throw new Error(`Select a ${parentLabel(editor.kind)} before saving.`);
  }
  return editor.parentId;
}

function parentLabel(kind: AiEntityKind): string {
  switch (kind) {
    case "account":
      return "provider";
    case "target":
      return "model";
    case "attachment":
      return "policy";
    default:
      return "parent entity";
  }
}

function quotaLabel(account: {
  quota_limit: number | null;
  quota_used: number;
  quota_unit: string;
  quota_period: string;
}): string {
  if (account.quota_limit === null) {
    return "unlimited";
  }
  return `${formatNumber(account.quota_used)} / ${formatNumber(account.quota_limit)} ${account.quota_unit} · ${account.quota_period}`;
}

function formatEpochSeconds(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) {
    return "—";
  }
  return new Date(seconds * 1000).toLocaleString();
}

function compileReportFromError(error: unknown): AiCompileReport | null {
  if (!(error instanceof ControlApiError)) {
    return null;
  }
  const detail = error.detail;
  if (!detail || typeof detail !== "object" || !("report" in detail)) {
    return null;
  }
  const report = (detail as { report?: AiCompileReport }).report;
  return report ?? null;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "request failed";
}
