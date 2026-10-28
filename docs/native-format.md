# Native JSONL document formats, versions 2 and 3

The shared `document` module reads completed files and writes fixed snapshots for
the GUI and other headless consumers. CLI v1 output remains unchanged. A native
file contains a `document` header, zero or more `entry` / `event` / `unparsed`
records, and exactly one final `artifact_end` record.

V2 remains the default. V3 is an explicit compact encoding in the shared codec;
CLI writing is not enabled by this codec change. Its header, every data record,
and footer must all say `schema_version: 3`. A reader never guesses compact
content inside a v2 envelope. Frontends must support opening/querying v3 before
exposing its writing option.

**A finalized file is not a complete-capture claim.** A snapshot may be finalized
while the source is still running, after retention removed older rows, or after
an interruption. Capture state, export scope, and artifact finalization are
separate fields.

## Public API

```rust,ignore
load(reader: impl BufRead, memory_budget: usize, cancel: &AtomicBool)
    -> Result<Document, DocumentError>

Document::from_snapshot(
    header: DocumentHeader,
    entries: Arc<LogStore>,
    scope: ExportScope,
    capture: CaptureStatus,
) -> Result<Document, DocumentError>

document.select(indices: &[usize], filter: serde_json::Value)
    -> Result<Document, DocumentError>

write_snapshot(writer: impl Write, document: &Document, cancel: &AtomicBool)
    -> Result<WriteSummary, DocumentError>

header.set_format(NativeFormat::CompactV3) -> Result<(), DocumentError>

document.convert_format(
    format: NativeFormat, // V2 or CompactV3
    memory_budget: usize,
    output_budget: u64, // Complete encoded artifact, including header/footer/LFs
    cancel: &AtomicBool,
) -> Result<Document, DocumentError>
```

`Document` owns an `Arc<LogStore>` and an aligned `record_metadata` vector. The
sidecar preserves sequence information and optional extensions without another
copy of each log message. Callers must treat a published document as immutable.
Constructing a live snapshot requires an identity on every row, including
device-originated markers and unparsed records. Empty snapshots are valid.

`load` builds a candidate and returns it only after the entire file passes
validation. It never updates a live store, drops a file prefix to fit a budget,
starts ADB, or enriches old rows from current process observations. On failure or
cancellation, the caller retains its previous source.

`StreamEncoder` offers the same validation for incremental header/record/footer
encoding without retaining capture history. Clone its small state before
encoding a pending record and commit that clone only after admitting the record
to the writer; output visibility is tracked separately after complete flush.
`decode_header`, `decode_record`, `decode_end` and `validate_end_summary` support
bounded readers without duplicating the record contract.

`set_format` configures a new snapshot or stream. Existing documents use
`convert_format`, which validates the source before removing redundant fields,
allocates a new artifact ID and records `derived_from_artifact`. It shares the
immutable entries, preserves scope/capture/extensions, checks the source and
candidate retained-memory estimates and preflights the actual encoder against both line and total
output limits. A v2 export that expands beyond either output limit fails; it
never truncates payload or display text. Each estimate must fit the budget;
the budget does not cover the sum of all simultaneous caller snapshots or exact
allocator RSS. Source accounting and candidate sidecar copies check cancellation
and budget at each record. Conversion never deep-copies the entire sidecar
before its next cancellation check.

Selection and resaving retain the document version. Loaded v3 documents have
`DocumentOrigin::NativeV3`; legacy import still defaults to v2. Stream and
snapshot output use the same record encoder.

The snapshot writer validates the snapshot, writes bounded lines, and flushes the writer.
It does not choose a destination, sync a file/directory, or atomically publish a
temporary file. Those operations belong to the frontend. An I/O error or
cancellation may leave a partial temporary stream; it must not replace an
existing user file.

## Header and source context

Every native record has a consistent `schema_version` and a `type`. The header contains:

