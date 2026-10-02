// BGRA to NV12: BT.709 matrix, limited range (Y 16 to 235, U and V 16 to
// 240). Two passes over one NV12 texture: the luma pass writes the Y plane
// through an R8_UNORM view, the chroma pass the UV plane at half size
// through an R8G8_UNORM view.
//
// Scaling is an area average (a box filter the size of the output pixel):
// every source pixel under an output pixel counts by how much of it lies
// under that pixel. At 1.5x, 3840 to 2560, each output pixel covers one and
// a half source pixels each way, so every source pixel lands in one or two
// output pixels and none is skipped. Bilinear sampling reads only the four
// pixels around the sample point and misses the rest, which makes one pixel
// lines of text come and go as they scroll.
//
// Chroma is computed from the area of its whole 2x2 block of luma pixels,
// which is the average of those four pixels' colours.

cbuffer Plane : register(b0)
{
    // The duplication image as Windows hands it, before rotation.
    uint2 source_size;
    // The same image turned upright.
    uint2 upright_size;
    // Upright source pixels under one pixel of this plane, per axis.
    float2 footprint;
    // Quarter turns clockwise from the duplication image to upright.
    uint quarter_turns;
    uint unused;
};

Texture2D<float4> source : register(t0);

float4 vs_main(uint id : SV_VertexID) : SV_Position
{
    // One triangle that covers the whole target.
    float2 corner = float2((id << 1) & 2, id & 2);
    return float4(corner * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}

float3 load_upright(int2 upright)
{
    // Past the edge (an odd size padded to even) repeats the edge pixel.
    upright = clamp(upright, int2(0, 0), int2(upright_size) - 1);
    int2 last = int2(source_size) - 1;
    int2 at = upright;
    if (quarter_turns == 1)
        at = int2(upright.y, last.y - upright.x);
    else if (quarter_turns == 2)
        at = last - upright;
    else if (quarter_turns == 3)
        at = int2(last.x - upright.y, upright.x);
    return source.Load(int3(at, 0)).rgb;
}

float3 area_average(float2 position)
{
    float2 start = floor(position) * footprint;
    float2 end = start + footprint;
    int2 first = int2(floor(start));
    int2 last = int2(ceil(end)) - 1;
    float3 sum = 0.0;
    float total = 0.0;
    [loop]
    for (int y = first.y; y <= last.y; y++)
    {
        float wy = min(end.y, y + 1.0) - max(start.y, (float)y);
        [loop]
        for (int x = first.x; x <= last.x; x++)
        {
            float w = wy * (min(end.x, x + 1.0) - max(start.x, (float)x));
            sum += w * load_upright(int2(x, y));
            total += w;
        }
    }
    return sum / total;
}

static const float3 luma_weights = float3(0.2126, 0.7152, 0.0722);

float ps_luma(float4 position : SV_Position) : SV_Target
{
    float y = dot(area_average(position.xy), luma_weights);
    return (16.0 + 219.0 * y) / 255.0;
}

float2 ps_chroma(float4 position : SV_Position) : SV_Target
{
    float3 rgb = area_average(position.xy);
    float y = dot(rgb, luma_weights);
    float2 uv = float2((rgb.b - y) / 1.8556, (rgb.r - y) / 1.5748);
    return (128.0 + 224.0 * uv) / 255.0;
}
