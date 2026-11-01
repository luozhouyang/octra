import { describe, expect, it } from "vitest";

import { ControlApiClient, ControlApiError } from "./index";

function fetchStub(response: Response): typeof fetch {
  return (async () => response) as typeof fetch;
}

describe("ControlApiClient", () => {
  it("sends the bearer token and returns the parsed body", async () => {
    let authorization: string | null = null;
    const fetchImpl = (async (_input: RequestInfo | URL, init?: RequestInit) => {
      authorization = new Headers(init?.headers).get("authorization");
      return new Response(JSON.stringify({ status: "ok" }), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    }) as typeof fetch;

    const client = new ControlApiClient({
      baseUrl: "http://127.0.0.1:8081/",
      token: "secret",
      fetchImpl,
    });

    const health = await client.health();

    expect(authorization).toBe("Bearer secret");
    expect(health).toEqual({ status: "ok" });
  });

  it("uses the global fetch when no implementation is given", async () => {
    const original = globalThis.fetch;
    let called = false;
    globalThis.fetch = (async () => {
      called = true;
      return new Response("{}", { status: 200 });
    }) as typeof fetch;

    try {
      const client = new ControlApiClient({ baseUrl: "http://127.0.0.1:8081" });
      await client.health();
      expect(called).toBe(true);
    } finally {
      globalThis.fetch = original;
    }
  });

  it("throws a typed error for failed responses", async () => {
    const client = new ControlApiClient({
      baseUrl: "http://127.0.0.1:8081",
      fetchImpl: fetchStub(new Response(JSON.stringify({ error: "unauthorized" }), { status: 401 })),
    });

    await expect(client.health()).rejects.toBeInstanceOf(ControlApiError);
  });
});