| Field | Meaning |
| --- | --- |
| `artifact_id` | Non-nil UUID identifying this artifact; filtered derivatives get a new ID. |
| `session_id` | Stable capture-session UUID, or null for a normalized legacy source. |
| `source.kind` | `adb` or `legacy_v1`; an unsupported required source kind is rejected. |
| `source.device_serial` | Required and nonempty for `adb`; unknown/null for legacy input. |
| `source.requested_buffers` | Nonempty requested list for an ADB snapshot; not actual per-record buffer provenance. |
| `source.requested_buffers_scope` | `snapshot_configuration` for a GUI snapshot, or `capture_configuration` for the fixed CLI v2 configuration. |
| `source.transport_format` | ADB source representation, `threadtime` or experimental `logcat_proto_experimental`; unknown/null for legacy. |
| `derived_from_artifact` | Parent artifact UUID for a filtered derivative, otherwise optional. |
| `normalization` | Explicit source-normalization observations, if known; absence is not a no-loss guarantee. |

The GUI may retain rows collected before the requested buffer list changed.
Therefore the current requested list must never be presented as the capture
configuration of every historical row. Individual `segment` and `attempt`
identities distinguish capture intervals. GUI capture observations now retain
the requested configuration for recent attempts, with explicit history eviction
counts; they do not reconstruct every interval of an arbitrarily long session.
Per-record actual buffer remains unknown.

## Records and stable references

All three record types have a positive, strictly increasing `seq` and optional
`provenance`. V2 and non-proto v3 records store `message`; proto v3 derives it.
Native ADB records require provenance with:

```json
{"session_id":"780fd159-ec1e-45b3-b55d-eef07774ccdc","seq":7,"segment":2,"attempt":3,"observed_unix_ms":1792740000123}
```

The provenance session must match the header, and its sequence must match the
record's outer sequence. Segment is positive, and attempt is at least segment.
`observed_unix_ms` is a host observation, not device event time, and may be null.
Clear, retention, pause, and reconnect must not reuse `(session_id, seq)`.
Sequence gaps are allowed: filtering and retention can omit records without
proving capture loss.

- `entry` requires `device_time.text`, `pid`, `tid`, `level`, `tag`, and `message`.
  Level is one of `V`, `D`, `I`, `W`, `E`, `F`. Source time text is preserved;
  missing year/timezone is not guessed. Multiline text and NUL use JSON escapes.
- `event` has a nonempty `event_kind`, currently `logcat_marker` for device
  structural markers. It does not impersonate an Info log with PID 0. Its log
  fields must be absent/null.
- `unparsed` preserves text the current log parser could not recognize. It has
  no fabricated PID, level, Tag, device time, process attribution, or event kind.

`process`, when present, contains `name` and a nullable `source`. The current
known sources are `ps_snapshot` and `device_process_cache`: sampled or device PID-cache hints, not verified process-instance or
package ownership. Unknown future source strings are preserved, while the
current UI's typed attribution stays unknown. A process name alone is not proof
of exact attribution. No later sample fills unknown historical rows.

This version does not create sequence-bearing host pause/reconnect events. Such
observations can be retained in the footer's capture status and known gaps. A
later event protocol must define ownership and ordering before adding them.

## Optional protobuf evidence in v2

Experimental protobuf records carry an optional `proto` object. Its required
fields are `time_sec`, `time_nsec`, `uid`, `pid`, `tid` (unsigned 64-bit),
`priority` (signed 32-bit numeric enum), `raw_payload`, `tag_bytes`, and
`message_bytes`, with an optional nullable `process_name`. The three byte fields use standard
padded base64, not JSON integer arrays. `raw_payload` is the exact received
protobuf payload **without** its eight-byte length prefix; unknown protobuf
fields and duplicate-field wire representations survive in these bytes. Each
byte/string field and the entire raw payload must be at most 1 MiB. Unknown
optional JSON fields inside `proto` survive load/select/save and count toward
the candidate memory budget.

Readers and writers re-decode `raw_payload` with the fixed AOSP schema and
compare every explicit evidence field. Conflicts, malformed base64/protobuf,
and oversized evidence reject the whole candidate. Display fields are checked
against normalization too: one trailing tag NUL removed, invalid UTF-8 replaced
only for display, complete message whitespace/newlines/NUL retained. A typed
entry requires a supported emitted level, representable PID/TID, and valid
nanoseconds. Its `device_time.text` is `seconds.nanoseconds` with nine decimal
digits; no host timezone conversion is implied. Its optional process source is
`device_process_cache`, with the name from the frame, never a host `ps` label.
That source requires matching protobuf evidence. Neither source establishes
process-instance identity or package ownership.

