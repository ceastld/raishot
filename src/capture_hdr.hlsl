// HDR 桌面复制得到的 FP16 scRGB 帧按 SDR 白点换回 8 位 sRGB 截图像素。
// 修改后按 capture_hdr_shaders.rs 页首的命令重新生成字节码。
Texture2D<float4> desktop : register(t0);

cbuffer Params : register(b0)
{
    int2 source_offset;  // 源纹理坐标 = 目标像素坐标 + 偏移
    float inv_sdr_white; // 1 / (SDR 白点 nits / 80)
    float unused;
};

// 单个覆盖视口的三角形，不需要顶点缓冲。
float4 vs_main(uint id : SV_VertexID) : SV_Position
{
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}

// SDR 内容在合成时被乘以白点倍数，除回后正好落在 [0,1]；更亮的 HDR 高光截断到白。
float4 ps_main(float4 position : SV_Position) : SV_Target
{
    float3 scene = desktop.Load(int3(int2(position.xy) + source_offset, 0)).rgb;
    float3 c = saturate(scene * inv_sdr_white);
    float3 encoded = c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
    return float4(encoded, 1);
}
