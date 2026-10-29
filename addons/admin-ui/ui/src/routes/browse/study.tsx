import { useQuery } from "@tanstack/react-query";
import { Link, useParams } from "@tanstack/react-router";
import { ArrowLeft, ChevronLeft, ChevronRight, FileText, ImageOff } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { ErrorAlert } from "@/components/page";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Select } from "@/components/ui/select";
import { type DicomJson, api, dicomweb } from "@/lib/api";
import { TAG, formatDate, formatName, formatTag, keyword, number, text } from "@/lib/dicom";
import { cn } from "@/lib/utils";

/** Modalities of series without images to show (presentation states, reports, structures). */
const NON_IMAGE = new Set(["PR", "SR", "KO", "DOC", "RTSTRUCT", "RTPLAN", "RTRECORD", "REG", "FID", "AU", "ECG", "HD"]);

const hasImages = (series: DicomJson) => !NON_IMAGE.has(text(series, TAG.Modality));

/** Window/level presets: centre and width, or the image's own values. */
const PRESETS: { name: string; window?: [number, number] }[] = [
  { name: "As stored" },
  { name: "Soft tissue", window: [40, 400] },
  { name: "Lung", window: [-600, 1500] },
  { name: "Bone", window: [300, 1500] },
  { name: "Brain", window: [40, 80] },
  { name: "Abdomen", window: [60, 400] },
];

function Thumbnail({ src, alt, images }: { src: string; alt: string; images: boolean }) {
  const [failed, setFailed] = useState(false);
  return failed || !images ? (
    <div className="grid size-full place-items-center text-xs text-neutral-400">
      {images ? <ImageOff className="size-5" /> : "no images"}
    </div>
  ) : (
    <img src={src} alt={alt} loading="lazy" className="size-full object-contain" onError={() => setFailed(true)} />
  );
}

/** One image of the stack: an instance, or a frame of a multi-frame instance. */
interface Image {
  instance: string;
  frame?: number;
}

function Metadata({ base, instance }: { base: string; instance: string }) {
  const metadata = useQuery({
    queryKey: ["metadata", base, instance],
    queryFn: () => dicomweb<DicomJson[]>(`${base}/instances/${instance}/metadata`),
  });
  const obj = metadata.data?.[0];
  if (metadata.error) return <ErrorAlert error={metadata.error} />;
  if (!obj) return <p className="p-4 text-sm text-neutral-400">Loading…</p>;
  return (
    <table className="w-full text-xs">
      <tbody className="divide-y divide-neutral-800">
        {Object.keys(obj)
          .sort()
          .map((tag) => (
            <tr key={tag}>
              <td className="px-3 py-1.5 font-mono whitespace-nowrap text-neutral-400">{formatTag(tag)}</td>
              <td className="px-3 py-1.5 whitespace-nowrap">{keyword(tag)}</td>
              <td className="px-3 py-1.5 font-mono text-neutral-400">{obj[tag]?.vr}</td>
              <td className="max-w-80 truncate px-3 py-1.5 font-mono" title={text(obj, tag)}>
                {text(obj, tag)}
              </td>
            </tr>
          ))}
      </tbody>
    </table>
  );
}