Unknown priorities (including numeric 7), Default/Silent, invalid time/ID,
empty/all-default and unknown-field-only payloads remain `unparsed` records
with evidence. Their outer PID/level/time/process fields stay absent. A zero
record stream is distinct from one empty frame; neither proves working schema
or complete capture. An accepted frame is evidence of bytes received, not of
logs that the device may have dropped or transformed earlier.

`text_loss`, if present on a proto row, describes host display conversion:
`truncated` is false because the complete admitted frame is retained, and
`invalid_utf8_replaced` follows its tag/message bytes. These flags do not claim
that device liblog preserved NUL or avoided truncation. The header can declare
`normalization.protobuf_raw_payload: "preserved_base64"`; this refers to the
protobuf representation after device conversion, not original logd bytes.

The existing 8 MiB encoded JSONL limit still applies even when base64 and JSON
escaping expand a valid 1 MiB frame beyond it; such a record fails explicitly
instead of silently truncating evidence. Memory accounting includes the raw
payload, separately decoded tag/message bytes, display text and JSON extensions.
V1 import never invents protobuf evidence from old display fields.

## Compact protobuf evidence in v3

V3 requires `record_encoding: "proto_raw_v1"` in the header, plus
`normalization.proto_projection: "aosp_logcat_16r1_host_v1"` and
`normalization.protobuf_raw_payload: "preserved_base64"`. Missing, conflicting or
unknown declarations are errors. They identify disk encoding and frozen host
projection; `source.transport_format` continues to identify ADB transport.

A proto record stores only `proto.raw_payload` and unknown `proto` extensions.
It retains `type`, `seq`, `provenance`, `text_loss`, `legacy_original_seq`,
`event_kind` and unknown row extensions. The following keys are forbidden even
when their value is null:

- In `proto`: `time_sec`, `time_nsec`, `priority`, `uid`, `pid`, `tid`,
  `tag_bytes`, `message_bytes`, `process_name`.
- On the row: `device_time`, `pid`, `tid`, `level`, `tag`, `message`, `process`.

The payload uses the same bounded padded base64 representation as v2. Empty
`raw_payload: ""` is one valid empty protobuf frame and derives an unparsed row;
a missing or null payload is invalid. Encoded strings exceeding 1,398,104 bytes
are rejected before base64 decoding, and decoded bytes must still fit 1 MiB.
The exact payload survives, including unknown fields, duplicate occurrences,
field ordering and noncanonical varints. No prost re-encoding replaces it.
The eight-byte little-endian frame prefix can be reconstructed from its length.

`DecodedEntry::project()` is the shared fixed projection for v2 validation and
v3 reconstruction. It preserves the existing AOSP schema and host rules: remove
exactly one trailing tag NUL, preserve message whitespace/newlines/NUL, replace
invalid tag/message UTF-8 only for display, retain integer time precision and
the device process-cache hint without trimming its name. Unsupported priority
(including 7), invalid nanoseconds/IDs and default/unknown-only frames remain
unparsed. Illegal `process_name` UTF-8 remains a protobuf decoding error.
Future interpretation changes require a distinct profile. The ADB adapter must
use this same method when its integration is updated.

The retained `type` must match that projection, and proto cannot be an event.
`text_loss` remains optional; absence stays unknown. Present loss flags must
match projection and cannot claim truncation. V3 decodes/projects each raw
payload once on import; writing revalidates publicly mutable evidence/display.
Text entries, events and unparsed non-proto rows remain valid in a v3 file using
the unchanged v2 data fields with version 3.

Unknown `device_time` and `process` children move to optional
`projection_extensions.device_time` and `.process` maps. Only nonempty maps are
written. They cannot shadow `text`, `name` or `source`, and their derived parent
must exist. V2 export restores these extensions to their original locations.
Unknown members of the `projection_extensions` container are kept separately
in `RecordMetadata.projection_extra`; a v2 export retains them as an opaque
row-level `projection_extensions` extension. V2 extensions already using
`record_encoding`, `proto_projection` or `projection_extensions` remain valid
v2, but conversion rejects the collision instead of overwriting or guessing
their meaning. A v2 file with that opaque container therefore requires explicit
handling before another compact conversion.

