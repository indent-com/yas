//! Codec-1 terminal deltas against the last completely written view frame.

use std::collections::BTreeMap;
use std::sync::Arc;

use yas_terminal_model::FrameState;
use yas_wire::schema::terminal::*;
use yas_wire::terminal::{Cell, Component, Grid, GridOperation, TerminalFrame};

use super::super::yas_terminal_backend::FrameGuard;
use super::{
    TerminalView, encode_terminal_grid_codec1, terminal_hyperlink_component,
    terminal_line_flags_component, terminal_overflow_component,
};

pub(super) struct FrameBase {
    sequence: u32,
    guard: FrameGuard,
    state: FrameState,
    scroll_offset: i64,
}

pub(super) fn encode(
    view: &mut TerminalView,
    state: &FrameState,
    guard: &FrameGuard,
) -> Option<(TerminalFrame, Arc<FrameBase>)> {
    let mut state = snapshot(state, view.rows, view.cols);
    // A reset, scroll, restart, or resize invalidates the backend epoch. A
    // drained old-generation write must never become the new epoch's base.
    let base = view.frame_base.as_deref().filter(|base| {
        base.guard.is_current()
            && base.sequence.wrapping_add(1) == view.next_sequence
            && (base.state.rows, base.state.cols) == (state.rows, state.cols)
    });
    let mut grid = make_grid(&state, view.scroll_offset, base);
    let mut flags = grid_flags(&grid, view.final_state);
    if flags == 0 && grid.operations.is_empty() {
        return None;
    }
    let dimensions = base.map(|base| (base.state.rows, base.state.cols));
    let grid_payload =
        match encode_terminal_grid_codec1(&grid, &mut flags, view.frame_bound, dimensions) {
            Ok(payload) => payload,
            Err(_) => {
                // Keep the declared decoded bound. A dense delta can be larger
                // than a keyframe; try the full representation before shedding
                // optional state. The saved base must match exactly what is sent.
                grid = make_grid(&state, view.scroll_offset, None);
                flags = grid_flags(&grid, view.final_state);
                if let Ok(payload) =
                    encode_terminal_grid_codec1(&grid, &mut flags, view.frame_bound, None)
                {
                    payload
                } else {
                    strip_optional_state(&mut state);
                    grid = make_grid(&state, view.scroll_offset, None);
                    flags = grid_flags(&grid, view.final_state);
                    encode_terminal_grid_codec1(&grid, &mut flags, view.frame_bound, None).ok()?
                }
            }
        };
    let sequence = view.next_sequence;
    view.next_sequence = sequence.wrapping_add(1);
    let base = Arc::new(FrameBase {
        sequence,
        guard: guard.clone(),
        state,
        scroll_offset: view.scroll_offset,
    });
    Some((
        TerminalFrame {
            view_id: view.view_id,
            frame_sequence: sequence,
            frame_flags: flags,
            base_sequence: None,
            grid_payload,
        },
        base,
    ))
}

fn snapshot(source: &FrameState, rows: u16, cols: u16) -> FrameState {
    // Views share one PTY geometry; never pad an offer beyond that grid.
    let rows = rows.min(source.rows);
    let cols = cols.min(source.cols);
    let mut state = FrameState::new(rows, cols);
    state.cursor_row = source.cursor_row.min(rows.saturating_sub(1));
    state.cursor_col = source.cursor_col.min(cols.saturating_sub(1));
    state.mode = source.mode;
    state.keyboard_flags = source.keyboard_flags;
    state.title.clone_from(&source.title);
    state.scrollback_lines = source.scrollback_lines;
    state
        .cell_links
        .resize(usize::from(rows) * usize::from(cols), 0);
    for row in 0..usize::from(rows) {
        let src = row * usize::from(source.cols);
        let dst = row * usize::from(cols);
        let end = dst + usize::from(cols);
        state.cells[dst * 12..end * 12]
            .copy_from_slice(&source.cells[src * 12..(src + usize::from(cols)) * 12]);
        let available = source
            .cell_links
            .len()
            .saturating_sub(src)
            .min(usize::from(cols));
        if available != 0 {
            state.cell_links[dst..dst + available]
                .copy_from_slice(&source.cell_links[src..src + available]);
        }
        state.line_flags[row] = source.line_flags.get(row).copied().unwrap_or(0);
    }
    for (&index, value) in &source.overflow {
        let row = index / usize::from(source.cols).max(1);
        let col = index % usize::from(source.cols).max(1);
        if row < usize::from(rows) && col < usize::from(cols) {
            state
                .overflow
                .insert(row * usize::from(cols) + col, value.clone());
        }
    }
    // Only retain URI entries used in this view's crop.
    for id in &state.cell_links {
        if *id != 0
            && let Some(uri) = source.link_uris.get(id)
        {
            state.link_uris.entry(*id).or_insert_with(|| uri.clone());
        }
    }
    state
}

