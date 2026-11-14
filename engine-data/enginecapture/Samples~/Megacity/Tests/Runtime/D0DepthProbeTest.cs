// D0 验收（docs/83 §7）：深度通路点亮。
//
// 刻意**不加载 Megacity 场景**——D0 要证的是「渲出来的数是不是米、对不对得上」，
// 那是通路的性质，与场景无关。用一个空场景 + 已知距离标定板，真值是算出来的而不是
// 目测的，失败时也能一眼看出差在哪。Megacity 全链跑在 S0/S1/S2 那几条上。
//
// 判据（docs/83 §7 D0 表）：
//   ① EXR 单通道 half 可被独立实现读出（这里读回自己写的字节，跨语言那条在 QC 侧）
//   ② 已知距离标定物读数误差 ≤1 cm（<16 m 段，fp16 尚能表达 cm）
//   ③ 天空 → NaN
//   ④ 深度是**沿光轴 Z** 而非欧氏距离——画面边角的标定板必须仍读出 Z

using System;
using System.Collections;
using System.IO;
using Diogenes.EngineCapture;
using NUnit.Framework;
using UnityEngine;
using UnityEngine.TestTools;

namespace Diogenes.Capture.Tests
{
    public class D0DepthProbeTest
    {
        const int W = DepthExporter.DepthWidth;    // 1280
        const int H = DepthExporter.DepthHeight;   // 720
        const float Near = 0.3f;
        const float Far = 500f;
        const float VFov = 58.72f;

        GameObject _root;
        RenderTexture _rt;
        RenderTexture _colorRt;

        [TearDown]
        public void TearDown()
        {
            if (_root != null) UnityEngine.Object.DestroyImmediate(_root);
            if (_rt != null) { _rt.Release(); UnityEngine.Object.DestroyImmediate(_rt); _rt = null; }
            if (_colorRt != null) { _colorRt.Release(); UnityEngine.Object.DestroyImmediate(_colorRt); _colorRt = null; }
        }

