// docs/68 §6.4 要的是 **view-space linear depth**：相机坐标系下沿光轴的 Z 距离，
// 不是到相机的欧氏距离。
//
// 两条刻意的选择：
// 1) 源深度由 RenderGraph 的 blit 通道以 `_BlitTexture` 绑进来，**不读全局
//    `_CameraDepthTexture`**。实测（Megacity/URP 17.1）在 `endCameraRendering` 时
//    该全局已被换成 4×4 占位纹理——图内资源出了 render graph 就失效，
//    照着采样会得到一整片远平面而且看起来"像深度图"，极难察觉。
// 2) 线性化用显式传入的 near/far，**不依赖 `_ZBufferParams`**——blit 通道执行时
//    该全局未必仍是本相机的值。反 Z 分支按 UNITY_REVERSED_Z 走：
//      反 Z（Metal/D3D/Vulkan）：raw=1 在近平面、0 在远平面
//        eye = near*far / (near + raw*(far-near))
//      正 Z：eye = near*far / (far - raw*(far-near))
//    取反会得到一张"远近颠倒但仍像深度图"的图，只有跨模态反投影自检抓得到
//    （docs/83 §5.3）。

Shader "Diogenes/LinearEyeDepth"
{
    SubShader
    {
        Tags { "RenderPipeline" = "UniversalPipeline" }
        ZTest Always ZWrite Off Cull Off

        Pass
        {
            Name "LinearEyeDepth"

            HLSLPROGRAM
            #pragma vertex Vert
            #pragma fragment Frag

            #include "Packages/com.unity.render-pipelines.universal/ShaderLibrary/Core.hlsl"
            #include "Packages/com.unity.render-pipelines.core/Runtime/Utilities/Blit.hlsl"

            float _DiogenesNear;
            float _DiogenesFar;

            float4 Frag(Varyings input) : SV_Target
            {
                UNITY_SETUP_STEREO_EYE_INDEX_POST_VERTEX(input);
                float raw = SAMPLE_TEXTURE2D_X(_BlitTexture, sampler_PointClamp,
                                               input.texcoord).r;

                float n = _DiogenesNear;
                float f = _DiogenesFar;
                float eye;
            #if UNITY_REVERSED_Z
                eye = (n * f) / max(1e-6, n + raw * (f - n));
            #else
                eye = (n * f) / max(1e-6, f - raw * (f - n));
            #endif
                return float4(eye, 0.0, 0.0, 1.0);
            }
            ENDHLSL
        }
    }

    Fallback Off
}