fn strip_optional_state(state: &mut FrameState) {
    state.line_flags.fill(0);
    state.overflow.clear();
    state.cell_links.fill(0);
    state.link_uris.clear();
    for cell in state.cells.as_chunks_mut::<12>().0 {
        cell[1] &= !0x40; // A missing hyperlink table cannot leave link-marked cells.
        if (cell[1] >> 3) & 7 == 7 {
            // An overflow marker requires its string even in an optional
            // component. Use a valid inline replacement if the string cannot fit.
            cell[1] = (cell[1] & !0x38) | (3 << 3);
            cell[8..12].copy_from_slice(&[0xef, 0xbf, 0xbd, 0]);
        }
    }
}

fn make_grid(state: &FrameState, scroll_offset: i64, base: Option<&FrameBase>) -> Grid {
    let previous = base.map(|base| &base.state);
    let keyframe = previous.is_none();
    let cells = state.cells.as_chunks::<12>().0;
    let mut grid = Grid {
        dimensions: keyframe.then_some((state.rows, state.cols)),
        cursor: previous
            .is_none_or(|old| {
                (old.cursor_row, old.cursor_col) != (state.cursor_row, state.cursor_col)
            })
            .then_some((state.cursor_row, state.cursor_col)),
        modes: previous
            .is_none_or(|old| old.mode != state.mode)
            .then_some(state.mode),
        scrollback_lines: previous
            .is_none_or(|old| old.scrollback_lines != state.scrollback_lines)
            .then_some(state.scrollback_lines),
        scroll_offset: base
            .is_none_or(|old| old.scroll_offset != scroll_offset)
            .then_some(scroll_offset),
        title: previous
            .is_none_or(|old| old.title != state.title)
            .then(|| state.title.clone()),
        operations: Vec::new(),
        components: Vec::new(),
    };
    let Some(previous) = previous else {
        // One full run also minimizes the maximum keyframe size used when
        // negotiating receive credit, independent of cell contents.
        grid.operations.push(GridOperation::PatchRun {
            start_cell: 0,
            cells: cells.to_vec(),
        });
        grid.components = components(state, None, &(0..cells.len()).collect::<Vec<_>>());
        return grid;
    };
    let dirty = dirty_cells(state, previous);
    grid.operations = patches(cells, &dirty);
    grid.components = components(state, Some(previous), &dirty);
    // Full-width terminal scrolling usually changes most cell positions.
    // Search the same small row window as the earlier terminal diff encoder.
    if dirty.len() >= usize::from(state.cols) * 3
        && let Some(shift) = scroll_shift(state, previous)
    {
        let mut shifted = previous.clone();
        let copy = copy_rows(&mut shifted, shift);
        let dirty = dirty_cells(state, &shifted);
        let mut scrolled = grid.clone();
        scrolled.operations = vec![copy];
        scrolled.operations.extend(patches(cells, &dirty));
        scrolled.components = components(state, Some(&shifted), &dirty);
        // Include component and operation-count overhead, not only cell bytes.
        let size = |grid: &Grid| {
            grid.encode_codec1(
                grid_flags(grid, false),
                u32::MAX,
                Some((state.rows, state.cols)),
            )
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
        };
        if size(&scrolled) < size(&grid) {
            grid = scrolled;
        }
    }
    grid
}