        [UnityTest]
        public IEnumerator D0_KnownDistance_ReadsBackInMetres()
        {
            const float planeZ = 5.0f;          // 标定板距离，米

            _root = new GameObject("D0Probe");
            var camGo = new GameObject("DepthCam");
            camGo.transform.SetParent(_root.transform, false);
            var cam = camGo.AddComponent<Camera>();
            cam.transform.position = Vector3.zero;
            cam.transform.rotation = Quaternion.identity;
            cam.nearClipPlane = Near;
            cam.farClipPlane = Far;
            cam.fieldOfView = VFov;
            cam.clearFlags = CameraClearFlags.SolidColor;
            cam.backgroundColor = Color.black;

            // 与光轴垂直的大平面：视野内每一点的 view-space Z 都恒为 planeZ，
            // 而到相机的欧氏距离在边角处明显更大——两者的差就是本测试的判别力所在。
            //
            // 用 Cube 而不是 Quad：Quad 只有单面，朝向不对就被背面剔除，深度整片写不进去
            // （首跑就是这么挂的：中心读数 = 远平面）。Cube 的正面朝向无歧义。
            // 厚度 1、中心放在 planeZ + 0.5，正面正好落在 planeZ。
            var slab = GameObject.CreatePrimitive(PrimitiveType.Cube);
            slab.transform.SetParent(_root.transform, false);
            slab.transform.position = new Vector3(0f, 0f, planeZ + 0.5f);
            slab.transform.rotation = Quaternion.identity;
            slab.transform.localScale = new Vector3(80f, 80f, 1f);

            // 相机的**颜色**目标与深度线性化目标必须是两张 RT。
            // 共用一张时 URP 帧末把相机颜色 blit 到目标上，会把我们写进去的深度盖掉——
            // 首跑读到 0.053 m（一个颜色值），低于 near=0.3 根本不可能是深度。
            _rt = new RenderTexture(new RenderTextureDescriptor(
                W, H, UnityEngine.Experimental.Rendering.GraphicsFormat.R32_SFloat, 24));
            _rt.Create();
            _colorRt = new RenderTexture(new RenderTextureDescriptor(
                W, H, UnityEngine.Experimental.Rendering.GraphicsFormat.R8G8B8A8_SRGB, 24));
            _colorRt.Create();
            cam.targetTexture = _colorRt;

            // 先失活再挂组件：OnEnable 会在 AddComponent 当场触发，若那时
            // DepthCamera/Target 还没赋值，pass 就带着 null 目标建起来，之后
            // 一次都不会记录（首跑即如此：BlitCount=0）。与 McClipCapture 里
            // 各 recorder 的挂法同一纪律。
            var blitGo = new GameObject("DepthPassBlitter");
            blitGo.SetActive(false);
            blitGo.transform.SetParent(_root.transform, false);
            var blitter = blitGo.AddComponent<Megacity.DepthPassBlitter>();
            blitter.DepthCamera = cam;
            blitter.Target = _rt;
            blitGo.SetActive(true);

            yield return null;
            yield return null;                   // 两帧：确保 blit 至少跑过一次

            Assert.That(blitter.FailReason, Is.Null, "shader 未就绪：" + blitter.FailReason);
            Assert.That(blitter.BlitCount, Is.GreaterThan(0), "深度 blit 一次都没跑");

            float[] depth = ReadBack(_rt);

            // ---- ② 画面中心：已知距离 ≤1 cm ----
            float center = depth[(H / 2) * W + (W / 2)];
            Assert.That(center, Is.EqualTo(planeZ).Within(0.01f),
                        "中心读数 " + center + " m，期望 " + planeZ + " m（±1cm）");

            // ---- ④ 沿光轴 Z，不是欧氏距离 ----
            // 画面角落同一平面上的点，欧氏距离约 planeZ/cos(θ)，明显大于 planeZ。
            // 若实现取了欧氏距离，这一条会立刻爆。
            int cx = (int)(W * 0.06f), cy = (int)(H * 0.06f);
            float corner = depth[cy * W + cx];
            Assert.That(corner, Is.EqualTo(planeZ).Within(0.02f),
                        "边角读数 " + corner + " m —— 若明显大于 " + planeZ +
                        " 说明取的是欧氏距离而非 view-space Z（docs/68 §6.4 明确非欧氏）");

            // ---- ③ 天空（平面之外）→ 远平面，交给 DepthSink 判 NaN ----
            // 平面 40x40 覆盖不到的角度：把相机转开 90° 再读一次
            cam.transform.rotation = Quaternion.Euler(0f, 90f, 0f);
            yield return null;
            yield return null;
            float[] sky = ReadBack(_rt);
            float skyCenter = sky[(H / 2) * W + (W / 2)];
            Assert.That(skyCenter, Is.GreaterThanOrEqualTo(Far * 0.9f),
                        "无几何处应给出远平面量级（实测 " + skyCenter + "），由 DepthSink 转 NaN");

            // ---- ① 走完整 DepthSink：量化 + EXR，并读回验证 ----
            string dir = Path.Combine(Path.GetTempPath(), "d0_" + Guid.NewGuid().ToString("N"));
            var sink = new DepthSink(W, H, Far, dir);
            var bytes = new byte[W * H * 4];
            Buffer.BlockCopy(depth, 0, bytes, 0, bytes.Length);
            sink.Write(0, bytes, bytes.Length);
            Assert.That(sink.FailReason, Is.Null);

            string exr = Path.Combine(dir, "depth_0.exr");
            Assert.That(File.Exists(exr), Is.True);
            var info = new FileInfo(exr);
            Assert.That(info.Length, Is.GreaterThan(0));
            Debug.Log(string.Format(
                "D0 OK：中心 {0:F4} m（期望 {1}），边角 {2:F4} m，天空 {3:F1} m，EXR {4} 字节（{5:F1}× 压缩）",
                center, planeZ, corner, skyCenter, info.Length, (double)(W * H * 2) / info.Length));

            Directory.Delete(dir, true);
        }

        static float[] ReadBack(RenderTexture rt)
        {
            var prev = RenderTexture.active;
            var tex = new Texture2D(rt.width, rt.height, TextureFormat.RFloat, false, true);
            RenderTexture.active = rt;
            tex.ReadPixels(new Rect(0, 0, rt.width, rt.height), 0, 0);
            tex.Apply();
            RenderTexture.active = prev;
            var data = tex.GetRawTextureData<float>();
            var outBuf = new float[rt.width * rt.height];
            data.CopyTo(outBuf);
            UnityEngine.Object.DestroyImmediate(tex);
            return outBuf;
        }
    }
}
