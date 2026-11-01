import { PlaceholderPage } from "../../components/placeholder-page";

export function LlmPage() {
  return (
    <PlaceholderPage
      title="LLM Gateway"
      phase="Phase 5–6"
      description="One endpoint for every model provider. Requests normalize into a canonical IR, route to a provider, and convert back into the client's protocol."
      capabilities={[
        "Canonical IR for messages, tools, reasoning, and multimodal content",
        "OpenAI Chat Completions, OpenAI Responses, Anthropic, and Gemini adapters",
        "Streaming event, usage, and error conversion",
        "Virtual model IDs, routing groups, retry, and fallback",
        "Virtual API keys with provider credentials injected upstream",
      ]}
    />
  );
}