fn grid_flags(grid: &Grid, final_state: bool) -> u16 {
    let mut flags = if final_state {
        FRAME_FINAL_STATE as u16
    } else {
        0
    };
    for (present, flag) in [
        (grid.dimensions.is_some(), FRAME_KEYFRAME | FRAME_DIMENSIONS),
        (grid.cursor.is_some(), FRAME_CURSOR),
        (grid.modes.is_some(), FRAME_MODES),
        (grid.scrollback_lines.is_some(), FRAME_SCROLLBACK),
        (grid.scroll_offset.is_some(), FRAME_VIEW_OFFSET),
        (grid.title.is_some(), FRAME_TITLE),
        (!grid.components.is_empty(), FRAME_COMPONENTS),
    ] {
        if present {
            flags |= flag as u16;
        }
    }
    flags
}

fn dirty_cells(state: &FrameState, previous: &FrameState) -> Vec<usize> {
    state
        .cells
        .as_chunks::<12>()
        .0
        .iter()
        .zip(previous.cells.as_chunks::<12>().0)
        .enumerate()
        .filter_map(|(index, (new, old))| {
            (new != old || state.overflow.get(&index) != previous.overflow.get(&index))
                .then_some(index)
        })
        .collect()
}

fn components(
    state: &FrameState,
    previous: Option<&FrameState>,
    dirty: &[usize],
) -> Vec<Component> {
    let mut components = Vec::new();
    let mut push = |kind, body| {
        components.push(Component {
            kind: kind as u8,
            required: false,
            body,
        })
    };
    if previous.map_or_else(
        || state.line_flags.iter().any(|flag| *flag != 0),
        |old| state.line_flags != old.line_flags,
    ) {
        push(
            COMPONENT_LINE_FLAGS,
            terminal_line_flags_component(&state.line_flags),
        );
    }
    let overflow: BTreeMap<_, _> = dirty
        .iter()
        .filter_map(|index| state.overflow.get(index).map(|text| (*index, text.clone())))
        .collect();
    if !overflow.is_empty() {
        push(
            COMPONENT_OVERFLOW_STRINGS,
            terminal_overflow_component(&overflow),
        );
    }
    if previous.map_or_else(
        || !state.link_uris.is_empty(),
        |old| state.cell_links != old.cell_links || state.link_uris != old.link_uris,
    ) {
        push(
            COMPONENT_HYPERLINKS,
            terminal_hyperlink_component(&state.cell_links, &state.link_uris),
        );
    }
    if previous.map_or(state.keyboard_flags != 0, |old| {
        state.keyboard_flags != old.keyboard_flags
    }) {
        push(COMPONENT_KEYBOARD_FLAGS, vec![state.keyboard_flags]);
    }
    components
}

fn uleb_len(value: usize) -> usize {
    ((usize::BITS - value.leading_zeros()).max(1) as usize).div_ceil(7)
}

// Pick the cheapest raw patch encoding, including the operation count. Never
// charge sparse changes for an uncropped bitmap of the complete terminal.
fn patches(cells: &[Cell], dirty: &[usize]) -> Vec<GridOperation> {
    let Some(&first) = dirty.first() else {
        return Vec::new();
    };
    let span = dirty.last().unwrap() - first + 1;
    let cell_bytes = dirty.len() * 12;
    let list_size = 2
        + uleb_len(dirty.len())
        + uleb_len(first)
        + dirty
            .windows(2)
            .map(|pair| uleb_len(pair[1] - pair[0]))
            .sum::<usize>()
        + cell_bytes;
    let bitmap_size = 2 + uleb_len(first) + uleb_len(span) + span.div_ceil(8) + cell_bytes;
    let mut runs = Vec::new();
    let mut start = 0;
    for end in 1..=dirty.len() {
        if end == dirty.len() || dirty[end] != dirty[end - 1] + 1 {
            runs.push((dirty[start], end - start));
            start = end;
        }
    }
    let run_size = uleb_len(runs.len())
        + cell_bytes
        + runs
            .iter()
            .map(|(start, len)| 1 + uleb_len(*start) + uleb_len(*len))
            .sum::<usize>();
    if run_size <= list_size.min(bitmap_size) {
        runs.into_iter()
            .map(|(start, len)| GridOperation::PatchRun {
                start_cell: start as u32,
                cells: cells[start..start + len].to_vec(),
            })
            .collect()
    } else if list_size <= bitmap_size {
        vec![GridOperation::PatchList {
            indices: dirty.iter().map(|index| *index as u32).collect(),
            cells: dirty.iter().map(|index| cells[*index]).collect(),
        }]
    } else {
        let mut bitmap = vec![0; span.div_ceil(8)];
        for index in dirty {
            bitmap[(index - first) / 8] |= 1 << ((index - first) % 8);
        }
        vec![GridOperation::PatchBitmap {
            start_cell: first as u32,
            span: span as u32,
            bitmap,
            cells: dirty.iter().map(|index| cells[*index]).collect(),
        }]
    }
}

