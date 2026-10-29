<!--typst-begin-exclude-->
# medicus DICOM Conformance Statement

> The PDF edition is built from this file: `typst compile docs/conformance/conformance-statement.typ`.
> Tables between `generated` markers are written by `cargo run -p medicus-conformance` and checked by the tests; do not edit them by hand.
<!--typst-end-exclude-->

<!-- generated:product -->

| Product | Version |
| --- | --- |
| medicus | 0.1.0 |

<!-- /generated:product -->

## Overview

medicus is a picture archiving and communication system (PACS): it stores DICOM composite instances received over the network, makes them searchable, and returns them to other applications. It offers two network front-ends, both optional addons of the server:

- **DIMSE**: Verification, Storage, and Query/Retrieve (C-FIND, C-MOVE, C-GET) in the Patient Root and Study Root information models.
- **DICOMweb**: QIDO-RS, WADO-RS (including rendered resources) and STOW-RS.

Data are organized in *nodes* and *partitions*. A node is a named endpoint — a DIMSE AE title and/or a DICOMweb path — backed by one partition, a separate data set with its own patients, studies and blobs. One server process serves any number of nodes. Every operation is authorized per node and caller (see [Security](#security)).

Instances are stored and returned exactly as received: medicus neither transcodes nor coerces attributes.

| Network service | User of service (SCU) | Provider of service (SCP) |
| --- | --- | --- |
| Verification | Yes | Yes |
| Storage | Yes | Yes |
| Patient Root Query/Retrieve – FIND | No | Yes |
| Patient Root Query/Retrieve – MOVE | No | Yes |
| Patient Root Query/Retrieve – GET | No | Yes |
| Study Root Query/Retrieve – FIND | No | Yes |
| Study Root Query/Retrieve – MOVE | No | Yes |
| Study Root Query/Retrieve – GET | No | Yes |

| DICOMweb service | User agent | Origin server |
| --- | --- | --- |
| Search (QIDO-RS) | No | Yes |
| Retrieve (WADO-RS), including rendered resources | No | Yes |
| Store (STOW-RS) | No | Yes |

Media interchange is not supported.

## Introduction

### Audience

This document is written for integrators and administrators who connect medicus to modalities, workstations, viewers and other archives. It assumes familiarity with the DICOM Standard, in particular PS3.2, PS3.4, PS3.7, PS3.8 and PS3.18.

### Remarks

This statement describes the behaviour of the version named above with the addons `dimse` and `dicomweb` as distributed. Conformance with DICOM does not by itself guarantee interoperability; the integration of medicus with equipment of other vendors should be validated.

medicus is open source. Where this document and the source code differ, the source code is authoritative; please report the difference.

### Terms and definitions

| Term | Meaning |
| --- | --- |
| Node | A named endpoint of medicus: a DIMSE AE title and/or a DICOMweb path, backed by one partition. |
| Partition | A separate data set: every patient, study, series and instance belongs to exactly one partition. UIDs are unique per partition. |
| Principal | A configured caller (a user or a remote application) that is granted permissions on nodes through roles. |
| Peer | A configured remote DIMSE application that medicus may connect to (C-MOVE destination, send jobs). |

### References

- NEMA PS3 / ISO 12052, *Digital Imaging and Communications in Medicine (DICOM) Standard*.
- medicus architecture: `docs/PLAN.md`; nodes, partitions and access control: `docs/adr/0003-nodes-partitions-access.md`.

## Networking

### Implementation model

#### Application data flow

One application entity per node. Each node's AE title is served by the same DIMSE listener; the *called* AE title of an association selects the node, and with it the partition the association operates on.

- A remote application that sends instances (C-STORE) has them stored in the node's partition.
- A remote application that queries (C-FIND) receives matches from the node's partition.
- A remote application that retrieves (C-MOVE, C-GET) has instances of the node's partition sent to a move destination or back over the same association.
- An administrator may ask medicus to verify a peer (C-ECHO) or to send instances to it (send jobs).

#### Functional definition of application entities

Each node's AE accepts associations from remote applications and serves Verification, Storage and Query/Retrieve as SCP. It initiates associations as Storage SCU for C-MOVE sub-operations and send jobs, and as Verification SCU when a peer is verified.

Instances received over DIMSE and DICOMweb go through the same ingest: a received instance is stored, indexed and announced identically whichever protocol delivered it.

#### Sequencing of real-world activities

None. Instances are available for query and retrieval as soon as their C-STORE response (or STOW-RS response) has been sent.

### Application entity specifications

#### SOP classes

<!-- generated:network-sop-classes -->

| SOP Class | SOP Class UID | SCU | SCP |
| --- | --- | --- | --- |
| Verification SOP Class | `1.2.840.10008.1.1` | Yes | Yes |
| Patient Root Query/Retrieve Information Model - FIND | `1.2.840.10008.5.1.4.1.2.1.1` | No | Yes |
| Study Root Query/Retrieve Information Model - FIND | `1.2.840.10008.5.1.4.1.2.2.1` | No | Yes |
| Patient Root Query/Retrieve Information Model - MOVE | `1.2.840.10008.5.1.4.1.2.1.2` | No | Yes |
| Study Root Query/Retrieve Information Model - MOVE | `1.2.840.10008.5.1.4.1.2.2.2` | No | Yes |
| Patient Root Query/Retrieve Information Model - GET | `1.2.840.10008.5.1.4.1.2.1.3` | No | Yes |
| Study Root Query/Retrieve Information Model - GET | `1.2.840.10008.5.1.4.1.2.2.3` | No | Yes |
| Storage SOP classes (189, see [Annex A](#annex-a-storage-sop-classes)) | – | Yes | Yes |

<!-- /generated:network-sop-classes -->

Further Storage SOP classes, including private ones, can be accepted with the `extra_sop_classes` setting. As Storage SCU, medicus sends instances of whatever SOP class they were stored with.

#### Association policies

##### General

The DICOM Application Context Name `1.2.840.10008.3.1.1.1` is the only one proposed and accepted.

| Parameter | Value |
| --- | --- |
| Maximum PDU size received (SCP) | 65536 bytes, configurable (`max_pdu_length`) |
| Maximum PDU size received (SCU) | 65536 bytes |
| PDUs sent | Fragmented to the peer's maximum PDU size; 1 MiB if the peer sets no limit |

##### Number of associations

| Parameter | Value |
| --- | --- |
| Simultaneous associations accepted | 64, configurable (`max_associations`); further connections wait for a free slot rather than being rejected |
| Simultaneous associations initiated | One per C-MOVE in progress, plus one per running send job (4 jobs at a time by default) |

##### Asynchronous nature

Asynchronous operations are not supported; the Asynchronous Operations Window is not negotiated. One operation runs at a time on an association: while a C-FIND, C-MOVE or C-GET is in progress, only a C-CANCEL for it is accepted, and any other request aborts the association.

##### Implementation identifying information

| Parameter | Value |
| --- | --- |
| Implementation Class UID in association negotiation | `2.25.175220362680028036156011002668768058626` (dicom-rs) |
| Implementation Version Name in association negotiation | `DICOM-rs 0.10.0` |
| Implementation Class UID in stored File Meta Information | `2.25.21299839958676251082708783506226217531` |
| Implementation Version Name in stored File Meta Information | `MEDICUS_` followed by the product version |

#### Association initiation policy

medicus initiates associations as Storage SCU and as Verification SCU.

##### Storage SCU: C-MOVE sub-operations and send jobs

*Trigger.* A C-MOVE request whose move destination is a configured peer, or a send job (submitted through the archive's services, and retried by its job engine).

*Calling AE title.* For C-MOVE sub-operations, the called AE title of the association that carried the C-MOVE request; for send jobs, the AE title of the source node (`MEDICUS` if it has none).

*Proposed presentation contexts.* One presentation context per distinct combination of SOP class and transfer syntax among the instances to send, each with exactly that SOP class as abstract syntax and the stored transfer syntax as its only transfer syntax. At most 128 presentation contexts are proposed per association; send jobs with more combinations use several associations. Instances are never transcoded.

| Situation | Behaviour |
| --- | --- |
| The peer rejects an instance's presentation context | The instance fails (status 0122) without being sent. |
| The peer cannot be reached (C-MOVE) | The C-MOVE fails with status A702. |
| The peer cannot be reached (send job) | The job is retried with exponential back-off: 30 s, doubling per attempt, at most 1 h, 5 attempts by default. |
| The association breaks while sending | The instance being sent and all remaining instances fail; a send job retries only the failed instances. |

C-STORE requests are sent with priority MEDIUM. The SCU uses a connection, read and write timeout of 60 seconds. SCU responses with warning statuses (0001, 0107, 0116, Bxxx) count as warnings; any other non-success status counts as a failure.

Move Originator Application Entity Title and Move Originator Message ID are set in C-STORE requests that are C-MOVE sub-operations.

##### Verification SCU

An administrator can verify a configured peer through the administrative API. medicus then opens an association with calling AE title `MEDICUS`, sends a C-ECHO request and releases the association.

#### Association acceptance policy

*Called AE title.* Associations must be addressed to the AE title of a node, on the node's port; any other called AE title is rejected with A-ASSOCIATE-RJ (rejected-permanent, DICOM UL service-user, "called-AE-title-not-recognized"). Associations with an unsupported protocol version or application context name are rejected by the same service-user source.

*Caller.* The calling AE title and the peer's network address identify the caller: the first principal whose `calling_ae` equals the calling AE title and whose `networks` (CIDR list, if any) contain the peer's address. Callers that match no principal are anonymous. A caller that has no permission at all on the called node is refused: the association is established and immediately aborted (A-ABORT, source DICOM UL service-provider).

*Timeouts.* An association that is silent for 300 seconds (configurable, `idle_timeout_secs`, 0 disables) is aborted; the same limit applies to association establishment. When the server shuts down, running associations get 10 seconds to complete before they are aborted.

*Role selection.* For Storage SOP classes, SCP/SCU role selection is accepted as proposed, so that a C-GET requestor may take the Storage SCP role. No role selection is returned for other SOP classes.

*Extended negotiation* is not supported: relational queries, combined date-time matching, fuzzy semantic matching of person names, timezone query adjustment, enhanced multi-frame image conversion and SOP class common extended negotiation are not negotiated and not applied.

##### Verification SCP

Every C-ECHO request on an admitted association is answered with status 0000 (Success).

##### Storage SCP

*Permission.* The caller needs the `store` permission on the node.

*Storage.* The data set is stored byte for byte as received, preceded by File Meta Information that medicus generates: Media Storage SOP Class and Instance UID from the C-STORE request, the presentation context's transfer syntax, the implementation identification above, and Source Application Entity Title set to the calling AE title. No attributes are coerced, removed or added, and no transcoding takes place. Patient, study and series attributes in the index are taken from the most recently stored instance.

*Level of support.* Level 2 (Full) in the sense that all Type 1, 2 and 3 attributes, private attributes included, are retained. Pixel data are not decompressed or validated.

*Duplicates.* When an instance with the same SOP Instance UID is already stored in the partition, the configured duplicate policy applies (`archive.duplicate_policy`, the same for all partitions):

| Policy | Behaviour | Status |
| --- | --- | --- |
| `overwrite` (default) | The new instance replaces the stored one. | 0000 |
| `keep_existing` | The new instance is discarded. | 0000 |
| `reject` | The new instance is refused. | C000 |

*Status codes.* Every status other than success carries an Error Comment (0000,0902).

| Status | Code | Meaning |
| --- | --- | --- |
| Success | 0000 | Stored (or discarded as a duplicate under `keep_existing`). |
| Refused | 0124 | The caller lacks the `store` permission. |
| Refused | 0122 | The Affected SOP Class UID differs from the presentation context's abstract syntax. |
| Refused | A700 | Out of resources: storage is critically full (default: at 95 % usage, until usage falls below 85 %), the header exceeds 64 MiB (`max_header_bytes`), or the storage is temporarily unavailable. |
| Error | C000 | Cannot understand: the data set cannot be parsed, lacks the Study, Series or SOP Instance UID, is a refused duplicate, or could not be stored. |

Warning statuses are never returned. A data set whose SOP Instance UID differs from the one in the C-STORE request is stored under the data set's UID.

##### Query/Retrieve SCP: C-FIND

*Permission.* The caller needs the `query` permission on the node.

*Levels.*

| Information model | Levels |
| --- | --- |
| Patient Root | PATIENT, STUDY, SERIES, IMAGE |
| Study Root | STUDY, SERIES, IMAGE |

Relational queries are not supported. The unique keys of higher levels are not required.

*Keys.* Any standard attribute of the query level or a higher level may be used as a matching key or requested as a return key. The following are indexed; other attributes are matched and returned from the stored header of a representative instance of each match, which is slower but complete:

| Level | Indexed attributes |
| --- | --- |
| Patient | Patient's Name, Patient ID, Issuer of Patient ID, Patient's Birth Date, Patient's Sex, Number of Patient Related Studies |
| Study | Study Instance UID, Study Date, Study Time, Accession Number, Study ID, Referring Physician's Name, Study Description, Modalities in Study, SOP Classes in Study, Number of Study Related Series, Number of Study Related Instances, Institution Name |
| Series | Series Instance UID, Modality, Series Number, Series Description, Series Date, Series Time, Body Part Examined, Station Name, Performed Procedure Step Start Date, Number of Series Related Instances |
| Image | SOP Instance UID, SOP Class UID, Instance Number, Content Date, Content Time, Number of Frames |

Further attributes can be indexed with the `archive.extra_tags` setting. Attributes of levels below the query level and private attributes are ignored as keys.

*Matching.*

| Key value | Matching |
| --- | --- |
| Empty, or only `*` | Universal matching |
| A single value | Single value matching; dates and times by meaning, IS and DS numerically; Person Names case-insensitively, other values case-sensitively |
| `*` and `?` (AE, CS, LO, LT, PN, SH, ST, UC, UR, UT) | Wild card matching |
| `-` (DA, TM, DT) | Range matching, open-ended ranges included |
| Several UIDs separated by `\` | List of UID matching |
| Several values separated by `\` (other VRs) | Matches if any value matches (not defined by the Standard) |
| A sequence with one item | Sequence matching: an item of the stored sequence must match all keys of the item |

Leading and trailing spaces are not significant. Empty value matching, multiple value matching, fuzzy matching and timezone adjustment are not applied.

*Responses.* Each match is returned with status FF00 (Pending) and contains the Query/Retrieve Level, every requested key — with its value, or zero-length if the match has none — and the unique key of the level, even if not requested. Retrieve AE Title is returned, as the called AE title, when requested. Instance Availability is not returned. Specific Character Set is `ISO_IR 192` when a response contains non-ASCII characters and absent otherwise.

| Status | Code | Meaning |
| --- | --- | --- |
| Success | 0000 | All matches have been sent. |
| Pending | FF00 | A match. |
| Cancel | FE00 | The query was cancelled by C-CANCEL. |
| Refused | 0124 | The caller lacks the `query` permission. |
| Failed | A900 | The identifier cannot be decoded, the level is missing or not supported by the information model, or a key value is invalid. |
| Failed | C000 | The query could not be performed. |

##### Query/Retrieve SCP: C-MOVE and C-GET

*Permissions.* Both need the `retrieve` permission on the node; C-MOVE also needs `send`. Moving to another node of the same medicus needs `store` on that node.

*Levels.* As for C-FIND. Instances are selected by the unique key of the query level only, which may be a list of UIDs separated by `\`; other keys are ignored. A PATIENT-level request selects all studies of the patient.

*Move destination.* The move destination is resolved first among the AE titles of medicus's own nodes — the instances are then copied into that node's partition — and otherwise among the configured peers. An unknown destination is refused with status A801.

*Sub-operations.* For C-MOVE, instances are sent as described for the Storage SCU. For C-GET, each instance is sent over the requesting association on an accepted presentation context with the instance's SOP class and stored transfer syntax, for which the requestor took the Storage SCP role; an instance without such a context fails with status 0122.

A pending response (FF00) with the numbers of remaining, completed, failed and warning sub-operations follows each sub-operation except the last. The final response carries the final counts and, if any sub-operation failed, an identifier with Failed SOP Instance UID List (0008,0058).

| Status | Code | Meaning |
| --- | --- | --- |
| Success | 0000 | All sub-operations completed without failures or warnings (also when nothing matched). |
| Warning | B000 | One or more sub-operations failed or completed with a warning. |
| Pending | FF00 | Sub-operations are continuing. |
| Cancel | FE00 | Cancelled by C-CANCEL; the sub-operation in progress is completed first. |
| Refused | 0124 | The caller lacks the required permission. |
| Refused | A801 | Move destination unknown. |
| Refused | A702 | The move destination cannot be reached. |
| Failed | A900 | The identifier cannot be decoded, or the level or unique key is missing or invalid. |

#### Presentation context acceptance

Each proposed presentation context is accepted if its abstract syntax is a supported SOP class and one of its transfer syntaxes is accepted; the first accepted transfer syntax in the proposed order is chosen. Otherwise it is rejected with "abstract-syntax-not-supported" or "transfer-syntaxes-not-supported".

By default every transfer syntax whose data sets medicus can read is accepted, for every SOP class — compressed pixel data are stored as received without being decoded. The `transfer_syntaxes` setting restricts the list. Transfer syntaxes unknown to medicus are never accepted, as the header must be parsed.

<!-- generated:transfer-syntaxes -->

| Transfer Syntax | UID | Pixel Data |
| --- | --- | --- |
| Implicit VR Little Endian | `1.2.840.10008.1.2` | Native |
| Explicit VR Little Endian | `1.2.840.10008.1.2.1` | Native |
| Encapsulated Uncompressed Explicit VR Little Endian | `1.2.840.10008.1.2.1.98` | Encapsulated |
| Deflated Explicit VR Little Endian | `1.2.840.10008.1.2.1.99` | Native |
| Explicit VR Big Endian | `1.2.840.10008.1.2.2` | Native |
| JPEG Baseline (Process 1) | `1.2.840.10008.1.2.4.50` | Encapsulated |
| JPEG Extended (Process 2 & 4) | `1.2.840.10008.1.2.4.51` | Encapsulated |
| JPEG Lossless, Non-Hierarchical (Process 14) | `1.2.840.10008.1.2.4.57` | Encapsulated |
| JPEG Lossless, Non-Hierarchical, First-Order Prediction | `1.2.840.10008.1.2.4.70` | Encapsulated |
| JPEG-LS Lossless Image Compression | `1.2.840.10008.1.2.4.80` | Encapsulated |
| JPEG-LS Lossy (Near-Lossless) Image Compression | `1.2.840.10008.1.2.4.81` | Encapsulated |
| JPEG 2000 Image Compression (Lossless Only) | `1.2.840.10008.1.2.4.90` | Encapsulated |
| JPEG 2000 Image Compression | `1.2.840.10008.1.2.4.91` | Encapsulated |
| JPEG 2000 Part 2 Multi-component Image Compression (Lossless Only) | `1.2.840.10008.1.2.4.92` | Encapsulated |
| JPEG 2000 Part 2 Multi-component Image Compression | `1.2.840.10008.1.2.4.93` | Encapsulated |
| JPIP Referenced | `1.2.840.10008.1.2.4.94` | Encapsulated |
| JPIP Referenced Deflate | `1.2.840.10008.1.2.4.95` | Native |
| MPEG2 Main Profile / Main Level | `1.2.840.10008.1.2.4.100` | Encapsulated |
| Fragmentable MPEG2 Main Profile / Main Level | `1.2.840.10008.1.2.4.100.1` | Encapsulated |
| MPEG2 Main Profile / High Level | `1.2.840.10008.1.2.4.101` | Encapsulated |
| Fragmentable MPEG2 Main Profile / High Level | `1.2.840.10008.1.2.4.101.1` | Encapsulated |
| MPEG-4 AVC/H.264 High Profile / Level 4.1 | `1.2.840.10008.1.2.4.102` | Encapsulated |
| Fragmentable MPEG-4 AVC/H.264 High Profile / Level 4.1 | `1.2.840.10008.1.2.4.102.1` | Encapsulated |
| MPEG-4 AVC/H.264 BD-Compatible High Profile / Level 4.1 | `1.2.840.10008.1.2.4.103` | Encapsulated |
| Fragmentable MPEG-4 AVC/H.264 BD-Compatible High Profile / Level 4.1 | `1.2.840.10008.1.2.4.103.1` | Encapsulated |
| MPEG-4 AVC/H.264 High Profile / Level 4.2 For 2D Video | `1.2.840.10008.1.2.4.104` | Encapsulated |
| Fragmentable MPEG-4 AVC/H.264 High Profile / Level 4.2 For 2D Video | `1.2.840.10008.1.2.4.104.1` | Encapsulated |
| MPEG-4 AVC/H.264 High Profile / Level 4.2 For 3D Video | `1.2.840.10008.1.2.4.105` | Encapsulated |
| Fragmentable MPEG-4 AVC/H.264 High Profile / Level 4.2 For 3D Video | `1.2.840.10008.1.2.4.105.1` | Encapsulated |
| MPEG-4 AVC/H.264 Stereo High Profile / Level 4.2 | `1.2.840.10008.1.2.4.106` | Encapsulated |
| Fragmentable MPEG-4 AVC/H.264 Stereo High Profile / Level 4.2 | `1.2.840.10008.1.2.4.106.1` | Encapsulated |
| HEVC/H.265 Main Profile / Level 5.1 | `1.2.840.10008.1.2.4.107` | Encapsulated |
| HEVC/H.265 Main 10 Profile / Level 5.1 | `1.2.840.10008.1.2.4.108` | Encapsulated |
| JPEG XL Lossless | `1.2.840.10008.1.2.4.110` | Encapsulated |
| JPEG XL Recompression | `1.2.840.10008.1.2.4.111` | Encapsulated |
| JPEG XL | `1.2.840.10008.1.2.4.112` | Encapsulated |
| High-Throughput JPEG 2000 Image Compression (Lossless Only) | `1.2.840.10008.1.2.4.201` | Encapsulated |
| High-Throughput JPEG 2000 with RPCL Options Image Compression (Lossless Only) | `1.2.840.10008.1.2.4.202` | Encapsulated |
| High-Throughput JPEG 2000 Image Compression | `1.2.840.10008.1.2.4.203` | Encapsulated |
| JPIP HTJ2K Referenced | `1.2.840.10008.1.2.4.204` | Encapsulated |
| JPIP HT2JK Referenced Deflate | `1.2.840.10008.1.2.4.205` | Native |
| RLE Lossless | `1.2.840.10008.1.2.5` | Encapsulated |
| SMPTE ST 2110-20 Uncompressed Progressive Active Video | `1.2.840.10008.1.2.7.1` | Encapsulated |
| SMPTE ST 2110-20 Uncompressed Interlaced Active Video | `1.2.840.10008.1.2.7.2` | Encapsulated |
| SMPTE ST 2110-30 PCM Digital Audio | `1.2.840.10008.1.2.7.3` | Encapsulated |
| Deflated Image Frame Compression | `1.2.840.10008.1.2.8.1` | Encapsulated |

<!-- /generated:transfer-syntaxes -->

### Network interfaces

medicus uses the TCP/IP stack of the operating system. Both IPv4 and IPv6 are supported; the listening addresses are configured. The DIMSE listener uses port 11112 by default, the DICOMweb listener port 8080. A node may name its own DIMSE port; its AE title is then accepted on that port only.

### Configuration

AE titles, DICOMweb paths, peers and access control are set in the server's configuration file.

| Setting | Default | Meaning |
| --- | --- | --- |
| `[nodes.NAME] dimse.ae_title` | `MEDICUS` (node `MEDICUS`, if no nodes are configured) | The node's AE title; unique across nodes. |
| `[nodes.NAME] dimse.port` | the port of `[addons.dimse] listen` | The port on which the node's AE title is accepted, and the only one. |
| `[nodes.NAME] dicomweb.path` | `medicus` | The node's DICOMweb path, served at `[addons.dicomweb] node_root`. |
| `[peers.NAME]` | – | A remote AE: AE title, host and port. |
| `[addons.dimse] listen` | `0.0.0.0:11112` | DIMSE listening address; nodes with their own port are served on it at the same address. |
| `[addons.dimse] max_pdu_length` | 65536 | Maximum PDU size received. |
| `[addons.dimse] max_associations` | 64 | Simultaneous associations, per port. |
| `[addons.dimse] idle_timeout_secs` | 300 | Abort silent associations after this time; 0 disables. |
| `[addons.dimse] extra_sop_classes` | none | Additional Storage SOP classes. |
| `[addons.dimse] transfer_syntaxes` | all readable | Accepted transfer syntaxes. |
| `[addons.dicomweb] listen` | `0.0.0.0:8080` | DICOMweb listening address. |
| `[addons.dicomweb] node_root` | `/nodes/{node}/rs` | A node's service root; `{node}` is its DICOMweb path. |
| `[addons.dicomweb] default_root` | `/rs` | The default node's service root. |
| `[archive] duplicate_policy` | `overwrite` | See Storage SCP. |

## DICOMweb

### Service roots

Every node with a DICOMweb path is served at its service root, by default `/nodes/{path}/rs`. One node — the configured `default_node`, or the only node if there is just one — is also served at the default root, by default `/rs`. Both roots are configurable (`node_root`, `default_root`), e.g. `/dicom-web/{node}` and `/dicom-web`. All resources below are relative to a service root.

### Search (QIDO-RS)

| Resource | Method |
| --- | --- |
| `/studies` | GET |
| `/series`, `/studies/{study}/series` | GET |
| `/instances`, `/studies/{study}/instances`, `/studies/{study}/series/{series}/instances` | GET |

Responses are `application/dicom+json`; an empty result is `200` with `[]`. XML responses are not supported.

| Query parameter | Support |
| --- | --- |
| `{attribute}={value}` | Attribute by keyword or tag; matching as for C-FIND. UID lists are comma-separated. Sequence attributes (dotted paths) are not supported (400). Unknown attributes give 400. |
| `includefield` | Keywords or tags, comma-separated or repeated; `all` returns every indexed attribute of the level and above. |
| `limit`, `offset` | Supported; `limit` is capped at 10000. |
| `fuzzymatching` | Accepted and ignored. |
| `orderby` | Not supported. |

Each result contains the default attributes of PS3.18 for its level, the unique keys of its level and above, the requested attributes that have a value, and a Retrieve URL (0008,1190). When further results exist beyond `limit`, the response carries `Warning: 299 medicus: There are additional results that can be requested`.

### Retrieve (WADO-RS)

| Resource | Response |
| --- | --- |
| `/studies/{study}`, `…/series/{series}`, `…/instances/{instance}` | `multipart/related; type="application/dicom"`, each part a stored Part 10 file |
| `…/metadata` (study, series, instance) | `application/dicom+json`: all attributes before Pixel Data, as stored |
| `…/instances/{instance}/frames/{list}` | `multipart/related`, the frames as stored |
| `…/rendered` (study, series, instance, single frame) | `image/jpeg`, `image/png` or `image/gif` |
| `…/thumbnail` (study, series, instance, single frame) | As `rendered` |

*Transfer syntaxes.* Instances and frames are returned in their stored transfer syntax; a request for any other (for example `transfer-syntax=1.2.840.10008.1.2.1` when the instance is stored compressed) is answered with 406. `transfer-syntax=*` accepts the stored one. Frames have the media type of their stored encoding (`image/jpeg`, `image/jls`, `image/jp2`, `image/jpx`, `image/jphc`, `image/x-dicom-rle`, `video/mpeg`), or `application/octet-stream` for native pixel data.

*Rendered resources.* The media type is taken from the `accept` query parameter or the Accept header; `image/jpeg` is the default. A study or series renders its first instance with pixel data, an instance its first frame; a frames request must name exactly one frame.

| Query parameter | Support |
| --- | --- |
| `quality` | 1–100, JPEG only; default 90. |
| `window` | `center,width[,function]` with function `linear` (default), `linear-exact` or `sigmoid`; otherwise the instance's own VOI LUT or window is applied. |
| `viewport` | `vw,vh[,sx,sy,sw,sh]`: crops, flips for negative `sw` or `sh`, scales preserving the aspect ratio and centres on a black canvas of `vw` × `vh`. |
| `annotation` | Not supported; the image is returned with `Warning: 299`. |
| `iccprofile` | Ignored; no ICC profile is embedded. |

Bulk data and pixel data resources, rendered multi-frame or video output and transcoding are not supported.

### Store (STOW-RS)

| Resource | Method |
| --- | --- |
| `/studies`, `/studies/{study}` | POST |

The request body must be `multipart/related` with `application/dicom` parts (415 otherwise); metadata with bulk data (`application/dicom+json`) is not supported. Parts are stored as for the Storage SCP.

The response is `application/dicom+json` with a Referenced SOP Sequence (each item with Retrieve URL) and a Failed SOP Sequence. The HTTP status is 200 when all instances were stored, 202 when some failed and 409 when none were stored.

| Failure Reason | Meaning |
| --- | --- |
| A700 | Out of resources or storage unavailable. |
| 0124 | The caller lacks the `store` permission. |
| A900 | The instance belongs to another study than the request path names (it has been stored nonetheless). |
| C000 | Any other failure, including refused duplicates and unparsable data. |

### Errors and cross-origin access

| HTTP status | Meaning |
| --- | --- |
| 400 | Invalid request (parameters, UIDs, frame numbers). |
| 403 | Permission denied. |
| 404 | No such node or resource; no pixel data or no such frame. |
| 406 | The requested media type or transfer syntax cannot be provided. |
| 409 | STOW-RS: none of the instances could be stored. |
| 413 | Instance too large to extract frames from or to render (2 GiB). |
| 415 | Unsupported request media type (STOW-RS). |
| 422 | The pixel data cannot be decoded. |
| 501, 503, 507 | Not supported, temporarily unavailable, storage full. |

Error responses carry a JSON body `{"message": "…"}`. Cross-origin requests from browsers (CORS) are allowed from the origins configured in `cors_origins`, none by default.

## Media interchange

Not supported. medicus does not read or write DICOM media; DICOMDIR files are skipped when importing files from a directory.

## Support of character sets

All character sets of PS3.3 C.12.1.1.2 that dicom-rs decodes are supported on input: received values are decoded to Unicode for indexing and matching, while the stored files keep their original encoding.

C-FIND responses are encoded in UTF-8 (`ISO_IR 192`) when they contain non-ASCII characters, and in the default repertoire otherwise. DICOMweb responses are UTF-8 JSON. WADO-RS returns stored files and metadata with the Specific Character Set they were received with.

Person names are matched case-insensitively, by Unicode lowercase folding.

## Security

### Access control

Callers are identified per protocol: over DIMSE by calling AE title and, optionally, the network they connect from; over DICOMweb by a user name supplied by a trusted reverse proxy in a configured header (`user_header`), or anonymously.

Permissions are granted per node through roles:

| Permission | Allows |
| --- | --- |
| `query` | C-FIND, QIDO-RS |
| `retrieve` | C-GET, C-MOVE (as source), WADO-RS |
| `store` | C-STORE, STOW-RS |
| `send` | C-MOVE, send jobs |

Anonymous callers receive the roles in `access.anonymous_roles`. If no roles and principals are configured at all, access is open — every caller may do everything — and the server warns at start-up. Every refused operation is logged and recorded as an `access.denied` event.

### Transport security

TLS is not supported by the DIMSE and DICOMweb listeners. Deploy medicus in a trusted network, or terminate TLS in front of it (a reverse proxy for DICOMweb, a TLS tunnel for DIMSE).

### Audit

medicus records stored, rejected and refused operations, jobs and storage conditions as events, available through its administrative API and to addons. ATNA audit logging (PS3.15 Annex A.5) is not supported.

<!--raw-typst #set heading(numbering: none) -->

## Annex A: Storage SOP classes

The Storage SOP classes accepted as SCP by default. Retired SOP classes are accepted for older equipment.

<!-- generated:storage-sop-classes -->

| SOP Class | SOP Class UID | Status |
| --- | --- | --- |
| Computed Radiography Image Storage | `1.2.840.10008.5.1.4.1.1.1` |  |
| Digital X-Ray Image Storage - For Presentation | `1.2.840.10008.5.1.4.1.1.1.1` |  |
| Digital X-Ray Image Storage - For Processing | `1.2.840.10008.5.1.4.1.1.1.1.1` |  |
| Digital Mammography X-Ray Image Storage - For Presentation | `1.2.840.10008.5.1.4.1.1.1.2` |  |
| Digital Mammography X-Ray Image Storage - For Processing | `1.2.840.10008.5.1.4.1.1.1.2.1` |  |
| Digital Intra-Oral X-Ray Image Storage - For Presentation | `1.2.840.10008.5.1.4.1.1.1.3` |  |
| Digital Intra-Oral X-Ray Image Storage - For Processing | `1.2.840.10008.5.1.4.1.1.1.3.1` |  |
| CT Image Storage | `1.2.840.10008.5.1.4.1.1.2` |  |
| CT Image Storage - For Processing | `1.2.840.10008.5.1.4.1.1.2.3` |  |
| Enhanced CT Image Storage | `1.2.840.10008.5.1.4.1.1.2.1` |  |
| Enhanced CT Image Storage - For Processing | `1.2.840.10008.5.1.4.1.1.2.4` |  |
| Legacy Converted Enhanced CT Image Storage | `1.2.840.10008.5.1.4.1.1.2.2` |  |
| Legacy Converted Enhanced CT Image Storage - For Processing | `1.2.840.10008.5.1.4.1.1.2.5` |  |
| Ultrasound Multi-frame Image Storage | `1.2.840.10008.5.1.4.1.1.3.1` |  |
| MR Image Storage | `1.2.840.10008.5.1.4.1.1.4` |  |
| Enhanced MR Image Storage | `1.2.840.10008.5.1.4.1.1.4.1` |  |
| MR Spectroscopy Storage | `1.2.840.10008.5.1.4.1.1.4.2` |  |
| Enhanced MR Color Image Storage | `1.2.840.10008.5.1.4.1.1.4.3` |  |
| Legacy Converted Enhanced MR Image Storage | `1.2.840.10008.5.1.4.1.1.4.4` |  |
| Ultrasound Image Storage | `1.2.840.10008.5.1.4.1.1.6.1` |  |
| Enhanced US Volume Storage | `1.2.840.10008.5.1.4.1.1.6.2` |  |
| Photoacoustic Image Storage | `1.2.840.10008.5.1.4.1.1.6.3` |  |
| Secondary Capture Image Storage | `1.2.840.10008.5.1.4.1.1.7` |  |
| Multi-frame Single Bit Secondary Capture Image Storage | `1.2.840.10008.5.1.4.1.1.7.1` |  |
| Multi-frame Grayscale Byte Secondary Capture Image Storage | `1.2.840.10008.5.1.4.1.1.7.2` |  |
| Multi-frame Grayscale Word Secondary Capture Image Storage | `1.2.840.10008.5.1.4.1.1.7.3` |  |
| Multi-frame True Color Secondary Capture Image Storage | `1.2.840.10008.5.1.4.1.1.7.4` |  |
| 12-lead ECG Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.1.1` |  |
| General ECG Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.1.2` |  |
| Ambulatory ECG Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.1.3` |  |
| General 32-bit ECG Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.1.4` |  |
| Hemodynamic Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.2.1` |  |
| Cardiac Electrophysiology Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.3.1` |  |
| Basic Voice Audio Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.4.1` |  |
| General Audio Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.4.2` |  |
| Arterial Pulse Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.5.1` |  |
| Respiratory Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.6.1` |  |
| Multi-channel Respiratory Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.6.2` |  |
| Routine Scalp Electroencephalogram Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.7.1` |  |
| Electromyogram Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.7.2` |  |
| Electrooculogram Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.7.3` |  |
| Sleep Electroencephalogram Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.7.4` |  |
| Body Position Waveform Storage | `1.2.840.10008.5.1.4.1.1.9.8.1` |  |
| Waveform Presentation State Storage | `1.2.840.10008.5.1.4.1.1.9.100.1` |  |
| Waveform Acquisition Presentation State Storage | `1.2.840.10008.5.1.4.1.1.9.100.2` |  |
| Grayscale Softcopy Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.1` |  |
| Color Softcopy Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.2` |  |
| Pseudo-Color Softcopy Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.3` |  |
| Blending Softcopy Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.4` |  |
| XA/XRF Grayscale Softcopy Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.5` |  |
| Grayscale Planar MPR Volumetric Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.6` |  |
| Compositing Planar MPR Volumetric Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.7` |  |
| Advanced Blending Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.8` |  |
| Volume Rendering Volumetric Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.9` |  |
| Segmented Volume Rendering Volumetric Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.10` |  |
| Multiple Volume Rendering Volumetric Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.11` |  |
| Variable Modality LUT Softcopy Presentation State Storage | `1.2.840.10008.5.1.4.1.1.11.12` |  |
| X-Ray Angiographic Image Storage | `1.2.840.10008.5.1.4.1.1.12.1` |  |
| Enhanced XA Image Storage | `1.2.840.10008.5.1.4.1.1.12.1.1` |  |
| X-Ray Radiofluoroscopic Image Storage | `1.2.840.10008.5.1.4.1.1.12.2` |  |
| Enhanced XRF Image Storage | `1.2.840.10008.5.1.4.1.1.12.2.1` |  |
| X-Ray 3D Angiographic Image Storage | `1.2.840.10008.5.1.4.1.1.13.1.1` |  |
| X-Ray 3D Craniofacial Image Storage | `1.2.840.10008.5.1.4.1.1.13.1.2` |  |
| Breast Tomosynthesis Image Storage | `1.2.840.10008.5.1.4.1.1.13.1.3` |  |
| Breast Projection X-Ray Image Storage - For Presentation | `1.2.840.10008.5.1.4.1.1.13.1.4` |  |
| Breast Projection X-Ray Image Storage - For Processing | `1.2.840.10008.5.1.4.1.1.13.1.5` |  |
| Intravascular Optical Coherence Tomography Image Storage - For Presentation | `1.2.840.10008.5.1.4.1.1.14.1` |  |
| Intravascular Optical Coherence Tomography Image Storage - For Processing | `1.2.840.10008.5.1.4.1.1.14.2` |  |
| Nuclear Medicine Image Storage | `1.2.840.10008.5.1.4.1.1.20` |  |
| Parametric Map Storage | `1.2.840.10008.5.1.4.1.1.30` |  |
| Raw Data Storage | `1.2.840.10008.5.1.4.1.1.66` |  |
| Spatial Registration Storage | `1.2.840.10008.5.1.4.1.1.66.1` |  |
| Spatial Fiducials Storage | `1.2.840.10008.5.1.4.1.1.66.2` |  |
| Deformable Spatial Registration Storage | `1.2.840.10008.5.1.4.1.1.66.3` |  |
| Segmentation Storage | `1.2.840.10008.5.1.4.1.1.66.4` |  |
| Surface Segmentation Storage | `1.2.840.10008.5.1.4.1.1.66.5` |  |
| Tractography Results Storage | `1.2.840.10008.5.1.4.1.1.66.6` |  |
| Label Map Segmentation Storage | `1.2.840.10008.5.1.4.1.1.66.7` |  |
| Height Map Segmentation Storage | `1.2.840.10008.5.1.4.1.1.66.8` |  |
| Real World Value Mapping Storage | `1.2.840.10008.5.1.4.1.1.67` |  |
| Surface Scan Mesh Storage | `1.2.840.10008.5.1.4.1.1.68.1` |  |
| Surface Scan Point Cloud Storage | `1.2.840.10008.5.1.4.1.1.68.2` |  |
| VL Endoscopic Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.1` |  |
| Video Endoscopic Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.1.1` |  |
| VL Microscopic Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.2` |  |
| Video Microscopic Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.2.1` |  |
| VL Slide-Coordinates Microscopic Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.3` |  |
| VL Photographic Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.4` |  |
| Video Photographic Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.4.1` |  |
| Ophthalmic Photography 8 Bit Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.5.1` |  |
| Ophthalmic Photography 16 Bit Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.5.2` |  |
| Stereometric Relationship Storage | `1.2.840.10008.5.1.4.1.1.77.1.5.3` |  |
| Ophthalmic Tomography Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.5.4` |  |
| Wide Field Ophthalmic Photography Stereographic Projection Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.5.5` |  |
| Wide Field Ophthalmic Photography 3D Coordinates Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.5.6` |  |
| Ophthalmic Optical Coherence Tomography En Face Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.5.7` |  |
| Ophthalmic Optical Coherence Tomography B-scan Volume Analysis Storage | `1.2.840.10008.5.1.4.1.1.77.1.5.8` |  |
| VL Whole Slide Microscopy Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.6` |  |
| Dermoscopic Photography Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.7` |  |
| Confocal Microscopy Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.8` |  |
| Confocal Microscopy Tiled Pyramidal Image Storage | `1.2.840.10008.5.1.4.1.1.77.1.9` |  |
| Lensometry Measurements Storage | `1.2.840.10008.5.1.4.1.1.78.1` |  |
| Autorefraction Measurements Storage | `1.2.840.10008.5.1.4.1.1.78.2` |  |
| Keratometry Measurements Storage | `1.2.840.10008.5.1.4.1.1.78.3` |  |
| Subjective Refraction Measurements Storage | `1.2.840.10008.5.1.4.1.1.78.4` |  |
| Visual Acuity Measurements Storage | `1.2.840.10008.5.1.4.1.1.78.5` |  |
| Spectacle Prescription Report Storage | `1.2.840.10008.5.1.4.1.1.78.6` |  |
| Ophthalmic Axial Measurements Storage | `1.2.840.10008.5.1.4.1.1.78.7` |  |
| Intraocular Lens Calculations Storage | `1.2.840.10008.5.1.4.1.1.78.8` |  |
| Macular Grid Thickness and Volume Report Storage | `1.2.840.10008.5.1.4.1.1.79.1` |  |
| Ophthalmic Visual Field Static Perimetry Measurements Storage | `1.2.840.10008.5.1.4.1.1.80.1` |  |
| Ophthalmic Thickness Map Storage | `1.2.840.10008.5.1.4.1.1.81.1` |  |
| Corneal Topography Map Storage | `1.2.840.10008.5.1.4.1.1.82.1` |  |
| Basic Text SR Storage | `1.2.840.10008.5.1.4.1.1.88.11` |  |
| Enhanced SR Storage | `1.2.840.10008.5.1.4.1.1.88.22` |  |
| Comprehensive SR Storage | `1.2.840.10008.5.1.4.1.1.88.33` |  |
| Comprehensive 3D SR Storage | `1.2.840.10008.5.1.4.1.1.88.34` |  |
| Extensible SR Storage | `1.2.840.10008.5.1.4.1.1.88.35` |  |
| Procedure Log Storage | `1.2.840.10008.5.1.4.1.1.88.40` |  |
| Mammography CAD SR Storage | `1.2.840.10008.5.1.4.1.1.88.50` |  |
| Key Object Selection Document Storage | `1.2.840.10008.5.1.4.1.1.88.59` |  |
| Chest CAD SR Storage | `1.2.840.10008.5.1.4.1.1.88.65` |  |
| X-Ray Radiation Dose SR Storage | `1.2.840.10008.5.1.4.1.1.88.67` |  |
| Radiopharmaceutical Radiation Dose SR Storage | `1.2.840.10008.5.1.4.1.1.88.68` |  |
| Colon CAD SR Storage | `1.2.840.10008.5.1.4.1.1.88.69` |  |
| Implantation Plan SR Storage | `1.2.840.10008.5.1.4.1.1.88.70` |  |
| Acquisition Context SR Storage | `1.2.840.10008.5.1.4.1.1.88.71` |  |
| Simplified Adult Echo SR Storage | `1.2.840.10008.5.1.4.1.1.88.72` |  |
| Patient Radiation Dose SR Storage | `1.2.840.10008.5.1.4.1.1.88.73` |  |
| Planned Imaging Agent Administration SR Storage | `1.2.840.10008.5.1.4.1.1.88.74` |  |
| Performed Imaging Agent Administration SR Storage | `1.2.840.10008.5.1.4.1.1.88.75` |  |
| Enhanced X-Ray Radiation Dose SR Storage | `1.2.840.10008.5.1.4.1.1.88.76` |  |
| Waveform Annotation SR Storage | `1.2.840.10008.5.1.4.1.1.88.77` |  |
| Content Assessment Results Storage | `1.2.840.10008.5.1.4.1.1.90.1` |  |
| Microscopy Bulk Simple Annotations Storage | `1.2.840.10008.5.1.4.1.1.91.1` |  |
| Encapsulated PDF Storage | `1.2.840.10008.5.1.4.1.1.104.1` |  |
| Encapsulated CDA Storage | `1.2.840.10008.5.1.4.1.1.104.2` |  |
| Encapsulated STL Storage | `1.2.840.10008.5.1.4.1.1.104.3` |  |
| Encapsulated OBJ Storage | `1.2.840.10008.5.1.4.1.1.104.4` |  |
| Encapsulated MTL Storage | `1.2.840.10008.5.1.4.1.1.104.5` |  |
| Positron Emission Tomography Image Storage | `1.2.840.10008.5.1.4.1.1.128` |  |
| Enhanced PET Image Storage | `1.2.840.10008.5.1.4.1.1.130` |  |
| Legacy Converted Enhanced PET Image Storage | `1.2.840.10008.5.1.4.1.1.128.1` |  |
| Basic Structured Display Storage | `1.2.840.10008.5.1.4.1.1.131` |  |
| CT Performed Procedure Protocol Storage | `1.2.840.10008.5.1.4.1.1.200.2` |  |
| XA Performed Procedure Protocol Storage | `1.2.840.10008.5.1.4.1.1.200.8` |  |
| RT Image Storage | `1.2.840.10008.5.1.4.1.1.481.1` |  |
| RT Dose Storage | `1.2.840.10008.5.1.4.1.1.481.2` |  |
| RT Structure Set Storage | `1.2.840.10008.5.1.4.1.1.481.3` |  |
| RT Beams Treatment Record Storage | `1.2.840.10008.5.1.4.1.1.481.4` |  |
| RT Plan Storage | `1.2.840.10008.5.1.4.1.1.481.5` |  |
| RT Brachy Treatment Record Storage | `1.2.840.10008.5.1.4.1.1.481.6` |  |
| RT Treatment Summary Record Storage | `1.2.840.10008.5.1.4.1.1.481.7` |  |
| RT Ion Plan Storage | `1.2.840.10008.5.1.4.1.1.481.8` |  |
| RT Ion Beams Treatment Record Storage | `1.2.840.10008.5.1.4.1.1.481.9` |  |
| RT Physician Intent Storage | `1.2.840.10008.5.1.4.1.1.481.10` |  |
| RT Segment Annotation Storage | `1.2.840.10008.5.1.4.1.1.481.11` |  |
| RT Radiation Set Storage | `1.2.840.10008.5.1.4.1.1.481.12` |  |
| C-Arm Photon-Electron Radiation Storage | `1.2.840.10008.5.1.4.1.1.481.13` |  |
| Tomotherapeutic Radiation Storage | `1.2.840.10008.5.1.4.1.1.481.14` |  |
| Robotic-Arm Radiation Storage | `1.2.840.10008.5.1.4.1.1.481.15` |  |
| RT Radiation Record Set Storage | `1.2.840.10008.5.1.4.1.1.481.16` |  |
| RT Radiation Salvage Record Storage | `1.2.840.10008.5.1.4.1.1.481.17` |  |
| Tomotherapeutic Radiation Record Storage | `1.2.840.10008.5.1.4.1.1.481.18` |  |
| C-Arm Photon-Electron Radiation Record Storage | `1.2.840.10008.5.1.4.1.1.481.19` |  |
| Robotic Radiation Record Storage | `1.2.840.10008.5.1.4.1.1.481.20` |  |
| RT Radiation Set Delivery Instruction Storage | `1.2.840.10008.5.1.4.1.1.481.21` |  |
| RT Treatment Preparation Storage | `1.2.840.10008.5.1.4.1.1.481.22` |  |
| Enhanced RT Image Storage | `1.2.840.10008.5.1.4.1.1.481.23` |  |
| Enhanced Continuous RT Image Storage | `1.2.840.10008.5.1.4.1.1.481.24` |  |
| RT Patient Position Acquisition Instruction Storage | `1.2.840.10008.5.1.4.1.1.481.25` |  |
| RT Beams Delivery Instruction Storage | `1.2.840.10008.5.1.4.34.7` |  |
| RT Brachy Application Setup Delivery Instruction Storage | `1.2.840.10008.5.1.4.34.10` |  |
| Standalone Modality LUT Storage (Retired) | `1.2.840.10008.5.1.4.1.1.10` | Retired |
| Standalone VOI LUT Storage (Retired) | `1.2.840.10008.5.1.4.1.1.11` | Retired |
| X-Ray Angiographic Bi-Plane Image Storage (Retired) | `1.2.840.10008.5.1.4.1.1.12.3` | Retired |
| Standalone PET Curve Storage (Retired) | `1.2.840.10008.5.1.4.1.1.129` | Retired |
| Ultrasound Multi-frame Image Storage (Retired) | `1.2.840.10008.5.1.4.1.1.3` | Retired |
| Nuclear Medicine Image Storage (Retired) | `1.2.840.10008.5.1.4.1.1.5` | Retired |
| Ultrasound Image Storage (Retired) | `1.2.840.10008.5.1.4.1.1.6` | Retired |
| VL Image Storage - Trial (Retired) | `1.2.840.10008.5.1.4.1.1.77.1` | Retired |
| VL Multi-frame Image Storage - Trial (Retired) | `1.2.840.10008.5.1.4.1.1.77.2` | Retired |
| Standalone Overlay Storage (Retired) | `1.2.840.10008.5.1.4.1.1.8` | Retired |
| Text SR Storage - Trial (Retired) | `1.2.840.10008.5.1.4.1.1.88.1` | Retired |
| Audio SR Storage - Trial (Retired) | `1.2.840.10008.5.1.4.1.1.88.2` | Retired |
| Detail SR Storage - Trial (Retired) | `1.2.840.10008.5.1.4.1.1.88.3` | Retired |
| Comprehensive SR Storage - Trial (Retired) | `1.2.840.10008.5.1.4.1.1.88.4` | Retired |
| Standalone Curve Storage (Retired) | `1.2.840.10008.5.1.4.1.1.9` | Retired |
| Waveform Storage - Trial (Retired) | `1.2.840.10008.5.1.4.1.1.9.1` | Retired |

<!-- /generated:storage-sop-classes -->

## Annex B: IOD contents and private attributes

medicus creates no SOP instances of its own and stores received instances unchanged, so no IOD contents are specified. It defines no private attributes and retains those it receives.
