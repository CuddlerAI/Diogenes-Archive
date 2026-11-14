// 深度通路的引擎侧接线（docs/83 缺口 D1）。
//
// 走过的弯路值得留着：初版用 `RenderPipelineManager.endCameraRendering` +
// `Graphics.Blit` 采样全局 `_CameraDepthTexture`，图省事、且在 URP 14 语义下成立。
// 在 Megacity（URP 17.1，RenderGraph 默认开）上实测**不成立**——诊断打点显示该时刻
// 全局已被换成 **4×4 占位纹理**，blit 出来是一整片远平面。危险之处在于它不报错，
// 产物看着仍"像深度图"，只有拿已知距离标定或跨模态反投影才认得出来。
//
// 现在走 RenderGraph 的正规通道：在 `beginCameraRendering` 把一个 pass
// `EnqueuePass` 进本相机的 ScriptableRenderer，pass 在图内拿
// `UniversalResourceData.cameraDepthTexture`（此时才是真的），blit 进我方导入的 RT。
// EnqueuePass 这条路不需要改 URP Renderer 资产，采集工程零配置即可用。

using UnityEngine;
using UnityEngine.Rendering;
using UnityEngine.Rendering.RenderGraphModule;
using UnityEngine.Rendering.RenderGraphModule.Util;
using UnityEngine.Rendering.Universal;

namespace Diogenes.Capture.Megacity
{
    /// <summary>
    /// 把深度相机的场景深度线性化成米，写进该路的 RFloat RT。
    /// 挂上并设好 <see cref="DepthCamera"/> / <see cref="Target"/> 即生效。
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class DepthPassBlitter : MonoBehaviour
    {
        public const string ShaderName = "Diogenes/LinearEyeDepth";

        static readonly int NearId = Shader.PropertyToID("_DiogenesNear");
        static readonly int FarId = Shader.PropertyToID("_DiogenesFar");

        [SerializeField] Camera _depthCamera;
        [SerializeField] RenderTexture _target;

        Material _mat;
        LinearizePass _pass;
        RTHandle _targetHandle;
        bool _hooked;

        public Camera DepthCamera { get => _depthCamera; set => _depthCamera = value; }
        public RenderTexture Target { get => _target; set => _target = value; }

        /// <summary>本 clip 内 blit 实际执行过的次数——「没跑却当跑了」是这条链最贵的失效模式。</summary>
        public int BlitCount => _pass != null ? _pass.Executed : 0;

        public string FailReason { get; private set; }

        void OnEnable()
        {
            var shader = Shader.Find(ShaderName);
            if (shader == null)
            {
                FailReason = "找不到 shader " + ShaderName + "（未随工程导入 / 未进 Always Included Shaders）";
                Debug.LogError("DepthPassBlitter: " + FailReason);
                return;
            }
            _mat = new Material(shader) { hideFlags = HideFlags.HideAndDontSave };

            if (_depthCamera != null)
            {
                var data = _depthCamera.GetUniversalAdditionalCameraData();
                if (data != null) data.requiresDepthTexture = true;   // 不要，URP 可能整帧不产深度
            }
            if (_target != null)
                _targetHandle = RTHandles.Alloc(_target, name: "DiogenesDepthLinear");

            var info = new RenderTargetInfo
            {
                width = _target != null ? _target.width : 0,
                height = _target != null ? _target.height : 0,
                volumeDepth = 1,
                msaaSamples = 1,
                format = _target != null ? _target.graphicsFormat
                                         : UnityEngine.Experimental.Rendering.GraphicsFormat.R32_SFloat,
                bindMS = false,
            };
            _pass = new LinearizePass(_mat, _targetHandle, info)
            {
                renderPassEvent = RenderPassEvent.AfterRenderingTransparents
            };

            RenderPipelineManager.beginCameraRendering += OnBeginCameraRendering;
            _hooked = true;
        }

        void OnDisable()
        {
            if (_hooked)
            {
                RenderPipelineManager.beginCameraRendering -= OnBeginCameraRendering;
                _hooked = false;
            }
            _pass = null;
            if (_targetHandle != null) { _targetHandle.Release(); _targetHandle = null; }
            if (_mat != null) { DestroyImmediate(_mat); _mat = null; }
        }

        void OnBeginCameraRendering(ScriptableRenderContext ctx, Camera cam)
        {
            if (_pass == null || cam != _depthCamera) return;
            _mat.SetFloat(NearId, cam.nearClipPlane);
            _mat.SetFloat(FarId, cam.farClipPlane);
            var data = cam.GetUniversalAdditionalCameraData();
            if (data == null || data.scriptableRenderer == null) return;
            data.scriptableRenderer.EnqueuePass(_pass);
        }

        sealed class LinearizePass : ScriptableRenderPass
        {
            readonly Material _mat;
            readonly RTHandle _dst;
            readonly RenderTargetInfo _dstInfo;

            public int Executed;

            public LinearizePass(Material mat, RTHandle dst, RenderTargetInfo dstInfo)
            {
                _mat = mat;
                _dst = dst;
                _dstInfo = dstInfo;
                // 只读深度，不需要读也不需要写颜色
                requiresIntermediateTexture = false;
            }

            class PassData
            {
                public Material Mat;
                public TextureHandle Src;
            }

            public override void RecordRenderGraph(RenderGraph renderGraph, ContextContainer frameData)
            {
                if (_mat == null || _dst == null) return;
                var resources = frameData.Get<UniversalResourceData>();
                TextureHandle src = resources.cameraDepthTexture;
                if (!src.IsValid()) return;

                // 必须给 RenderTargetInfo：RTHandles.Alloc(RenderTexture) 包出来的句柄
                // 对 render graph 是个不透明的 RenderTargetIdentifier，图无从知道它的
                // 尺寸/格式，单参 ImportTexture 会直接抛异常。
                TextureHandle dst = renderGraph.ImportTexture(_dst, _dstInfo);
                if (!dst.IsValid()) return;

                // 用显式 raster pass 而不是 AddBlitPass：后者要向目标句柄要
                // descriptor，而**导入**的句柄只有 RenderTargetInfo、没有 descriptor，
                // 会抛 "does not have a valid descriptor"。
                using (var builder = renderGraph.AddRasterRenderPass<PassData>(
                           "Diogenes LinearEyeDepth", out var data))
                {
                    data.Mat = _mat;
                    data.Src = src;
                    builder.UseTexture(src, AccessFlags.Read);
                    builder.SetRenderAttachment(dst, 0, AccessFlags.Write);
                    builder.AllowPassCulling(false);   // 目标在图外，图看不出它被用了
                    builder.SetRenderFunc((PassData d, RasterGraphContext ctx) =>
                        Blitter.BlitTexture(ctx.cmd, d.Src, new Vector4(1f, 1f, 0f, 0f), d.Mat, 0));
                }
                Executed++;
            }
        }
    }
}
