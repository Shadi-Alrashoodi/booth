// The sharer's pointer, one textured rectangle. How it mixes with the
// picture under it is up to the blend state the pass is drawn with: plain
// alpha for colour pointers; for monochrome and masked ones a multiply by
// the AND mask and then an exclusion with the XOR image, which for black
// and white is exactly what Windows' AND-then-XOR does, inverting the
// screen where both bits are set.

cbuffer Quad : register(b0)
{
    // The rectangle in target pixels: left, top, right, bottom.
    float4 rect;
    float2 target_size;
    float2 unused;
};

Texture2D shape : register(t0);
SamplerState smooth : register(s0);

struct Corner
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

Corner vs_main(uint id : SV_VertexID)
{
    // A strip of four: top left, top right, bottom left, bottom right.
    float2 corner = float2(id & 1, id >> 1);
    float2 pixel = lerp(rect.xy, rect.zw, corner);
    Corner output;
    output.position = float4(pixel / target_size * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    output.uv = corner;
    return output;
}

float4 ps_main(Corner input) : SV_Target
{
    return shape.Sample(smooth, input.uv);
}
