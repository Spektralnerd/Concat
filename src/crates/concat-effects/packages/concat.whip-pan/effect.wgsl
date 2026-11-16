// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

// Whip Pan: the camera flicks from one shot to the next, whichever way the
// angle points. Side by side, the shots sit next to each other along the
// flick and the frame is dragged across them; mirrored, each shot runs on
// in mirror images of itself, the camera races out through the one and in
// through the other, and the swap hides where the streak is thickest. The
// drag is packed into a shorter stretch of the cut the higher the speed,
// and smeared along its way in proportion to how fast it moves - a long
// streak at the flick, sharp at the ends.
//
// At its defaults - to the right, side by side, the speed's own streak -
// it draws what it drew before it could turn.

struct Params { speed: f32, angle: f32, style: f32, blur: f32 }

// The streak's samples at the speed's own length; a longer one takes more,
// so it stays a smear rather than a row of copies.
const TAPS: f32 = 24.0;
const MOST_TAPS: f32 = 64.0;

// The pan's position, nought to one, at `p`: still, a flick, still.
fn pan(p: f32, window: f32) -> f32 {
    let q = clamp((p - 0.5) / window + 0.5, 0.0, 1.0);
    return q * q * (3.0 - 2.0 * q);
}

// A point folded back into the picture, as a mirror at each edge would,
// so a picture moved past its frame has no empty corners.
fn fold(uv: vec2<f32>) -> vec2<f32> {
    let m = abs(uv - 2.0 * floor(uv * 0.5 + vec2<f32>(0.5)));
    return clamp(m, vec2<f32>(0.0), vec2<f32>(1.0));
}

// The whole of the camera's move, in the layer's fractions: the angle is
// read on the screen, so a diagonal is one on a wide frame too, and the
// move is as long as it takes the next shot to cover the frame - one width
// to the side, one height up or down, and on a slant the reach of both.
fn travel() -> vec2<f32> {
    let turn = radians(params.angle);
    // Up on the screen is towards a smaller v.
    let screen = vec2<f32>(cos(turn), -sin(turn));
    let way = normalize(screen / max(frame.size, vec2<f32>(1.0)));
    return way * (abs(way.x) + abs(way.y));
}

// Premultiplied, so the streak's mean weighs a clear pixel as nothing.
fn weighed(c: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(c.rgb * c.a, c.a);
}

// The camera at `cam` along a move of `way`, looking at `uv`.
//
// Side by side, the next shot lies a whole move along: a point past the
// line halfway between the two pictures' middles is in it. Mirrored, the
// camera is in the outgoing shot's mirror field until half way, and in the
// incoming one's from then on, arriving at its picture unmoved.
fn seen(uv: vec2<f32>, cam: f32, way: vec2<f32>, mirrored: bool) -> vec4<f32> {
    if (mirrored) {
        if (cam < 0.5) {
            return weighed(from_at(fold(uv + cam * way)));
        }
        return weighed(to_at(fold(uv + (cam - 1.0) * way)));
    }
    let at = uv + cam * way;
    let reach = length(way);
    if (dot(at - vec2<f32>(0.5), way / reach) > 0.5 * reach) {
        return weighed(to_at(fold(at - way)));
    }
    return weighed(from_at(fold(at)));
}

fn transition(uv: vec2<f32>, progress: f32) -> vec4<f32> {
    let p = clamp(progress, 0.0, 1.0);
    let speed = clamp(params.speed, 20.0, 100.0) / 100.0;
    let blur = clamp(params.blur, 0.0, 300.0) / 100.0;
    let mirrored = params.style > 0.5;
    let way = travel();
    // The flick takes from nine tenths of the cut at 20% to a quarter at full.
    let window = mix(1.06, 0.25, speed);
    let t = pan(p, window);
    // How far the pan moves in a frame of a one-second cut, here.
    let q = clamp((p - 0.5) / window + 0.5, 0.0, 1.0);
    let rate = 6.0 * q * (1.0 - q) / window;
    // The streak, as a share of the whole move.
    let smear = min(rate / 30.0 * 1.6, 0.6) * blur;
    let taps = i32(clamp(ceil(TAPS * max(blur, 1.0)), TAPS, MOST_TAPS));
    var gathered = vec4<f32>(0.0);
    for (var k = 0; k < taps; k++) {
        let off = (f32(k) / f32(taps - 1) - 0.5) * smear;
        gathered += seen(uv, t + off, way, mirrored);
    }
    let mean = gathered / f32(taps);
    return vec4<f32>(mean.rgb / max(mean.a, 1e-6), mean.a);
}
