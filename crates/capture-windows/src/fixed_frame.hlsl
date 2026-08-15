cbuffer SourceRegion : register(b0)
{
    float4 source_region;
};

struct VertexOutput
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

VertexOutput vs_main(uint vertex_id : SV_VertexID)
{
    static const float2 positions[6] = {
        float2(-1.0,  1.0),
        float2( 1.0,  1.0),
        float2(-1.0, -1.0),
        float2(-1.0, -1.0),
        float2( 1.0,  1.0),
        float2( 1.0, -1.0),
    };
    static const float2 uvs[6] = {
        float2(0.0, 0.0),
        float2(1.0, 0.0),
        float2(0.0, 1.0),
        float2(0.0, 1.0),
        float2(1.0, 0.0),
        float2(1.0, 1.0),
    };

    VertexOutput output;
    output.position = float4(positions[vertex_id], 0.0, 1.0);
    output.uv = source_region.xy + uvs[vertex_id] * source_region.zw;
    return output;
}

Texture2D<float4> source_texture : register(t0);
SamplerState source_sampler : register(s0);

float4 ps_main(VertexOutput input) : SV_Target
{
    float4 color = source_texture.Sample(source_sampler, input.uv);
    color.a = 1.0;
    return color;
}
