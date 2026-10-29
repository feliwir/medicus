import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import type { ColumnDef } from "@tanstack/react-table";
import { Search } from "lucide-react";
import { type FormEvent, useEffect, useState } from "react";
import { ErrorAlert, Field, Page } from "@/components/page";
import { Alert } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DataTable } from "@/components/ui/data-table";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { ApiError, type DicomJson, api, dicomweb } from "@/lib/api";
import { TAG, formatDate, formatName, text } from "@/lib/dicom";

const PAGE = 50;
const NODE_KEY = "medicus-browse-node";

const INCLUDE = [TAG.StudyDescription, TAG.ModalitiesInStudy, TAG.NumberOfStudyRelatedSeries, TAG.NumberOfStudyRelatedInstances].join(",");

interface Filters {
  name: string;
  id: string;
  accession: string;
  from: string;
  to: string;
  modality: string;
}

const noFilters: Filters = { name: "", id: "", accession: "", from: "", to: "", modality: "" };

/** The QIDO-RS query of the filters: names match by prefix, dates as a range. */
export function studyQuery(f: Filters, offset: number): string {
  const p = new URLSearchParams({ limit: String(PAGE + 1), offset: String(offset), includefield: INCLUDE });
  if (f.name.trim()) p.set("PatientName", `${f.name.trim()}*`);
  if (f.id.trim()) p.set("PatientID", f.id.trim());
  if (f.accession.trim()) p.set("AccessionNumber", f.accession.trim());
  if (f.modality) p.set("ModalitiesInStudy", f.modality);
  const da = (d: string) => d.replaceAll("-", "");
  if (f.from || f.to) p.set("StudyDate", `${da(f.from)}-${da(f.to)}`);
  return p.toString();
}

