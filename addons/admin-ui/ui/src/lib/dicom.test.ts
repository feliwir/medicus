import { describe, expect, it } from "vitest";
import { formatName, formatTag, keyword, number, text } from "./dicom";

describe("DICOM JSON", () => {
  const obj = {
    "00100010": { vr: "PN", Value: [{ Alphabetic: "Doe^John" }] },
    "00080061": { vr: "CS", Value: ["CT", "SR"] },
    "00201208": { vr: "IS", Value: [12] },
    "00081115": { vr: "SQ", Value: [{}, {}] },
    "7FE00010": { vr: "OW", BulkDataURI: "x" },
  };

  it("reads values", () => {
    expect(text(obj, "00100010")).toBe("Doe^John");
    expect(text(obj, "00080061")).toBe("CT\\SR");
    expect(text(obj, "00081115")).toBe("2 items");
    expect(text(obj, "7FE00010")).toBe("(binary)");
    expect(text(obj, "00100020")).toBe("");
    expect(number(obj, "00201208")).toBe(12);
  });

  it("formats", () => {
    expect(formatName("Doe^John^Q")).toBe("Doe, John Q");
    expect(formatTag("0020000D")).toBe("(0020,000D)");
    expect(keyword("0020000d")).toBe("StudyInstanceUID");
  });
});
