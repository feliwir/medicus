// Reading DICOM JSON (PS3.18 F) for display.

import type { DicomJson } from "./api";

export const TAG = {
  PatientName: "00100010",
  PatientID: "00100020",
  PatientBirthDate: "00100030",
  PatientSex: "00100040",
  StudyDate: "00080020",
  StudyTime: "00080030",
  AccessionNumber: "00080050",
  ModalitiesInStudy: "00080061",
  Modality: "00080060",
  StudyDescription: "00081030",
  SeriesDescription: "0008103E",
  StudyInstanceUID: "0020000D",
  SeriesInstanceUID: "0020000E",
  SOPInstanceUID: "00080018",
  SeriesNumber: "00200011",
  InstanceNumber: "00200013",
  NumberOfFrames: "00280008",
  NumberOfStudyRelatedSeries: "00201206",
  NumberOfStudyRelatedInstances: "00201208",
  NumberOfSeriesRelatedInstances: "00201209",
} as const;

/** Keywords of common attributes, for the metadata view; others show their tag only. */
const KEYWORDS: Record<string, string> = {
  ...Object.fromEntries(Object.entries(TAG).map(([k, v]) => [v, k])),
  "00020010": "TransferSyntaxUID",
  "00080005": "SpecificCharacterSet",
  "00080008": "ImageType",
  "00080016": "SOPClassUID",
  "00080021": "SeriesDate",
  "00080022": "AcquisitionDate",
  "00080023": "ContentDate",
  "00080031": "SeriesTime",
  "00080032": "AcquisitionTime",
  "00080033": "ContentTime",
  "00080070": "Manufacturer",
  "00080080": "InstitutionName",
  "00080090": "ReferringPhysicianName",
  "00081010": "StationName",
  "00081090": "ManufacturerModelName",
  "00180015": "BodyPartExamined",
  "00180050": "SliceThickness",
  "00180060": "KVP",
  "00181030": "ProtocolName",
  "00185100": "PatientPosition",
  "00200010": "StudyID",
  "00200032": "ImagePositionPatient",
  "00200037": "ImageOrientationPatient",
  "00200052": "FrameOfReferenceUID",
  "00201041": "SliceLocation",
  "00280002": "SamplesPerPixel",
  "00280004": "PhotometricInterpretation",
  "00280010": "Rows",
  "00280011": "Columns",
  "00280030": "PixelSpacing",
  "00280100": "BitsAllocated",
  "00280101": "BitsStored",
  "00280102": "HighBit",
  "00280103": "PixelRepresentation",
  "00281050": "WindowCenter",
  "00281051": "WindowWidth",
  "00281052": "RescaleIntercept",
  "00281053": "RescaleSlope",
  "7FE00010": "PixelData",
};

export function keyword(tag: string): string {
  return KEYWORDS[tag.toUpperCase()] ?? "";
}

/** `(GGGG,EEEE)`. */
export function formatTag(tag: string): string {
  return `(${tag.slice(0, 4)},${tag.slice(4)})`;
}

/** An attribute's value as text: person names by their alphabetic form, several values joined
 * with `\`, sequences as their item count. */
export function text(obj: DicomJson | undefined, tag: string): string {
  const element = obj?.[tag];
  if (!element) return "";
  if (element.BulkDataURI !== undefined) return "(binary)";
  const values = element.Value ?? [];
  if (element.vr === "SQ") return `${values.length} item${values.length === 1 ? "" : "s"}`;
  return values
    .map((v) => {
      if (v && typeof v === "object") {
        const pn = v as { Alphabetic?: string };
        return pn.Alphabetic ?? JSON.stringify(v);
      }
      return String(v);
    })
    .join("\\");
}

export function number(obj: DicomJson | undefined, tag: string): number | undefined {
  const v = obj?.[tag]?.Value?.[0];
  return typeof v === "number" ? v : typeof v === "string" && v.trim() ? Number(v) : undefined;
}

/** `YYYYMMDD` as a locale date. */
export function formatDate(da: string): string {
  if (!/^\d{8}$/.test(da)) return da;
  return new Date(Number(da.slice(0, 4)), Number(da.slice(4, 6)) - 1, Number(da.slice(6, 8))).toLocaleDateString();
}

/** A person name for display: `Doe^John` → `Doe, John`. */
export function formatName(pn: string): string {
  const [family, given, middle] = pn.split("^");
  return [family, [given, middle].filter(Boolean).join(" ")].filter(Boolean).join(", ");
}
