static const float4 k_vertex[6] = {
    // 1---2 4
    // |  / /|
    // | / / |
    // |/ /  |
    // 0 3---5
    float4(-1.0, -1.0, 0.0, 1.0),
    float4(-1.0,  1.0, 0.0, 0.0),
    float4( 1.0,  1.0, 1.0, 0.0),
    float4(-1.0, -1.0, 0.0, 1.0),
    float4( 1.0,  1.0, 1.0, 0.0),
    float4( 1.0, -1.0, 1.0, 1.0),
};

void vs_main(
    const uint id   : SV_VertexID,
    out float4 o_pos: SV_Position,
    out float2 o_uv : TEXCOORD0) {
    o_pos = float4(k_vertex[id].xy, 0.0, 1.0);
    o_uv = k_vertex[id].zw;
}

Texture2D    g_texture: register(t0);
SamplerState g_sampler: register(s0);

float4 ps_main(
    const float4 pos: SV_Position,
    const float2 uv: TEXCOORD0): SV_Target {
    return float4(g_texture.Sample(g_sampler, uv).xyz, 1.0);
}