/** A series as a stack of rendered images (server-side rendering through WADO-RS). */
function Preview({ root, study, series }: { root: string; study: string; series: DicomJson }) {
  const uid = text(series, TAG.SeriesInstanceUID);
  const base = `${root}/studies/${study}/series/${uid}`;
  const instances = useQuery({
    queryKey: ["instances", base],
    queryFn: () => dicomweb<DicomJson[]>(`${base}/instances?includefield=${TAG.InstanceNumber},${TAG.NumberOfFrames}`),
  });
  const images: Image[] = useMemo(() => {
    const sorted = [...(instances.data ?? [])].sort(
      (a, b) => (number(a, TAG.InstanceNumber) ?? 0) - (number(b, TAG.InstanceNumber) ?? 0),
    );
    return sorted.flatMap((i) => {
      const frames = number(i, TAG.NumberOfFrames) ?? 1;
      const instance = text(i, TAG.SOPInstanceUID);
      return frames > 1 ? Array.from({ length: frames }, (_, f) => ({ instance, frame: f + 1 })) : [{ instance }];
    });
  }, [instances.data]);
  const [index, setIndex] = useState(0);
  const [preset, setPreset] = useState(0);
  const [showMetadata, setShowMetadata] = useState(false);
  const [failed, setFailed] = useState<string>();
  const stage = useRef<HTMLDivElement>(null);
  useEffect(() => setIndex(0), [base]);

  const url = (image: Image | undefined) => {
    if (!image) return "";
    const path = image.frame ? `${base}/instances/${image.instance}/frames/${image.frame}/rendered` : `${base}/instances/${image.instance}/rendered`;
    const w = PRESETS[preset]?.window;
    return w ? `${path}?window=${w[0]},${w[1]},linear` : path;
  };
  // Preload the neighbours, so scrolling does not wait for each image.
  useEffect(() => {
    for (const i of [index + 1, index + 2, index - 1]) {
      const image = images[i];
      if (image) new window.Image().src = url(image);
    }
  });

  const step = (delta: number) => setIndex((i) => Math.min(images.length - 1, Math.max(0, i + delta)));
  const current = images[index];

  return (
    // The viewer is dark in both themes, as reading rooms are.
    <div className="flex h-full min-h-[28rem] flex-col overflow-hidden rounded-md bg-neutral-950 text-neutral-100 lg:flex-row">
      <div className="flex min-w-0 flex-1 flex-col">
        <div className="flex items-center gap-2 border-b border-neutral-800 px-3 py-2">
          <span className="min-w-0 flex-1 truncate text-sm">
            {text(series, TAG.Modality)} · {text(series, TAG.SeriesDescription) || `Series ${text(series, TAG.SeriesNumber)}`}
          </span>
          <Select
            aria-label="Window"
            className="w-40 shrink-0 [&_select]:h-9 [&_select]:border-neutral-700 [&_select]:bg-neutral-900 [&_select]:text-sm [&_select]:text-neutral-100"
            value={preset}
            onChange={(e) => setPreset(Number(e.target.value))}
          >
            {PRESETS.map((p, i) => (
              <option key={p.name} value={i}>
                {p.name}
              </option>
            ))}
          </Select>
          <Button
            size="sm"
            variant="ghost"
            className={cn("shrink-0 text-neutral-100 hover:bg-white/10", showMetadata && "bg-white/10")}
            onClick={() => setShowMetadata(!showMetadata)}
          >
            <FileText /> Attributes
          </Button>
        </div>
        <div
          ref={stage}
          tabIndex={0}
          aria-label="Image stack: scroll or use the arrow keys"
          className="relative grid min-h-0 flex-1 place-items-center p-2 outline-none focus-visible:ring-2 focus-visible:ring-ring"
          onWheel={(e) => step(e.deltaY > 0 ? 1 : -1)}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown" || e.key === "ArrowRight" || e.key === "PageDown") {
              e.preventDefault();
              step(e.key === "PageDown" ? 10 : 1);
            } else if (e.key === "ArrowUp" || e.key === "ArrowLeft" || e.key === "PageUp") {
              e.preventDefault();
              step(e.key === "PageUp" ? -10 : -1);
            } else if (e.key === "Home") setIndex(0);
            else if (e.key === "End") setIndex(images.length - 1);
          }}
        >
          {instances.error ? (
            <div className="p-4">
              <ErrorAlert error={instances.error} />
            </div>
          ) : !hasImages(series) ? (
            <span className="max-w-sm px-4 text-center text-sm text-neutral-400">
              This series ({text(series, TAG.Modality)}) holds no images to show. Its attributes are available.
            </span>
          ) : current && failed === url(current) ? (
            <span className="max-w-sm px-4 text-center text-sm text-neutral-400">
              The server cannot render this image, e.g. because it cannot decode its transfer syntax.
            </span>
          ) : current ? (
            <img
              src={url(current)}
              alt={`Image ${index + 1} of ${images.length}`}
              className="max-h-full max-w-full object-contain select-none"
              draggable={false}
              onError={() => setFailed(url(current))}
            />
          ) : (
            <span className="text-sm text-neutral-400">Loading…</span>
          )}
          {images.length > 0 && (
            <span className="absolute top-2 left-3 font-mono text-xs text-neutral-300">
              {index + 1} / {images.length}
              {current?.frame && ` · frame ${current.frame}`}
            </span>
          )}
        </div>
        {images.length > 1 && (
          <div className="flex items-center gap-2 border-t border-neutral-800 px-2 py-1">
            <Button size="icon" variant="ghost" className="text-neutral-100 hover:bg-white/10" aria-label="Previous image" onClick={() => step(-1)}>
              <ChevronLeft />
            </Button>
            <input
              type="range"
              aria-label="Image"
              min={0}
              max={images.length - 1}
              value={index}
              onChange={(e) => setIndex(Number(e.target.value))}
              className="flex-1 accent-sky-400"
            />
            <Button size="icon" variant="ghost" className="text-neutral-100 hover:bg-white/10" aria-label="Next image" onClick={() => step(1)}>
              <ChevronRight />
            </Button>
          </div>
        )}
      </div>
      {showMetadata && current && (
        <div className="max-h-full overflow-auto border-t border-neutral-800 lg:w-[30rem] lg:border-t-0 lg:border-l">
          <Metadata base={base} instance={current.instance} />
        </div>
      )}
    </div>
  );
}

