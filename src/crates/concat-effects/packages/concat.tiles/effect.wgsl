// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

// Tiles: the picture repeated side by side, a wall of screens. Each tile
// holds the picture as large as it must be to fill it - a slice in a row
// of tall tiles, the whole picture in a grid as many down as across - and
// Zoom takes it nearer or further from there; Center picks the point of it
// in each tile's middle, for a face that is not in the middle of the shot:
// -50 to 50 across, left to right, and up the picture. The tiles can come
// one at a time, in an order: on the beat from the clip's start, or as far
// as Shown says, which a keyframe can move. Where no tile is yet, between
// tiles and past a picture zoomed out smaller than its tile, it is black
// or clear.
//
// Freeze, building on the beat, leaves each tile at the moment the next
// one came: the host plans the wall as the clip and, over it, one copy per
// tile already left behind - the clip at that tile's moment, `frame.part`
// its place in the order, the first being 1 - which draws that tile alone
// and nothing else. The clip itself then carries minus the number of
// copies and leaves their tiles clear, so each tile is drawn once and a
// fade or a blend mode treats a frozen tile as it does the live one.
// Without copies - an effect card, a probe - it draws every tile live.
// The shader never sees the switch; the host reads it.

struct Params {
    across: f32,
    down: f32,
    zoom: f32,
    shift_x: f32,
    shift_y: f32,
    shown: f32,
    build: f32,
    bpm: f32,
    freeze: f32,
    order: f32,
    gap: f32,
    empty: f32,
}

const MOST_ACROSS: i32 = 12;
const MOST_DOWN: i32 = 8;

// What a tile is sorted by when the order is not plain reading order:
// its distance from the middle of the frame, in pixels, or a draw.
fn sort_key(x: i32, y: i32, cols: i32, rows: i32, order: i32) -> f32 {
    if (order == 2) {
        let tile = frame.size / vec2<f32>(f32(cols), f32(rows));
        let off = vec2<f32>(f32(x) + 0.5 - f32(cols) * 0.5, f32(y) + 0.5 - f32(rows) * 0.5);
        return length(off * tile);
    }
    return hash(vec2<f32>(f32(x), f32(y)), 7.0);
}

// When tile (`x`, `y`) comes, counted in tiles: 0 with the first. Reading
// order, its mirror, or by key - from the middle, those as far out as each
// other come together; at random, one at a time.
fn rank(x: i32, y: i32, cols: i32, rows: i32) -> i32 {
    let order = i32(round(params.order));
    if (order == 0) {
        return y * cols + x;
    }
    if (order == 1) {
        return y * cols + (cols - 1 - x);
    }
    let mine = sort_key(x, y, cols, rows, order);
    var before = 0;
    for (var j = 0; j < rows; j++) {
        for (var i = 0; i < cols; i++) {
            if (sort_key(i, j, cols, rows, order) < mine - 1e-3) {
                before++;
            }
        }
    }
    return before;
}

// How many tiles are there so far: as many as Shown lets through, and,
// building on the beat, one more on each step from the clip's start.
fn showing(count: i32) -> i32 {
    var most = i32(ceil(clamp(params.shown, 0.0, 100.0) / 100.0 * f32(count) - 1e-4));
    let build = i32(round(params.build));
    if (build > 0) {
        let per_beat = select(select(1.0, 2.0, build == 2), 4.0, build == 3);
        let step = 60.0 / clamp(params.bpm, 40.0, 240.0) / per_beat;
        let built = i32(floor(max(frame.clip_time, 0.0) / step + 1e-4)) + 1;
        most = min(most, built);
    }
    return most;
}

// Whether this draw is a frozen tile's copy, which shows only its tile.
fn copy() -> bool {
    return frame.part > 0.5;
}

// How many tiles, first in the order, copies draw over the clip itself:
// zero unless the host made copies.
fn frozen() -> i32 {
    return i32(round(max(-frame.part, 0.0)));
}

// What is drawn where there is no tile: black or clear, and always clear
// in a frozen tile's copy, which must not hide the wall beneath it.
fn nothing() -> vec4<f32> {
    let clear = copy() || round(params.empty) == 1.0;
    return select(vec4<f32>(0.0, 0.0, 0.0, 1.0), vec4<f32>(0.0), clear);
}

fn effect(uv: vec2<f32>) -> vec4<f32> {
    let cols = clamp(i32(round(params.across)), 1, MOST_ACROSS);
    let rows = clamp(i32(round(params.down)), 1, MOST_DOWN);
    let grid = vec2<f32>(f32(cols), f32(rows));
    let cell = min(floor(uv * grid), grid - vec2<f32>(1.0));
    let x = i32(cell.x);
    let y = i32(cell.y);
    let place = rank(x, y, cols, rows);
    if (copy()) {
        if (place != i32(round(frame.part)) - 1) {
            return nothing();
        }
    } else if (place < frozen()) {
        // A copy draws this tile, frozen.
        return vec4<f32>(0.0);
    } else if (place >= max(showing(cols * rows), frozen() + 1)) {
        // The newest tile always: the host counted it there, and a count
        // a hair apart from the host's must not leave a hole for a frame.
        return nothing();
    }

    // Where in its tile the pixel is, in pixels from the tile's middle.
    let tile = frame.size / grid;
    let inside = (uv * grid - cell - vec2<f32>(0.5)) * tile;
    let margin = clamp(params.gap, 0.0, 20.0) / 100.0 * min(tile.x, tile.y) * 0.5;
    if (any(abs(inside) > tile * 0.5 - vec2<f32>(margin))) {
        return nothing();
    }

    // The picture scaled to cover its tile, then by Zoom, with the point
    // Center names - a share of the picture from its middle, up positive -
    // on the tile's middle.
    let cover = max(1.0 / grid.x, 1.0 / grid.y);
    let scale = cover * clamp(params.zoom, 25.0, 400.0) / 100.0;
    let center = clamp(vec2<f32>(params.shift_x, -params.shift_y), vec2<f32>(-50.0), vec2<f32>(50.0)) / 100.0;
    let at = vec2<f32>(0.5) + center + inside / (scale * frame.size);
    if (any(at < vec2<f32>(0.0)) || any(at > vec2<f32>(1.0))) {
        return nothing();
    }
    return sample(at);
}
