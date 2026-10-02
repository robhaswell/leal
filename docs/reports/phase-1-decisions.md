# Phase 1 gate — decisions for Rob

Date: 2026-10-02 · Status: **decided by Rob, 2026-10-02** (open items noted)

Every decision or approval the phase 1 gate needs from Rob, with a
recommendation and where to look. Collected from the phase 1 review,
ADR-0008 and docs/perf.md.

## To decide

1. **Accept ADR-0008** (ten editing and saving rules from the review).
   *Recommend:* accept. PLAN 2.1–2.5 carry each one as "(ADR-0008
   decision N, pending)". → `docs/adr/0008-editing-and-saving-rules.md`
2. **Run `just perf` on an unlocked Mac**, and on a base M1 Air if one is
   available. The scrolling rows say "fail", but every run was on a locked
   screen. PLAN ticks 1.10 although this is still open. *Recommend:* run it
   before approving; if scrolling still drops frames unlocked, the next step
   is an ADR (ADR-0001's fallback), which needn't hold up phase 2. →
   `docs/perf.md`, "Measuring on a base M1 Air"
3. **Idle memory: footprint or RSS.** 10.9 MB footprint against 65–68 MB
   RSS. *Recommend:* footprint (Activity Monitor's figure); `just perf` says
   "pending Rob" until then. → `docs/perf.md`, "Decisions"
4. **What the heap budget counts.** *Recommend:* every malloc zone minus
   the per-window AppKit baseline (about 21 MB), search results included,
   AppKit's drawing peaks left out: about 7 MB settled. → `docs/perf.md`,
   "Decisions"
5. **Open to first rows: cold or warm.** 95 ms cold, 36 ms warm here; on
   an Air the cold open is likely over 150 ms. *Recommend:* judge the warm
   open against 150 ms, and the cold open as part of launch (450 ms, 263 ms
   here). → `docs/perf.md`, "Decisions"
6. **Launch to empty window**, when Leal opens no empty window.
   *Recommend:* keep measuring to the end of `applicationDidFinishLaunching`.
   → `docs/perf.md`, "Decisions"
7. **Search memory**: 12 bytes per matching row (120 MB on the 1 GB file).
   *Recommend:* accept for v1; a compact store is listed under PLAN 4.1. →
   `docs/perf.md`, "Decisions"
8. **Network shares on option C** (stream a share rather than read it all
   at open), which would change accepted ADR-0006. *Recommend:* keep
   ADR-0006's read-or-copy at open for v1. Streaming needs reads kept off
   the main thread and share-specific error handling, and attribute caching
   makes changes during the copy unreliable to detect. →
   `docs/tasks/1.1a.md`, "Proposal for Rob: network shares on option C"
9. **Screenshots of the UI with no mockup** (ADR-0005 decision 8): 1.6's
   lock glyph, header titles in column widths and View ▸ Header row; 1.7's
   Treat As and Reopen with Encoding menus, suggestion banners, attribute
   and status notes, and the disconnected and changed-while-reading banners;
   1.8's Go to Row sheet, Ignore Case chevron, "wrapped" sign and 06c as a
   sheet; 1.9's changed and deleted banners, File ▸ Reload from Disk and
   their status notes. *Recommend:* approve, or list changes as a 1.x
   follow-up. → `docs/tasks/p1-gate-*.png`
10. **Column drag-to-reorder in v1**, before 2.4 builds the column map.
    *Recommend:* not in v1; it isn't in the mockups, and the column map
    keeps it possible later. → `docs/tasks/1.6.md`, open questions
11. **1.2a real exports** from Excel, Numbers and Google Sheets.
    *Recommend:* send them if you have them; otherwise 1.2a moves to
    phase 4, as PLAN already says. → `docs/PLAN.md`, 1.2a
12. **Try a real USB stick by hand**: unplug and replug it mid-copy, and
    eject it after the copy completes. This is the only sandboxed check of
    removable drives. *Recommend:* do it once, about five minutes. →
    `docs/tasks/1.9.md`, "Notes for Rob"
13. **CI benchmark thresholds** of 20% for regressions and 10% for noise.
    *Recommend:* keep them for now; tightening them after a few weeks of
    history is listed under PLAN 4.1. → `docs/tasks/1.2b.md`, open
    questions
14. **Minimum macOS and a Mac App Store build** (DESIGN §8, items 3 and 5).
    *Recommend:* macOS 14 as drafted, and decide the App Store question by
    4.5. → `docs/DESIGN.md` §8
15. **Approve phase 1** and tag `phase-1`, once the items above are
    settled. → `CLAUDE.md`, "Phase gates"

## Already decided (no action needed)

- **1.7:** the mixed line endings wording stays ("12 rows end
  differently; the rest end with CRLF"), and the diagnostics' Next stops at
  the last occurrence instead of wrapping. → `docs/tasks/1.7.md`
- **1.8:** find's Next wraps, with a brief "wrapped" sign and no beep.
  Use Selection for Find (⌘E) and the system find pasteboard are deferred
  to PLAN 4.4. A copy over about 100 MB asks first. → `docs/tasks/1.8.md`

## Rob's answers (2026-10-02)

| # | Decision |
|---|---|
| 1 | ADR-0008 accepted. |
| 2 | Rob ran `just perf` unlocked (`docs/perf-runs/2026-10-02-m5pro-unlocked.md`). No M1 Air is available, so Rob **accepted the 3× rule**: the M1 Air stays the design target, the M5 Pro must show about 3× headroom (main-thread work per frame ≤ ~2.8 ms), and an Air is confirmed during the beta. To be written into DESIGN §1. |
| 3 | Idle memory is measured as **physical footprint**. |
| 4 | Heap: every malloc zone minus the per-window AppKit baseline; search results counted; AppKit drawing peaks excluded. |
| 5 | Open is judged warm against 150 ms; the cold open counts as part of launch. |
| 6 | Launch measured to the end of `applicationDidFinishLaunching`. |
| 7 | Search memory accepted for v1. |
| 8 | Network shares stream like removable drives, prioritising first paint: **ADR-0009** (accepted), PLAN task 2.0. |
| 9 | Screenshots of the unmocked UI approved. |
| 10 | No column drag-to-reorder in v1. |
| 11 | 1.2a moves to phase 4 (Rob: ignore for now). |
| 12 | Deferred: Rob tests a real USB stick later. |
| 13 | Benchmark thresholds kept. |
| 14 | macOS 14 minimum; the App Store question is decided by 4.5. |
| 15 | Phase 1 approved. |