export function Study() {
  const { node = "", study = "" } = useParams({ strict: false });
  const nodes = useQuery({ queryKey: ["nodes"], queryFn: api.nodes });
  const root = nodes.data?.find((n) => n.dicomweb_path === node)?.dicomweb_url ?? "";
  const info = useQuery({
    enabled: Boolean(root),
    queryKey: ["study", root, study],
    queryFn: () =>
      dicomweb<DicomJson[]>(
        `${root}/studies?StudyInstanceUID=${study}&includefield=${[TAG.PatientBirthDate, TAG.PatientSex, TAG.StudyDescription, TAG.ModalitiesInStudy].join(",")}`,
      ),
  });
  const series = useQuery({
    enabled: Boolean(root),
    queryKey: ["series", root, study],
    queryFn: () =>
      dicomweb<DicomJson[]>(
        `${root}/studies/${study}/series?includefield=${[TAG.SeriesDescription, TAG.NumberOfSeriesRelatedInstances].join(",")}`,
      ),
  });
  // Image series first, then by number.
  const sorted = [...(series.data ?? [])].sort(
    (a, b) =>
      Number(hasImages(b)) - Number(hasImages(a)) ||
      (number(a, TAG.SeriesNumber) ?? 0) - (number(b, TAG.SeriesNumber) ?? 0),
  );
  const [selected, setSelected] = useState<string>();
  const current = sorted.find((s) => text(s, TAG.SeriesInstanceUID) === selected) ?? sorted[0];
  const s = info.data?.[0];

  return (
    <div className="space-y-6 p-6">
      <div>
        <Button asChild variant="ghost" className="-ml-4">
          <Link to="/browse">
            <ArrowLeft /> Studies
          </Link>
        </Button>
        <h1 className="mt-2 text-headline-sm">{s ? formatName(text(s, TAG.PatientName)) || "Unnamed patient" : "Study"}</h1>
        {s && (
          <dl className="mt-2 flex flex-wrap gap-x-6 gap-y-1 text-sm">
            {(
              [
                ["Patient ID", text(s, TAG.PatientID)],
                ["Born", formatDate(text(s, TAG.PatientBirthDate))],
                ["Sex", text(s, TAG.PatientSex)],
                ["Study date", formatDate(text(s, TAG.StudyDate))],
                ["Description", text(s, TAG.StudyDescription)],
                ["Accession", text(s, TAG.AccessionNumber)],
              ] as const
            )
              .filter(([, v]) => v)
              .map(([k, v]) => (
                <div key={k} className="flex gap-2">
                  <dt className="text-muted-foreground">{k}</dt>
                  <dd>{v}</dd>
                </div>
              ))}
          </dl>
        )}
        <p className="mt-1 font-mono text-xs text-muted-foreground">{study}</p>
      </div>
      <ErrorAlert error={info.error ?? series.error} />
      {/* List and detail: the series beside the viewer, which fills the remaining height. */}
      <div className="grid gap-4 lg:h-[calc(100vh-14rem)] lg:min-h-[32rem] lg:grid-cols-[15rem_1fr]">
        <ul className="flex gap-2 overflow-x-auto lg:flex-col lg:overflow-x-visible lg:overflow-y-auto lg:pr-1" aria-label="Series">
          {sorted.map((se) => {
            const uid = text(se, TAG.SeriesInstanceUID);
            const active = current && uid === text(current, TAG.SeriesInstanceUID);
            return (
              <li key={uid} className="w-56 shrink-0 lg:w-auto">
                <button
                  type="button"
                  onClick={() => setSelected(uid)}
                  aria-pressed={active}
                  className={cn(
                    "flex w-full items-center gap-3 rounded-md border bg-card p-2 text-left transition-colors hover:border-primary",
                    active && "border-primary bg-accent ring-1 ring-primary",
                  )}
                >
                  <div className="size-16 shrink-0 overflow-hidden rounded-xs bg-neutral-950">
                    <Thumbnail
                      src={`${root}/studies/${study}/series/${uid}/thumbnail?viewport=128,128`}
                      alt=""
                      images={hasImages(se)}
                    />
                  </div>
                  <div className="min-w-0 flex-1 space-y-1">
                    <div className="truncate text-sm font-medium">{text(se, TAG.SeriesDescription) || "Unnamed series"}</div>
                    <div className="flex items-center gap-1 text-xs text-muted-foreground">
                      <Badge variant="outline">{text(se, TAG.Modality)}</Badge>#{text(se, TAG.SeriesNumber)} ·{" "}
                      {text(se, TAG.NumberOfSeriesRelatedInstances)}
                    </div>
                  </div>
                </button>
              </li>
            );
          })}
        </ul>
        <div className="h-[70vh] min-h-0 lg:h-auto">
          {current && <Preview root={root} study={study} series={current} />}
        </div>
      </div>
    </div>
  );
}
