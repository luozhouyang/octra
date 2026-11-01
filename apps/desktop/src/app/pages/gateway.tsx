import { PlaceholderPage } from "../../components/placeholder-page";

export function GatewayPage() {
  return (
    <PlaceholderPage
      title="API Gateway"
      phase="Phase 2"
      description="Unified HTTP proxy for local APIs. Configuration is stored in SQLite and loaded into the runtime snapshot; the data plane lands next."
      capabilities={[
        "Service, route, and upstream registry",
        "Host, path, method, and header matching",
        "Upstream load balancing with health checks",
        "Authentication, authorization, and rate limiting",
        "Graceful configuration reload without dropping requests",
      ]}
    />
  );
}
