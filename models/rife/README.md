# RIFE for frames in between

`../rife-v4.6-flow.onnx` is the network the fork's smooth slow motion runs
(see `src/crates/concat-vision/src/tween.rs`): Practical-RIFE's v4.6 flow
network (https://github.com/hzwer/Practical-RIFE, MIT licence), with the
weights `flownet_v4.6.pkl` as vs-rife publishes them
(https://github.com/HolyWu/vs-rife/releases/download/model/flownet_v4.6.pkl,
MIT licence), exported to ONNX by `export.py` here:

    python -I export.py flownet_v4.6.pkl rife-v4.6-flow.onnx 1.0 flow

It takes `img0` and `img1`, float32 `[1, 3, H, W]` RGB in `0..1` with `H`
and `W` multiples of 32, and `timestep`, float32 `[1, 1, 1, 1]`, and answers
`flow`, `[1, 4, H, W]` - where each pixel of the moment was in `img0` (x, y)
and in `img1`, in pixels - and `mask`, `[1, 1, H, W]`, the share of `img0`
before its sigmoid. The app moves and mixes the pictures itself, at their
own size.
