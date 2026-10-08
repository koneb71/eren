import { describe, expect, it } from "vitest";
import { safeHttpUrl } from "./url";

describe("safeHttpUrl", () => {
  it("passes http and https through as they are", () => {
    expect(safeHttpUrl("https://example.com/a?b=1#c")).toBe("https://example.com/a?b=1#c");
    expect(safeHttpUrl("http://localhost:4820/")).toBe("http://localhost:4820/");
    expect(safeHttpUrl("HTTPS://EXAMPLE.COM")).toBe("https://example.com/");
  });

  it("refuses every scheme that is not http(s), however it is spelled", () => {
    for (const bad of [
      "javascript:alert(1)",
      "JavaScript:alert(1)",
      " javascript:alert(1)",
      "data:text/html;base64,PHNjcmlwdD4=",
      "vbscript:msgbox",
      "file:///etc/passwd",
      "mailto:a@b.c",
      "ftp://x/y",
    ]) {
      expect(safeHttpUrl(bad), bad).toBeUndefined();
    }
  });

  it("refuses what is not a URL at all", () => {
    for (const bad of ["", "   ", "/relative", "example.com", "not a url", null, undefined, 42, {}]) {
      expect(safeHttpUrl(bad), String(bad)).toBeUndefined();
    }
  });
});
