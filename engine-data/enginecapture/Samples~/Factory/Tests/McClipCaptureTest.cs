// Factory 版采集入口——类名保持 McClipCaptureTest 以复用
// engine-data/capture-orchestrator/megacity_capture.py 的 -testFilter 通道。
// 环境变量接口与 Megacity 版一致，另加：
//   DIOGENES_SCENE_PATH  要加载的场景资产路径（如 Assets/FactoryTraining/Scenes/xxx.unity）；
//                        为空则在测试空场景里跑（管线冒烟用）
//   DIOGENES_YAW         数值（缺省 0；工厂场景无「走廊自动对齐」概念，auto 视为 0）
//   DIOGENES_SCENE_ASSET_ID / _GAME_NAME / _LICENSE_ID / _LICENSE / _ATTRIBUTION
//                        场景身份与许可，由编排器 `--scene-id` 从
//                        engine-data/scenes/manifest.json 的登记注入；
//                        许可两项缺省为空 ⇒ Run 拒绝出片（docs/106）
using System;
using System.Collections;
using System.IO;
using Diogenes.Capture.Factory;
using NUnit.Framework;
using UnityEngine;
using UnityEngine.SceneManagement;
using UnityEngine.TestTools;
using Debug = UnityEngine.Debug;

namespace Diogenes.Capture.Factory.Tests
{
    public class McClipCaptureTest
    {
        static string Env(string key, string fallback)
        {
            string v = Environment.GetEnvironmentVariable(key);
            return string.IsNullOrEmpty(v) ? fallback : v;
        }

        /// <summary>宿主场景的人物/载具预制体（给了就用真模型，没给就程序化拼装）。</summary>
        static GameObject FindPrefab(string assetPath)
        {
#if UNITY_EDITOR
            if (string.IsNullOrEmpty(assetPath)) return null;
            var go = UnityEditor.AssetDatabase.LoadAssetAtPath<GameObject>(assetPath);
            if (go == null) Debug.LogWarning("[factory-capture] prefab not found: " + assetPath);
            return go;
#else
            return null;
#endif
        }