fn scroll_shift(state: &FrameState, previous: &FrameState) -> Option<i16> {
    if state.rows < 4 {
        return None;
    }
    let cells = state.cells.as_chunks::<12>().0;
    let old = previous.cells.as_chunks::<12>().0;
    let cols = usize::from(state.cols);
    let mut best = None;
    let mut best_matches = 0;
    for distance in 1..=state.rows.saturating_sub(3).min(8) {
        let offset = usize::from(distance) * cols;
        let overlap = cells.len() - offset;
        for shift in [distance as i16, -(distance as i16)] {
            let (new, old) = if shift > 0 {
                (&cells[..overlap], &old[offset..])
            } else {
                (&cells[offset..], &old[..overlap])
            };
            let matches = new.iter().zip(old).filter(|(new, old)| new == old).count();
            if matches * 5 >= overlap * 4 && matches > best_matches {
                best = Some(shift);
                best_matches = matches;
            }
        }
    }
    best
}

fn copy_rows(state: &mut FrameState, shift: i16) -> GridOperation {
    let distance = shift.unsigned_abs();
    let rows = state.rows - distance;
    let cols = usize::from(state.cols);
    let (src_row, dst_row) = if shift > 0 {
        (distance, 0)
    } else {
        (0, distance)
    };
    let src = usize::from(src_row) * cols;
    let dst = usize::from(dst_row) * cols;
    let count = usize::from(rows) * cols;
    state
        .cells
        .copy_within(src * 12..(src + count) * 12, dst * 12);
    state.cell_links.copy_within(src..src + count, dst);
    let copied = state
        .overflow
        .range(src..src + count)
        .map(|(index, text)| (dst + index - src, text.clone()))
        .collect::<Vec<_>>();
    state
        .overflow
        .retain(|index, _| *index < dst || *index >= dst + count);
    state.overflow.extend(copied);
    GridOperation::CopyRect {
        src_row,
        src_col: 0,
        dst_row,
        dst_col: 0,
        rows,
        cols: state.cols,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::terminal_encoding_view;
    use super::super::{
        TerminalFrameReceipt, TerminalFrameWriteCompletion, TerminalFrameWriteOutcome,
        terminal_frame_written,
    };
    use super::*;

    fn write(
        view: &mut TerminalView,
        state: &FrameState,
        guard: &FrameGuard,
    ) -> (TerminalFrame, Grid) {
        let (frame, base) = encode(view, state, guard).unwrap();
        let grid = frame
            .decode_grid_codec1(view.frame_bound, Some((state.rows, state.cols)))
            .unwrap();
        view.available_frame_slots -= 1;
        terminal_frame_written(
            view,
            &TerminalFrameReceipt {
                view_id: frame.view_id,
                sequence: frame.frame_sequence,
                final_state: view.final_state,
                completion: Arc::new(TerminalFrameWriteCompletion::new()),
                frame_base: base,
            },
            TerminalFrameWriteOutcome::Written,
        )
        .unwrap();
        // The test receiver presents each written frame immediately.
        view.available_frame_slots += 1;
        view.acknowledged_sequence = frame.frame_sequence;
        (frame, grid)
    }

    fn letter(state: &mut FrameState, index: usize, byte: u8) {
        let cell = &mut state.cells.as_chunks_mut::<12>().0[index];
        *cell = [0; 12];
        cell[1] = 8;
        cell[8] = byte;
    }

    fn apply_cells(cells: &mut Vec<Cell>, grid: &Grid) {
        if let Some((rows, cols)) = grid.dimensions {
            *cells = vec![[0; 12]; usize::from(rows) * usize::from(cols)];
        }
        for op in &grid.operations {
            match op {
                GridOperation::PatchRun {
                    start_cell,
                    cells: patch,
                } => cells[*start_cell as usize..*start_cell as usize + patch.len()]
                    .copy_from_slice(patch),
                GridOperation::PatchList {
                    indices,
                    cells: patch,
                } => {
                    for (index, cell) in indices.iter().zip(patch) {
                        cells[*index as usize] = *cell;
                    }
                }
                GridOperation::PatchBitmap {
                    start_cell,
                    span,
                    bitmap,
                    cells: patch,
                } => {
                    let mut patch = patch.iter();
                    for bit in 0..*span as usize {
                        if bitmap[bit / 8] & (1 << (bit % 8)) != 0 {
                            cells[*start_cell as usize + bit] = *patch.next().unwrap();
                        }
                    }
                    assert!(patch.next().is_none());
                }
                GridOperation::CopyRect {
                    src_row,
                    src_col,
                    dst_row,
                    dst_col,
                    rows,
                    cols,
                } => {
                    assert_eq!((*src_col, *dst_col), (0, 0));
                    let cols = usize::from(*cols);
                    let src = usize::from(*src_row) * cols;
                    cells.copy_within(
                        src..src + usize::from(*rows) * cols,
                        usize::from(*dst_row) * cols,
                    );
                }
                _ => panic!("unexpected operation"),
            }
        }
    }

    #[test]
    fn keyboard_mode_only_deltas_and_resets_are_transmitted() {
        let mut view = terminal_encoding_view(3, 10);
        let guard = FrameGuard::current_for_test();
        let mut state = FrameState::new(3, 10);
        write(&mut view, &state, &guard);
        for flags in [31, 1, 0] {
            state.keyboard_flags = flags;
            let (frame, grid) = write(&mut view, &state, &guard);
            assert_eq!(frame.frame_flags, FRAME_COMPONENTS as u16);
            assert!(grid.operations.is_empty());
            assert_eq!(
                grid.components,
                vec![Component {
                    kind: COMPONENT_KEYBOARD_FLAGS as u8,
                    required: false,
                    body: vec![flags],
                }]
            );
            assert!(encode(&mut view, &state, &guard).is_none());
        }
        state.keyboard_flags = 31;
        assert_eq!(snapshot(&state, 2, 5).keyboard_flags, 31);
        assert!(
            make_grid(&state, 0, None)
                .components
                .iter()
                .any(|c| c.kind == COMPONENT_KEYBOARD_FLAGS as u8 && c.body == [31])
        );
    }

    #[test]
    fn one_cell_and_cursor_updates_do_not_scale_with_grid_size() {
        for (rows, cols) in [(3, 10), (150, 182)] {
            let mut view = terminal_encoding_view(rows, cols);
            let guard = FrameGuard::current_for_test();
            let mut state = FrameState::new(rows, cols);
            let (first, _) = write(&mut view, &state, &guard);
            assert_ne!(first.frame_flags & FRAME_KEYFRAME as u16, 0);
            let index = usize::from(rows) * usize::from(cols) - 1;
            letter(&mut state, index, b'x');
            let (frame, grid) = write(&mut view, &state, &guard);
            assert_eq!(frame.frame_sequence, 2);
            assert_eq!(frame.frame_flags & FRAME_KEYFRAME as u16, 0);
            assert!(
                frame.grid_payload.len() <= 18,
                "one-cell update grew to {}",
                frame.grid_payload.len()
            );
            assert_eq!(
                grid.operations,
                vec![GridOperation::PatchRun {
                    start_cell: index as u32,
                    cells: vec![state.cells.as_chunks::<12>().0[index]]
                }]
            );
            state.set_cursor(rows - 1, cols - 1);
            let (frame, grid) = write(&mut view, &state, &guard);
            assert_eq!(frame.grid_payload.len(), 5);
            assert_eq!(frame.frame_flags, FRAME_CURSOR as u16);
            assert!(grid.operations.is_empty());
            assert!(encode(&mut view, &state, &guard).is_none());
            assert_eq!(view.next_sequence, 4);
            view.final_state = true;
            let (frame, _) = write(&mut view, &state, &guard);
            assert_eq!(frame.frame_flags, FRAME_FINAL_STATE as u16);
            assert_eq!(frame.grid_payload.len(), 1);
        }
    }

    #[test]
    fn patch_selection_obeys_sparse_and_cropped_bitmap_size_bounds() {
        let state = FrameState::new(10, 100);
        let cells = state.cells.as_chunks::<12>().0;
        assert!(matches!(
            patches(cells, &[500, 501, 502])[0],
            GridOperation::PatchRun { .. }
        ));
        assert!(matches!(
            patches(cells, &[0, 999])[0],
            GridOperation::PatchList { .. }
        ));
        assert!(matches!(
            patches(cells, &[500, 502, 504, 506])[0],
            GridOperation::PatchBitmap { .. }
        ));
        for stride in 1..40 {
            let dirty = (117..947).step_by(stride).collect::<Vec<_>>();
            let mut grid = Grid {
                dimensions: None,
                cursor: None,
                modes: None,
                scrollback_lines: None,
                scroll_offset: None,
                title: None,
                operations: patches(cells, &dirty),
                components: Vec::new(),
            };
            let size = |grid: &Grid| {
                grid.encode_codec1(0, u32::MAX, Some((10, 100)))
                    .unwrap()
                    .len()
            };
            let selected = size(&grid);
            let patch = dirty.iter().map(|index| cells[*index]).collect::<Vec<_>>();
            grid.operations = vec![GridOperation::PatchList {
                indices: dirty.iter().map(|index| *index as u32).collect(),
                cells: patch.clone(),
            }];
            assert!(selected <= size(&grid));
            let span = dirty.last().unwrap() - dirty[0] + 1;
            let mut bitmap = vec![0; span.div_ceil(8)];
            for index in &dirty {
                let bit = index - dirty[0];
                bitmap[bit / 8] |= 1 << (bit % 8);
            }
            grid.operations = vec![GridOperation::PatchBitmap {
                start_cell: dirty[0] as u32,
                span: span as u32,
                bitmap,
                cells: patch,
            }];
            assert!(selected <= size(&grid));
        }
    }

    #[test]
    fn scroll_deltas_copy_rows_and_round_trip_in_both_directions() {
        for shift in [1, -1, 8, -8] {
            let mut view = terminal_encoding_view(24, 80);
            let guard = FrameGuard::current_for_test();
            let mut state = FrameState::new(24, 80);
            for index in 0..24 * 80 {
                letter(&mut state, index, b'a' + (index / 80) as u8);
            }
            let mut receiver = Vec::new();
            let (_, grid) = write(&mut view, &state, &guard);
            apply_cells(&mut receiver, &grid);
            state.cell_links.resize(24 * 80, 0);
            copy_rows(&mut state, shift);
            let range = if shift > 0 {
                (24 - shift as usize) * 80..24 * 80
            } else {
                0..(-shift as usize) * 80
            };
            for index in range {
                letter(&mut state, index, b'!');
            }
            let (_, grid) = write(&mut view, &state, &guard);
            assert!(matches!(grid.operations[0], GridOperation::CopyRect { .. }));
            apply_cells(&mut receiver, &grid);
            assert_eq!(receiver, state.cells.as_chunks::<12>().0);
            // Subsequent sparse edits must use the post-scroll baseline.
            for step in 0..30 {
                let index = (step * 67) % (24 * 80);
                letter(&mut state, index, b'0' + (step % 10) as u8);
                let (_, grid) = write(&mut view, &state, &guard);
                apply_cells(&mut receiver, &grid);
                assert_eq!(receiver, state.cells.as_chunks::<12>().0);
            }
        }
    }

    #[test]
    fn metadata_clears_and_overflow_hash_collisions_are_sent() {
        let mut view = terminal_encoding_view(5, 10);
        let guard = FrameGuard::current_for_test();
        let mut state = FrameState::new(5, 10);
        letter(&mut state, 12, b'x');
        state.cells[12 * 12 + 1] = 0x40 | (7 << 3);
        state.overflow.insert(12, "long unicode cluster".into());
        state.cell_links.resize(50, 0);
        state.cell_links[12] = 1;
        state.link_uris.insert(1, "https://example.com".into());
        state.set_wrapped(1, true);
        state.set_title("title");
        write(&mut view, &state, &guard);
        state
            .overflow
            .insert(12, "different string with identical cell bytes".into());
        let (_, grid) = write(&mut view, &state, &guard);
        assert_eq!(grid.operations.len(), 1);
        assert_eq!(grid.components.len(), 1);
        assert_eq!(grid.components[0].kind, COMPONENT_OVERFLOW_STRINGS as u8);
        assert_eq!(
            grid.components[0].body,
            terminal_overflow_component(&state.overflow)
        );
        letter(&mut state, 12, b'x');
        state.overflow.clear();
        state.cell_links.fill(0);
        state.link_uris.clear();
        state.set_wrapped(1, false);
        state.set_title("");
        let (_, grid) = write(&mut view, &state, &guard);
        assert_eq!(grid.title.as_deref(), Some(""));
        assert_eq!(
            grid.components
                .iter()
                .map(|c| (c.kind, c.body.clone()))
                .collect::<Vec<_>>(),
            vec![
                (COMPONENT_LINE_FLAGS as u8, vec![0]),
                (COMPONENT_HYPERLINKS as u8, vec![0, 0])
            ]
        );
    }

    #[test]
    fn only_written_frames_advance_the_baseline_and_cutovers_force_keyframes() {
        let mut view = terminal_encoding_view(5, 10);
        let guard = FrameGuard::current_for_test();
        let mut state = FrameState::new(5, 10);
        write(&mut view, &state, &guard);
        let old = view.frame_base.clone().unwrap();
        letter(&mut state, 1, b'x');
        let (frame, candidate) = encode(&mut view, &state, &guard).unwrap();
        assert!(Arc::ptr_eq(view.frame_base.as_ref().unwrap(), &old));
        view.available_frame_slots -= 1;
        terminal_frame_written(
            &mut view,
            &TerminalFrameReceipt {
                view_id: 1,
                sequence: frame.frame_sequence,
                final_state: false,
                completion: Arc::new(TerminalFrameWriteCompletion::new()),
                frame_base: candidate,
            },
            TerminalFrameWriteOutcome::Discarded,
        )
        .unwrap();
        assert!(Arc::ptr_eq(view.frame_base.as_ref().unwrap(), &old));
        assert_eq!(view.next_sequence, 2);
        assert_eq!(view.available_frame_slots, 3);
        let (_, grid) = write(&mut view, &state, &guard);
        assert_eq!(
            grid.operations.len(),
            1,
            "discarded edit must be retransmitted"
        );
        guard.invalidate_for_test();
        let guard = FrameGuard::current_for_test();
        let (frame, _) = write(&mut view, &state, &guard);
        assert_ne!(frame.frame_flags & FRAME_KEYFRAME as u16, 0);
        state = FrameState::new(4, 10);
        let (frame, _) = write(&mut view, &state, &guard);
        assert_ne!(frame.frame_flags & FRAME_KEYFRAME as u16, 0);
        let mut other_view = terminal_encoding_view(4, 10);
        let (frame, _) = write(&mut other_view, &state, &guard);
        assert_ne!(frame.frame_flags & FRAME_KEYFRAME as u16, 0);
        view.next_sequence = u32::MAX;
        let (frame, _) = write(&mut view, &state, &guard);
        assert_ne!(frame.frame_flags & FRAME_KEYFRAME as u16, 0);
        letter(&mut state, 0, b'w');
        let (frame, _) = write(&mut view, &state, &guard);
        assert_eq!(frame.frame_sequence, 0);
        assert_eq!(frame.frame_flags & FRAME_KEYFRAME as u16, 0);
    }

    #[test]
    fn bounded_keyframe_fallback_saves_the_state_actually_sent() {
        let mut view = terminal_encoding_view(5, 10);
        let guard = FrameGuard::current_for_test();
        let mut state = FrameState::new(5, 10);
        write(&mut view, &state, &guard);
        state.cells[1] = 7 << 3;
        state
            .overflow
            .insert(0, "x".repeat(view.frame_bound as usize));
        let (frame, _) = write(&mut view, &state, &guard);
        assert_ne!(frame.frame_flags & FRAME_KEYFRAME as u16, 0);
        assert!(view.frame_base.as_ref().unwrap().state.overflow.is_empty());
        // Once the optional data fits, the same cell bytes still need a patch.
        state.overflow.insert(0, "é́́".into());
        let (_, grid) = write(&mut view, &state, &guard);
        assert_eq!(grid.operations.len(), 1);
        assert_eq!(grid.components[0].kind, COMPONENT_OVERFLOW_STRINGS as u8);
    }
}
