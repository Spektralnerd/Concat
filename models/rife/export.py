# Exports a RIFE v4.6 flow network (Practical-RIFE's IFNet_HDv3 v4.6
# architecture, MIT) to one ONNX graph:
#   img0, img1: float32 [1,3,H,W] RGB in 0..1, H and W multiples of 32
#   timestep:   float32 [1,1,1,1], 0..1
#   output:     float32 [1,3,H,W]
# Usage: python -I export.py WEIGHTS.pkl OUT.onnx [scale]
import sys

import torch
import torch.nn as nn
import torch.nn.functional as F


def conv(cin, cout, k=3, stride=1, pad=1):
    return nn.Sequential(nn.Conv2d(cin, cout, k, stride, pad, bias=True), nn.LeakyReLU(0.2, True))


class ResConv(nn.Module):
    def __init__(self, c):
        super().__init__()
        self.conv = nn.Conv2d(c, c, 3, 1, 1)
        self.beta = nn.Parameter(torch.ones((1, c, 1, 1)))
        self.relu = nn.LeakyReLU(0.2, True)

    def forward(self, x):
        return self.relu(self.conv(x) * self.beta + x)


class IFBlock(nn.Module):
    def __init__(self, cin, c):
        super().__init__()
        self.conv0 = nn.Sequential(conv(cin, c // 2, 3, 2, 1), conv(c // 2, c, 3, 2, 1))
        self.convblock = nn.Sequential(*[ResConv(c) for _ in range(8)])
        self.lastconv = nn.Sequential(nn.ConvTranspose2d(c, 4 * 6, 4, 2, 1), nn.PixelShuffle(2))

    def forward(self, x, flow, scale):
        x = F.interpolate(x, scale_factor=1.0 / scale, mode="bilinear")
        if flow is not None:
            flow = F.interpolate(flow, scale_factor=1.0 / scale, mode="bilinear") / scale
            x = torch.cat((x, flow), 1)
        feat = self.convblock(self.conv0(x))
        tmp = F.interpolate(self.lastconv(feat), scale_factor=scale, mode="bilinear")
        return tmp[:, :4] * scale, tmp[:, 4:5]


def warp(img, flow, div, grid):
    flow = torch.cat([flow[:, 0:1] / div[0], flow[:, 1:2] / div[1]], 1)
    g = (grid + flow).permute(0, 2, 3, 1)
    return F.grid_sample(img, g, mode="bilinear", padding_mode="border", align_corners=True)


class IFNet(nn.Module):
    def __init__(self, scale=1.0, flow_only=False):
        super().__init__()
        self.block0 = IFBlock(7, 192)
        self.block1 = IFBlock(8 + 4, 128)
        self.block2 = IFBlock(8 + 4, 96)
        self.block3 = IFBlock(8 + 4, 64)
        self.scales = [8 / scale, 4 / scale, 2 / scale, 1 / scale]
        self.flow_only = flow_only

    def forward(self, img0, img1, timestep):
        n, _, h, w = img0.shape
        img0 = img0.clamp(0.0, 1.0)
        img1 = img1.clamp(0.0, 1.0)
        t = timestep.expand(n, 1, h, w)
        div = torch.stack([(w - 1.0) / 2.0 * torch.ones(()), (h - 1.0) / 2.0 * torch.ones(())])
        xs = torch.linspace(-1.0, 1.0, w).view(1, 1, 1, w).expand(n, 1, h, w)
        ys = torch.linspace(-1.0, 1.0, h).view(1, 1, h, 1).expand(n, 1, h, w)
        grid = torch.cat([xs, ys], 1)
        blocks = [self.block0, self.block1, self.block2, self.block3]
        flow = mask = None
        w0, w1 = img0, img1
        for i in range(4):
            if flow is None:
                flow, mask = blocks[i](torch.cat((img0, img1, t), 1), None, self.scales[i])
            else:
                f0, m0 = blocks[i](torch.cat((w0, w1, t, mask), 1), flow, self.scales[i])
                flow = flow + f0
                mask = mask + m0
            w0 = warp(img0, flow[:, :2], div, grid)
            w1 = warp(img1, flow[:, 2:4], div, grid)
        if self.flow_only:
            # The motion from the moment to each frame, in pixels, and the
            # weight of the first frame before its sigmoid: the caller warps
            # and blends, at whatever size the pictures are.
            return flow, mask
        m = torch.sigmoid(mask)
        return (w0 * m + w1 * (1 - m)).clamp(0.0, 1.0)


def main():
    weights, out = sys.argv[1], sys.argv[2]
    scale = float(sys.argv[3]) if len(sys.argv) > 3 else 1.0
    flow_only = len(sys.argv) > 4 and sys.argv[4] == "flow"
    sd = torch.load(weights, map_location="cpu", weights_only=True)
    sd = {k.replace("module.", ""): v for k, v in sd.items() if "module." in k}
    net = IFNet(scale, flow_only).eval()
    missing, unexpected = net.load_state_dict(sd, strict=False)
    print("missing", missing, "unexpected", unexpected)
    x0, x1 = torch.rand(1, 3, 256, 448), torch.rand(1, 3, 256, 448)
    t = torch.full((1, 1, 1, 1), 0.5)
    torch.onnx.export(
        net, (x0, x1, t), out,
        input_names=["img0", "img1", "timestep"],
        output_names=["flow", "mask"] if flow_only else ["output"],
        dynamic_axes={"img0": {2: "h", 3: "w"}, "img1": {2: "h", 3: "w"},
                      **({"flow": {2: "h", 3: "w"}, "mask": {2: "h", 3: "w"}} if flow_only else {"output": {2: "h", 3: "w"}})},
        opset_version=17, dynamo=False,
    )
    import numpy as np
    import onnxruntime as ort

    session = ort.InferenceSession(out, providers=["CPUExecutionProvider"])
    for shape in [(256, 448), (384, 640)]:
        a, b = torch.rand(1, 3, *shape), torch.rand(1, 3, *shape)
        with torch.no_grad():
            ref = net(a, b, t)
        ref = ref[0].numpy() if flow_only else ref.numpy()
        got = session.run(None, {"img0": a.numpy(), "img1": b.numpy(), "timestep": t.numpy()})[0]
        print(shape, "max diff vs torch", float(np.abs(got - ref).max()))


if __name__ == "__main__":
    main()