        [UnityTest]
        [Timeout(7200000)]
        public IEnumerator Mc_CaptureClip_FromPlan()
        {
            string planDir = Env("DIOGENES_PLAN_DIR", "");
            string outDir = Env("DIOGENES_OUT_DIR",
                "/Users/example/Documents/diogenes-capture-runs/factory-clips");
            string clipName = Env("DIOGENES_CLIP_NAME", "");
            string[] off = Env("DIOGENES_WORLD_OFFSET", "0,1.6,0").Split(',');
            var worldOffset = new Vector3(
                float.Parse(off[0]), float.Parse(off[1]), float.Parse(off[2]));
            bool render = Env("DIOGENES_RENDER", "1") != "0";
            bool audioOn = Env("DIOGENES_AUDIO", "1") != "0";
            int warmup = int.Parse(Env("DIOGENES_WARMUP", "60"));
            string yawEnv = Env("DIOGENES_YAW", "0");
            float yaw = yawEnv == "auto" ? 0f : float.Parse(yawEnv);
            string scenePath = Env("DIOGENES_SCENE_PATH", "");

            Assert.IsTrue(File.Exists(Path.Combine(planDir, "plan.json")),
                "plan dir invalid: " + planDir);
            Directory.CreateDirectory(outDir);

            // 宿主场景的 gameplay 脚本会打自己的 Error log（找不到存档、依赖被关掉的主相机…），
            // 与采集正确性无关，但 Unity 测试默认把任何 Error log 判为失败。
            // 本条 clip 的判定一律来自构造自检（LaneFramesVerified）与 engine_qc，不看日志。
            LogAssert.ignoreFailingMessages = true;

            if (!string.IsNullOrEmpty(scenePath))
            {
#if UNITY_EDITOR
                var load = UnityEditor.SceneManagement.EditorSceneManager.LoadSceneInPlayMode(
                    scenePath, new LoadSceneParameters(LoadSceneMode.Single));
                float deadline = Time.realtimeSinceStartup + 600f;
                while (!load.isLoaded && Time.realtimeSinceStartup < deadline)
                    yield return null;
                Assert.IsTrue(load.isLoaded && load.IsValid(),
                    "scene load failed: " + scenePath);
                Debug.Log("[factory-capture] scene loaded: " + scenePath);
                // 场景自带相机/监听器会与采集 rig 冲突，全部关掉
                foreach (var cam in UnityEngine.Object.FindObjectsByType<Camera>(
                             FindObjectsSortMode.None))
                    cam.enabled = false;
                foreach (var lis in UnityEngine.Object.FindObjectsByType<AudioListener>(
                             FindObjectsSortMode.None))
                    lis.enabled = false;

                // 宿主 gameplay 剥离（同 Megacity 剥 Netcode 的思路）：训练交互/UI 脚本
                // 依赖被我们关掉的主相机，留着会每帧 NRE 并把测试判失败；几何/光照/音频保留。
                // 每个新宿主场景填一次，属场景 onboarding 的一部分。
                string kill = Env("DIOGENES_DISABLE_ROOTS", "");
                if (!string.IsNullOrEmpty(kill))
                {
                    var names = new System.Collections.Generic.HashSet<string>(
                        kill.Split(','), StringComparer.OrdinalIgnoreCase);
                    int n = 0;
                    foreach (var root in load.GetRootGameObjects())
                    {
                        if (!names.Contains(root.name)) continue;
                        root.SetActive(false);
                        n++;
                    }
                    Debug.Log("[factory-capture] disabled " + n + " gameplay roots: " + kill);
                }
#else
                Assert.Fail("scene load only supported in editor test run");
#endif
            }

            // 动态物体：宿主场景静态时由 NpcCrowd 铺设（docs/70 §4.3）。
            // DIOGENES_NPC=0 关闭；DIOGENES_NPC_HUMANOID / _VEHICLE 调数量；
            // DIOGENES_GROUND_Y 给地面高度（人脚要落地，相机高度不等于地面）。
            NpcCrowdConfig crowd = null;
            if (Env("DIOGENES_NPC", "1") != "0")
            {
                crowd = new NpcCrowdConfig
                {
                    HumanoidCount = int.Parse(Env("DIOGENES_NPC_HUMANOID", "16")),
                    VehicleCount = int.Parse(Env("DIOGENES_NPC_VEHICLE", "5")),
                    GroundY = float.Parse(Env("DIOGENES_GROUND_Y", "0")),
                    Seed = int.Parse(Env("DIOGENES_NPC_SEED", "1")),
                };
            }

            var o = new FactoryCaptureOptions
            {
                PlanDir = planDir,
                OutDir = outDir,
                ClipName = clipName,
                WorldOffset = worldOffset,
                WorldYawDeg = yaw,
                RenderVideo = render,
                CaptureAudio = audioOn,
                WarmupTicks = warmup,
                Crowd = crowd,
                CrowdHumanoidPrefab = FindPrefab(Env("DIOGENES_NPC_HUMANOID_PREFAB", "")),
                CrowdVehiclePrefab = FindPrefab(Env("DIOGENES_NPC_VEHICLE_PREFAB", "")),
                SceneFile = Env("DIOGENES_SCENE_FILE", ""),
                // 宿主场景名必须随工程改——它进 systeminfo.gameProcessName，
                // 交付形态审计会拿它与上传路径的 game 段比对（实测：从 Factory
                // 拷来的采集层默认值没跟着改，warehouse 批 10/10 判 BAD）。
                GameProcessName = Env("DIOGENES_GAME_NAME", "FactoryTraining"),
                SceneAssetId = Env("DIOGENES_SCENE_ASSET_ID", "factory-training@assetstore-344832"),
                // 许可标记**没有缺省值**（docs/106）：由 scenes/manifest.json 的场景登记
                // 经编排器 --scene-id 注入。上面两项当初就是靠「记得手工导环境变量」，
                // warehouse-v1/v2 没导对就把 Robotics-Warehouse 的数据标成了 Factory；
                // 许可字段不重蹈覆辙——缺了让 Run 拒绝出片，而不是沿用上个场景的许可。
                LicenseId = Env("DIOGENES_LICENSE_ID", ""),
                License = Env("DIOGENES_LICENSE", ""),
                Attribution = Env("DIOGENES_ATTRIBUTION", ""),
            };
            var r = new FactoryCaptureResult();
            yield return FactoryClipCapture.Run(o, r);

            Debug.Log("[factory-capture] ok=" + r.Ok + " err=" + r.Error
                + " clipDir=" + r.ClipDir
                + " laneFramesVerified=" + r.LaneFramesVerified
                + " avgTickMs=" + r.AvgTickMs.ToString("F1")
                + " audio=" + r.AudioCaptured + "/" + r.AudioMuxed);
            Assert.IsTrue(r.Ok, "capture failed: " + r.Error);
            Assert.Greater(r.LaneFramesVerified, 0, "no lane frames verified");
        }
    }
}