export function Studies() {
  const navigate = useNavigate();
  const nodes = useQuery({ queryKey: ["nodes"], queryFn: api.nodes });
  const served = (nodes.data ?? []).filter((n) => n.dicomweb_path && !n.remote);
  const [path, setPath] = useState<string>(() => {
    try {
      return localStorage.getItem(NODE_KEY) ?? "";
    } catch {
      return "";
    }
  });
  const node = served.find((n) => n.dicomweb_path === path) ?? served[0];
  useEffect(() => {
    try {
      if (node?.dicomweb_path) localStorage.setItem(NODE_KEY, node.dicomweb_path);
    } catch {
      // Not remembered.
    }
  }, [node?.dicomweb_path]);

  const [draft, setDraft] = useState<Filters>(noFilters);
  const [filters, setFilters] = useState<Filters>(noFilters);
  const [offset, setOffset] = useState(0);
  const studies = useQuery({
    queryKey: ["studies", node?.dicomweb_path, filters, offset],
    queryFn: () => dicomweb<DicomJson[]>(`/nodes/${node?.dicomweb_path}/rs/studies?${studyQuery(filters, offset)}`),
    enabled: Boolean(node),
  });
  const rows = (studies.data ?? []).slice(0, PAGE);
  const more = (studies.data?.length ?? 0) > PAGE;

  const submit = (e: FormEvent) => {
    e.preventDefault();
    setOffset(0);
    setFilters(draft);
  };

  const columns: ColumnDef<DicomJson, unknown>[] = [
    {
      id: "patient",
      header: "Patient",
      accessorFn: (s) => text(s, TAG.PatientName),
      cell: ({ row }) => (
        <div>
          <div className="font-medium">{formatName(text(row.original, TAG.PatientName)) || "—"}</div>
          <div className="font-mono text-xs text-muted-foreground">{text(row.original, TAG.PatientID)}</div>
        </div>
      ),
    },
    { id: "date", header: "Date", accessorFn: (s) => text(s, TAG.StudyDate), cell: ({ getValue }) => formatDate(getValue() as string) },
    { id: "description", header: "Description", accessorFn: (s) => text(s, TAG.StudyDescription) },
    {
      id: "modalities",
      header: "Modalities",
      accessorFn: (s) => text(s, TAG.ModalitiesInStudy),
      cell: ({ getValue }) => (
        <div className="flex gap-1">
          {(getValue() as string)
            .split("\\")
            .filter(Boolean)
            .map((m) => (
              <Badge key={m} variant="outline">
                {m}
              </Badge>
            ))}
        </div>
      ),
    },
    { id: "accession", header: "Accession", accessorFn: (s) => text(s, TAG.AccessionNumber), cell: ({ getValue }) => <span className="font-mono">{getValue() as string}</span> },
    {
      id: "count",
      header: "Series / images",
      accessorFn: (s) => Number(text(s, TAG.NumberOfStudyRelatedInstances) || 0),
      cell: ({ row }) => (
        <span className="font-mono text-muted-foreground">
          {text(row.original, TAG.NumberOfStudyRelatedSeries)} / {text(row.original, TAG.NumberOfStudyRelatedInstances)}
        </span>
      ),
    },
  ];

  if (nodes.data && served.length === 0) {
    return (
      <Page title="Browse" description="The studies stored in a node.">
        <Alert variant="info">
          <div>
            No local node is served over DICOMweb, which the browser reads through. Give a node a DICOMweb path in{" "}
            <Link to="/config/nodes" className="underline">
              Nodes
            </Link>{" "}
            and enable the dicomweb addon in{" "}
            <Link to="/config/addons" className="underline">
              Addons
            </Link>
            .
          </div>
        </Alert>
      </Page>
    );
  }
  const disabled = studies.error instanceof ApiError && studies.error.code === "dicomweb_disabled";

  return (
    <Page
      title="Browse"
      description="The studies stored in a node, as its DICOMweb clients see them."
      actions={
        served.length > 1 && (
          <Select
            aria-label="Node"
            className="w-56"
            value={node?.dicomweb_path}
            onChange={(e) => {
              setPath(e.target.value);
              setOffset(0);
            }}
          >
            {served.map((n) => (
              <option key={n.name} value={n.dicomweb_path}>
                {n.name}
              </option>
            ))}
          </Select>
        )
      }
    >
      <form onSubmit={submit} className="flex flex-wrap items-end gap-3">
        <Field id="f-name" label="Patient name">
          <Input id="f-name" placeholder="Begins with…" className="w-52" value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
        </Field>
        <Field id="f-id" label="Patient ID">
          <Input id="f-id" className="w-40 font-mono" value={draft.id} onChange={(e) => setDraft({ ...draft, id: e.target.value })} />
        </Field>
        <Field id="f-acc" label="Accession">
          <Input id="f-acc" className="w-40 font-mono" value={draft.accession} onChange={(e) => setDraft({ ...draft, accession: e.target.value })} />
        </Field>
        <Field id="f-from" label="From">
          <Input id="f-from" type="date" className="w-44" value={draft.from} onChange={(e) => setDraft({ ...draft, from: e.target.value })} />
        </Field>
        <Field id="f-to" label="To">
          <Input id="f-to" type="date" className="w-44" value={draft.to} onChange={(e) => setDraft({ ...draft, to: e.target.value })} />
        </Field>
        <Field id="f-mod" label="Modality">
        <Select id="f-mod" className="w-36" value={draft.modality} onChange={(e) => setDraft({ ...draft, modality: e.target.value })}>
          <option value="">Any</option>
          {["CT", "MR", "CR", "DX", "US", "MG", "NM", "PT", "XA", "RF", "OT", "SR", "SEG"].map((m) => (
            <option key={m}>{m}</option>
          ))}
        </Select>
        </Field>
        <Button type="submit">
          <Search /> Search
        </Button>
        {JSON.stringify(draft) !== JSON.stringify(noFilters) && (
          <Button
            type="button"
            variant="ghost"
            onClick={() => {
              setDraft(noFilters);
              setFilters(noFilters);
              setOffset(0);
            }}
          >
            Clear
          </Button>
        )}
      </form>
      {disabled ? (
        <Alert variant="info">
          <div>
            The dicomweb addon is not enabled; the browser reads through it. Enable it in{" "}
            <Link to="/config/addons" className="underline">
              Addons
            </Link>
            .
          </div>
        </Alert>
      ) : (
        <ErrorAlert error={studies.error} />
      )}
      <DataTable
        columns={columns}
        data={rows}
        rowKey={(s) => text(s, TAG.StudyInstanceUID)}
        onRowClick={(s) =>
          void navigate({ to: "/browse/$node/$study", params: { node: node?.dicomweb_path ?? "", study: text(s, TAG.StudyInstanceUID) } })
        }
        empty={studies.isLoading ? "Searching…" : "No studies match."}
      />
      <div className="flex items-center justify-end gap-2">
        <span className="text-sm text-muted-foreground">
          {rows.length ? `${offset + 1}–${offset + rows.length}` : ""}
        </span>
        <Button variant="outline" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE))}>
          Previous
        </Button>
        <Button variant="outline" disabled={!more} onClick={() => setOffset(offset + PAGE)}>
          Next
        </Button>
      </div>
    </Page>
  );
}
