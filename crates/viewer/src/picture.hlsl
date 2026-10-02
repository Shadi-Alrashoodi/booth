// NV12 (BT.709, limited range) to full range RGB, scaled into the picture's
// rectangle, with the bars around it in ink. The conversion is the capture
// shader's run backwards: that one computes Y from the BT.709 weights and U
// and V from the blue and red differences, so these are the same equations
// solved for R, G and B.
//
// Scaling is an area average, the capture shader's filter: an output pixel
// is the average of the source pixels under it, each counted by how much of
// it lies under that pixel. Scaled down, every source pixel counts, so one
// pixel lines of text do not flicker in and out as they would with bilinear
// sampling. Scaled up, an output pixel covers part of one source pixel or
// straddles two, so each source pixel becomes a block with a one pixel blend
// at its edges: text stays crisp instead of going soft. At one to one it is
// an exact copy. Luma and chroma are averaged on their own planes, chroma
// over half the footprint, which undoes the capture side's 2x2 average.

cbuffer Picture : register(b0)
{
    // The picture's rectangle in target pixels: left, top, right, bottom.
    float4 area;
    // The picture's size in source pixels, which may be less than the
    // texture's: decoders pad to their block size.
    float2 source_size;
    // Source pixels under one target pixel, per axis.
    float2 footprint;
    float4 ink;
};

Texture2DArray luma : register(t0);
Texture2DArray chroma : register(t1);

float4 vs_main(uint id : SV_VertexID) : SV_Position
{
    // One triangle that covers the whole viewport.
    float2 corner = float2((id << 1) & 2, id & 2);
    return float4(corner * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}

float4 area_average(Texture2DArray plane, int2 size, float2 start, float2 end)
{
    int2 first = int2(floor(start));
    int2 last = int2(ceil(end)) - 1;
    float4 sum = 0.0;
    float total = 0.0;
    [loop]
    for (int y = first.y; y <= last.y; y++)
    {
        float wy = min(end.y, y + 1.0) - max(start.y, (float)y);
        [loop]
        for (int x = first.x; x <= last.x; x++)
        {
            float w = wy * (min(end.x, x + 1.0) - max(start.x, (float)x));
            int2 at = clamp(int2(x, y), int2(0, 0), size - 1);
            sum += w * plane.Load(int4(at, 0, 0));
            total += w;
        }
    }
    return sum / max(total, 1e-6);
}

float4 ps_main(float4 position : SV_Position) : SV_Target
{
    float2 p = position.xy;
    if (any(p < area.xy) || any(p >= area.zw))
        return ink;
    float2 start = floor(p - area.xy) * footprint;
    float2 end = start + footprint;
    int2 size = int2(source_size);
    float y = area_average(luma, size, start, end).r;
    float2 uv = area_average(chroma, (size + 1) / 2, start * 0.5, end * 0.5).rg;

    float luma_full = (y * 255.0 - 16.0) / 219.0;
    float blue_diff = (uv.x * 255.0 - 128.0) / 224.0;
    float red_diff = (uv.y * 255.0 - 128.0) / 224.0;
    float b = luma_full + 1.8556 * blue_diff;
    float r = luma_full + 1.5748 * red_diff;
    float g = (luma_full - 0.2126 * r - 0.0722 * b) / 0.7152;
    return float4(saturate(float3(r, g, b)), 1.0);
}
