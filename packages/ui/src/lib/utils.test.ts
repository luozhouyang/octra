import { describe, expect, it } from "vitest";

import { cn } from "./utils";

describe("cn", () => {
  it("merges conflicting tailwind classes", () => {
    expect(cn("px-2 py-1", "px-4")).toBe("py-1 px-4");
  });

  it("ignores falsy values", () => {
    expect(cn("text-sm", false && "hidden", undefined)).toBe("text-sm");
  });
});
