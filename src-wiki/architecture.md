# Architecture

Constraints the code doesn't state on its own. The [contributor guide](../CONTRIBUTING.md#current-architecture)
covers how the pieces fit.

## Stack

- ratatui + crossterm for the TUI; noodles for all format I/O. Don't add rust-htslib: the stack is
  pure Rust on purpose.
- Input is coordinate-sorted, BAI-indexed BAM. CRAM and CSI (needed for contigs over 512 Mbp) are
  out of scope until BAM/BAI is solid.
- File work runs off the render thread because HPC network filesystems (Lustre, GPFS) have
  random-access latency long enough to stall drawing.
- Apus bounds thread use on shared HPC login nodes: one shared I/O worker, plus one scan worker
  per loaded BAM so local-area scans don't hold up other file work. Keep BGZF decompression on the
  worker that runs the query, and avoid APIs that start a Rayon pool.
- `apus-desktop/` is a separate workspace member (`default-members = ["."]`) using direct `wgpu` +
  Slug rendering; keep graphics dependencies out of root `Cargo.toml` and keep `crossterm` /
  `ratatui/crossterm` behind the default `terminal` feature so `apus-desktop` builds without them.
- The [WebGL2 demo runner](../apus-web/) builds outside the native workspace. It shares the
  desktop renderer and uses resident data sources. Its App starts without file workers.

## Commands

**`CommandOutcome` is grouped by recipient, and no match over it ends in a wildcard.** `Shell`,
`Genome` and `Table` hold the commands exactly one receiver applies; the remaining variants are
shared and every viewer reads them on its own axis. Owners match their group exhaustively and
everyone else ignores the whole group in one arm, so a new grouped command is a compile error only
for its owner and a new shared variant is one in every viewer. A variant belongs in a group only if
exactly one receiver ever answers anything but `NotApplicable`. Keep the shared list short rather
than reaching for a trait-object message.

**A command must have a receiver that does something.** `:refresh` was deleted (2026-08-06)
because every viewer inherited a no-op for it.

**No command may wait for acknowledgement.** Confirmation prompts were removed permanently
(2026-08-12): a prompt works only for typed lines, never for a keybind or a startup batch. A
command that would want one gets an explicit second command instead, as `:load` refuses and names
the `:load_index` line that would build the index.

**The router calls the shell synchronously through `DispatchTarget`**, so the next command in a
batch sees the effects of the last (a focus change, say). The earlier design returned effects to be
applied after routing and could not keep that order.

**Resolution reads no viewer state.** `command::parse` turns text into an outcome without knowing
which tab is active. Anything only a receiver can check (a contig name, a palette entry, whether a
VCF row index is ready) is validated on apply. An outcome whose meaning depends on the receiver
therefore carries every reading: `Goto` holds both a bp pair and a row number, and each viewer takes
the one its axis uses. Choosing the reading by active tab was rejected because the router would
need viewer knowledge, and the resolved value would depend on who received it.

**An outcome whose recipient may not be the active tab is broadcast** (`router::is_broadcast`), and
viewers that don't own it answer `NotApplicable`. The router never looks a viewer up by type.

**Asynchronous failures are notifications, not `Applied::Err`.** `apply_outcome` returns before a
worker touches the disk, so a worker-side failure (`:load` with no cache) arrives as a sticky
notification. A worker can't know whether its command came from the bar, a key or startup, so
the severity is the same for all three.

## Startup

The startup config replays through the ordinary command router after the palette. Positional
files run last and retain their argument order for tab focus, even if the config changed tabs.
Each BAM path is remembered in the Genome viewer. Only the active BAM owns read and scan sources,
and a replacement installs both together. A manual switch preserves the locus. When a VCF jump
needs a BAM choice, Genome sends its path inventory back through the router. The VCF viewer reads
genotypes for that record on the I/O worker, keeping its row buffer independent of sample count.
It retains focus until a choice sends the BAM and locus together to Genome.

## Status row

The status row shows the viewer's `status_override` and feedback on the command just typed, nothing
else. Every message a viewer originates is a `Notification`, which has severity and is drawn
bottom-right. When viewers could write to the status row, a background job finishing blanked the
coordinate readout mid-scroll.

Every diagnostic (startup, command dispatch, worker death, viewer notifications) is appended to
`App`'s `DiagnosticLog`, which a runner without a screen reads through `App::has_errors()` and
`App::diagnostics()`. `App::tick()` ticks every viewer and logs every viewer's records at once, but
`NotificationStack` displays a tab's records only once it is active. A file named on the command
line that fails to load is an `Error`, so headless capture can exit non-zero on it; palette-file
problems stay `Warn`.

## Layer boundaries

- `crossterm` stays confined to `src/main.rs`. Library modules accept backend-agnostic
  `crate::input` types (`Key`, `Modifiers`, `MouseEvent`), and shared bootstrap/layout/settle-loop
  code lives in `src/runtime.rs`, keeping `libapus` free of terminal event dependencies for
  alternative front ends.
- `ViewerApp::has_pending_work()` is separate from `StatusProvider::background_work()`.
  `background_work()` drives the `.busy()` keybind guard (blocking disruptive actions during long
  index builds), whereas `has_pending_work()` reports any in-flight view fetch or animation for
  readiness checks without locking out normal navigation.
- noodles record types stay out of rendering and UI. Reads cross the seam as
  `alignment::read::Read`, mapped at first contact.
- Pileup reads keep CIGAR and SEQ in BAM's packed encodings. The ingest boundary rejects unknown
  CIGAR opcodes, and rendering matches the crate-owned `CigarKind` exhaustively.
- No `unwrap()` on file I/O a user can hit (bad index, missing `.fai`, network glitch). Report it as a
  notification.

## Invariants

The [project invariants](../CONTRIBUTING.md#project-invariants--read-before-any-edit) describe the
current shape and change with it. Source comments cite them by number, so a retired invariant keeps
its number and is marked retired. Prefer invariants a reader can check ("`command` imports no
viewer state") over aspirations ("viewers are loosely coupled").