Disk savings do not imply the same memory savings: the current `LogEntry`
continues to retain raw evidence, decoded bytes and display strings. Memory
limits account for that fully projected representation and unknown extensions,
including invalid UTF-8 display expansion; the 8 MiB line and conservative
parser working-set limits also remain in force.

## Footer, export scope, and loss

`artifact_end` has the same artifact ID as the header, `finalized: true`, and the
exact `record_count`, `first_seq`, and `last_seq` of the file. Bounds are null for
an empty file. The footer does not consume a session sequence.

`scope` contains:

- `kind`: `retained`, `filtered`, or `legacy_import`.
- `source_first_seq`, `source_last_seq`, `source_record_count`: bounds/count of
  the fixed source snapshot from which this artifact was selected.
- `omitted_before`: known prior removals, or null if unknown. This is retention
  information, not an inferred count of lost device logs.
- `filter`: a frozen filter description for `filtered` artifacts; otherwise null.

Unfiltered scopes must cover all source rows. Filtered exports preserve original
sequences, retain source bounds, and allocate a new artifact ID. Filtering an
already filtered document preserves the parent predicate as
`{"all_of":[parent_filter,new_filter]}`; it does not replace that earlier scope.
The serialized filter description records selection intent; this codec does not
re-run or certify an arbitrary filter expression.

`capture.state` is a source observation such as `running`, `stopped`, or
`unknown`. `capture.completeness` accepts only `unknown` or `partial` because this
backend cannot establish complete capture. `known_gaps` retains explicit
observations, and `observed_unix_ms` dates that status when available. An empty
known-gaps list does not establish gap-free capture. Source truncation or UTF-8
replacement flags must come from capture observations; the codec never infers
them from ordinary message text.

GUI snapshots retain at most 64 `capture.recent_observations`, including attempt
configuration, confirmed starts, requested stops and observed exits. The
`omitted_observations` count exposes older observations removed by this bound;
aggregate interruption/start-failure/resumed-pause/fault counts survive eviction
and view clearing. Diagnostics are capped at 1 KiB with a truncation flag.
Observation timestamps use the host wall clock; `after_seq` is the last record
admitted by the host, not a precise device-time boundary. A graceful final stop
alone does not claim a gap; resuming that session records an unknown interval.
Device-discovery failure does not by itself count as a stream interruption.

GUI `scope.retention_evicted_records` and `scope.view_cleared_records` distinguish
local history removal from capture interruption. Header `view_filter` records
the reading filter used when a live snapshot or a filtered derivative is
created, without claiming it constrained an unfiltered snapshot. Saving an
opened document or converting its encoding preserves that artifact's source
view metadata; a new filtered export replaces `view_filter` with its frozen
filter, independently of the authoritative `scope.filter` selection contract.
`source.capturing_host` includes OS, architecture and application version;
`source.device_discovery`, when known, retains model/state/transport from the
latest successful discovery snapshot. These are context, not verified device
build properties or proof of per-record application ownership. Filtered exports
and offline resaves preserve these optional observations.

The footer validates framing/count/range consistency. It is not a cryptographic
signature, tamper detector, or guarantee against an earlier device-side drop.

## Legacy v1 import

Existing CLI v1 files contain only entry lines and do not require a footer. A
valid final JSON value without a newline is accepted. An empty file is an empty
legacy candidate with unknown capture state.

The adapter validates v1 schema/type, positive increasing sequence, numeric
fields, and the established six log levels. It preserves the v1 sequence as
`legacy_original_seq`. It does not infer a marker from PID 0, infer a package
from Tag, invent a timezone, or interpret unknown extensions as provenance.
The display row uses the legacy entry representation; its original kind remains
unknown.

The imported document receives a new **artifact** UUID, while its capture
session, device metadata, process attribution, observation time, segment, and
attempt remain unknown. `LogEntry.provenance` stays `None`. Re-saving as native
v2 keeps `source.kind: legacy_v1`, a null session, and the original legacy
sequence. It does not manufacture a historical capture identity. Unknown v1
fields are preserved under `legacy_extensions`, preventing collisions with v2
fields of the same name.

## Host text-loss observations

Every record kind can contain optional `text_loss`:

