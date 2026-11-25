# .ai Save Size Investigation

> **Status:** Investigation proposal  
> **Created:** 2026-11-20  
> **Related:** PR B - .ai save size bloat

## Problem Statement

`.ai` files are reported to be 3-15x the size of their source documents. This is caused by embedding two full editing copies of the document:

1. **VectorCraft native document** - for VectorCraft to reopen the document with full fidelity
2. **AIPrivateData** - for Adobe Illustrator to open the document with editability

Both copies are compressed (zstd level 9) but still represent substantial data duplication.

## Current Implementation

A PDF-compatible `.ai` file contains:
1. PDF page content (visible art, potentially rasterized/flattened)
2. VectorCraft native document (embedded in PDF, compressed)
3. AIPrivateData (Illustrator-format editing data, compressed)

See:
- `crates/engine/src/cmd/fileio/save.rs` lines 840-868 (native and private closures)
- `crates/eps/src/ai_editing.rs` (AIPrivateData generation)
- `crates/pdf/src/piece_info.rs` (AIPrivateData embedding)

## Why Both Copies Exist

| Copy | Purpose | Compression |
|------|---------|-------------|
| VectorCraft native | Full VectorCraft editability (effects, symbols, swatches, styles) | zstd level 9 via format |
| AIPrivateData | Illustrator compatibility (layers, paths, colors, artboards) | zstd level 9 |

Both serve different apps and have different capabilities:
- VectorCraft native preserves features Illustrator doesn't understand
- AIPrivateData writes in a format Illustrator can read

## Investigation Questions

1. **User intent analysis**: Do users save `.ai` primarily for:
   - Illustrator compatibility (needs AIPrivateData)
   - VectorCraft editing preservation (needs native)
   - Both (needs both)

2. **Size breakdown**: What percentage of file size comes from:
   - PDF page content
   - VectorCraft native copy
   - AIPrivateData copy
   - Shared resources (images embedded multiple times)

3. **Deduplication opportunities**:
   - Can images be shared between copies?
   - Can text/path data be deduplicated?
   - Is there redundant metadata?

4. **User options**: Should there be explicit options like:
   - `editingData: false` (already exists - disables AIPrivateData)
   - New option to disable VectorCraft native while keeping AIPrivateData?

## Risks of Changing

- **Data loss**: Removing either copy means losing editability in one app
- **Compatibility**: AIPrivateData format is reverse-engineered from user files
- **Round-trip fidelity**: Changes could affect open/save/open cycles

## Proposed Next Steps

1. **Measure**: Profile file sizes on representative documents to understand the breakdown
2. **Document**: Add size expectations to user-facing documentation
3. **Consider**: User-facing option to choose "VectorCraft only" or "Illustrator compatible"
4. **Investigate**: Image deduplication between the three representations

## Implementation Priority

**Low** - The current approach is correct for maximum compatibility. File size is a tradeoff for editability. Any changes risk breaking round-trip fidelity.

The existing `editingData: false` option already allows users to reduce size when they don't need Illustrator compatibility.
