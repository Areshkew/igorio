Texture2D desktop : register(t0);
Texture2D cursor_black : register(t1);
Texture2D cursor_white : register(t2);
SamplerState linear_clamp : register(s0);

cbuffer Frame : register(b0) {
    float4 crop;   // origin and size, normalized to the input texture
    float4 cursor; // origin and size in the same coordinates
};

struct Vertex {
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

Vertex vs(uint id : SV_VertexID) {
    Vertex v;
    v.uv = float2((id << 1) & 2, id & 2);
    v.position = float4(v.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return v;
}

float4 ps(Vertex v) : SV_Target {
    float2 uv = crop.xy + v.uv * crop.zw;
    float3 color = desktop.Sample(linear_clamp, uv).rgb;
    if (cursor.z > 0 && cursor.w > 0) {
        float2 pos = (uv - cursor.xy) / cursor.zw;
        if (all(pos >= 0) && all(pos < 1)) {
            float3 black = cursor_black.Sample(linear_clamp, pos).rgb;
            float3 white = cursor_white.Sample(linear_clamp, pos).rgb;
            // Evaluate the cursor against the desktop. Black/white also represent
            // the two endpoints for legacy cursor pixels that invert the background.
            color = lerp(black, white, color);
        }
    }
    return float4(color, 1);
}