```json
{"truncated":false,"invalid_utf8_replaced":true}
```

These booleans record what the host text adapter actually observed: reaching its
raw-line limit, or replacing invalid UTF-8 while decoding that line. Both fields
are required when the object is present. `null`/absence means unknown; v1 import
and native re-save of v1 retain that unknown state. Both booleans being false
only means the host did not observe these two losses. It does not establish that
the device emitted complete data, that logd lost nothing, or that capture had no
gap. An ordinary message ending in a truncation-looking string is not evidence.
Unknown optional fields inside `text_loss` are preserved.

## Unknown fields, limits, and failure semantics

Unknown optional fields are retained in the header, source, normalization,
records, device time, process attribution, provenance, text-loss observations,
footer, scope, and capture
status. JSON member order and textual formatting are not preserved. Duplicate
known fields are rejected; duplicate unknown keys follow the JSON map's last
value semantics. Unknown record types or incompatible required schema are
errors, because silently skipping them would alter evidence and sequence scope.
Programmatically supplied extensions may not shadow reserved fields.
Extensions for absent parent objects are rejected instead of silently dropped;
event/unparsed rows cannot carry process attribution on either read or write.

Unknown JSON numbers use `serde_json` arbitrary precision. Underflowing values
such as `1e-400`, integers beyond `u64` such as `18446744073709551617`, and long
decimal fractions survive load/select/save and v2–v3 conversion without numeric
rounding. This includes nested arrays/objects, protobuf extensions, compact
projection extensions, scope filters, and capture gaps. Exponent spelling may
be normalized (for example `1e400` to `1e+400`); core integer ranges, types, and
duplicate-field validation are unchanged. Numeric token storage counts toward
the memory estimate.

The document codec explicitly rejects the JSON object key
`$serde_json::private::Number` at every depth, including escaped spellings.
`serde_json` uses this key internally for arbitrary precision and cannot
distinguish some literal extension objects from numbers after `flatten`.
Rejection occurs on raw input before that ambiguity can change an object's
type. It applies to `load` (including v1 import), public native header/data/footer
decoders used by query, and document output. The same characters in an ordinary
string value remain valid. This is a restriction on otherwise legal JSON
objects; such input is never silently converted to a number.

Each encoded JSON line is limited to **8 MiB, including its newline if present**.
This applies equally to headers, data, and footers. ADB's raw-line limit is a
different boundary: JSON escaping may expand source text. Offline files may end
with a valid footer without a trailing newline. Native EOF without a footer,
malformed lines, identity/sequence mismatch, incorrect footer totals, and any
data after the footer—including a blank line—reject the whole candidate.

The import budget accounts for the log store, record-sidecar capacity, nested
unknown JSON values, and header/footer data. Before deserializing a line, the
loader reserves a conservative working-set estimate of 64 times its encoded
size plus a fixed allowance. This protects against small encoded arrays/maps
expanding into many owned JSON values. It can reject large benign lines before
their eventual retained representation would exhaust the budget; the error
reports the requested budget and estimate. The bounded input-line allocation and
one LogStore allocation chunk can temporarily precede the next accounting
check. These are conservative owned-data estimates, not an exact process RSS
cap; the frontend must also account for its old view and simultaneous snapshots.

Cancellation is checked between bounded records and before final success,
including during conversion accounting and sidecar cloning. One record's
bounded copy/encoding can finish before cancellation is observed; copying all
records cannot. This
offline API does not provide an interruptible network reader or implement live
tailing of a file still being written.

## Validation coverage

The module tests cover native v2/v3 empty and multi-kind documents, legacy migration
without invented identity, nested extension preservation, newer attribution
vocabulary, filtered references and composed predicates, malformed or missing
footers, trailing bytes, bad sequences/sessions, encoded-size and memory limits,
cancellation, and output I/O failures. Fixtures are synthetic and headless; they
do not establish real-device capture completeness or atomic filesystem publish
behavior. Compact tests additionally cover independent wire goldens, duplicate
fields, unknown priorities, invalid UTF-8/time/IDs, exact v2–v3–v2 semantic and
payload round trips, extension collisions, mixed-version rejection and explicit
v2 export failure for a compact NUL-heavy frame exceeding the v2 line limit.
