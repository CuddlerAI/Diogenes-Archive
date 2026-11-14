// docs/78 辩题十一（裁决：LocalSimulation 采集世界）：自建剥离 Netcode 的采集世界。
// 世界只装 Default|LocalSimulation|Streaming 过滤命中的系统——ClientSimulation-only /
// ServerSimulation 系统（netcode、HUD、输入）天然被排除，车流/飞艇等
// ClientSimulation|LocalSimulation 标注的系统照常运行。

using System;
using System.Collections.Generic;
using System.Text;
using Unity.Entities;
using Unity.MegacityMetro.Traffic;
using Unity.Scenes;
using UnityEngine;
using Hash128 = Unity.Entities.Hash128;

namespace Diogenes.Capture.Megacity
{
    /// <summary>Builds and tears down the Diogenes capture world for Megacity Metro.</summary>
    public static class CaptureWorldBuilder
    {
        /// <summary>Capture subscene set (docs/78 §2 辩题十一 + Traffic.unity 勘察补充:
        /// 路网/车流 authoring 全部在 Traffic 子场景，不载它 TrafficSetupSystem 无路可铺).</summary>
        public static readonly (string Name, string Guid)[] CaptureSubScenes =
        {
            ("Level", "ed1a49ee1f7b28b499c8cece71ee2353"),
            ("Traffic", "46dffb08de5f4cf498cabf1a709740e0"),
            ("Blimps", "d15a274585a786440ad97fbd9f40d43a"),
            ("Common", "3326447b997a6814bab701262e9f6f38"),
            ("MegacityMetroLevelBounds", "f98b38b047e44c14ab1e89f0c3d96b12"),
        };

        public const string AudioSubSceneGuid = "5510665dddcbf4a4bb1f3d3f71689c64";

        /// <summary>Create the LocalSimulation capture world and hook it into the player loop.
        /// Pass appendToPlayerLoop=false for editor-driven worlds that are pumped manually
        /// with world.Update() (SceneProbeExporter batchmode -executeMethod path).</summary>
        public static World CreateWorld(string name = "DiogenesCaptureWorld",
                                        bool appendToPlayerLoop = true)
        {
            var world = new World(name, WorldFlags.Game);
            var systems = DefaultWorldInitialization.GetAllSystems(
                WorldSystemFilterFlags.Default
                | WorldSystemFilterFlags.LocalSimulation
                | WorldSystemFilterFlags.Streaming);
            DefaultWorldInitialization.AddSystemsToRootLevelSystemGroups(world, systems);
            if (appendToPlayerLoop)
                ScriptBehaviourUpdateOrder.AppendWorldToCurrentPlayerLoop(world);
            if (World.DefaultGameObjectInjectionWorld == null || !World.DefaultGameObjectInjectionWorld.IsCreated)
                World.DefaultGameObjectInjectionWorld = world;
            return world;
        }

        /// <summary>Kick off async loads for the capture subscene set. BlockOnImport makes
        /// the editor bake entity scenes synchronously on first run.</summary>
        public static List<(string Name, Entity SceneEntity)> LoadCaptureSubScenes(
            World world, bool includeAudio)
        {
            var loads = new List<(string, Entity)>();
            foreach (var (name, guid) in CaptureSubScenes)
                loads.Add((name, Load(world, guid)));
            if (includeAudio)
                loads.Add(("Audio_Subscene", Load(world, AudioSubSceneGuid)));
            return loads;
        }

        static Entity Load(World world, string guid)
        {
            return SceneSystem.LoadSceneAsync(world.Unmanaged, new Hash128(guid),
                new SceneSystem.LoadParameters
                {
                    Flags = SceneLoadFlags.BlockOnImport
                });
        }

        public static bool AllScenesLoaded(World world, List<(string Name, Entity SceneEntity)> loads)
        {
            foreach (var (_, e) in loads)
                if (!SceneSystem.IsSceneLoaded(world.Unmanaged, e))
                    return false;
            return true;
        }

        public static string SceneLoadStatus(World world, List<(string Name, Entity SceneEntity)> loads)
        {
            var sb = new StringBuilder();
            foreach (var (name, e) in loads)
                sb.Append(name).Append('=')
                  .Append(SceneSystem.IsSceneLoaded(world.Unmanaged, e) ? "loaded" : "pending")
                  .Append(' ');
            return sb.ToString();
        }

        /// <summary>TrafficEnabled 置位（docs/78 §4.2 步骤 1）。Traffic 子场景的
        /// TrafficConfigAuthoring 默认 enableTraffic=1，正常烘焙后单例已存在——
        /// 这里只在缺失时补桩并如实返回来源。</summary>
        public static string EnsureTrafficEnabled(World world)
        {
            var em = world.EntityManager;
            using (var q = em.CreateEntityQuery(ComponentType.ReadOnly<TrafficEnabled>()))
            {
                if (!q.IsEmptyIgnoreFilter) return "baked";
            }
            var e = em.CreateEntity();
            em.AddComponent<TrafficEnabled>(e);
            return "stubbed";
        }

        /// <summary>车流密度干预（docs/78 R'6）第一步：在世界首次 update 前挂起
        /// TrafficSetupSystem，避免它用烘焙默认间距（30–200m，全城仅 ~1500 台）先行铺车。</summary>
        public static void HoldTrafficSetup(World world)
        {
            var h = world.Unmanaged.GetExistingUnmanagedSystem(typeof(TrafficSetupSystem));
            if (h != SystemHandle.Null)
                world.Unmanaged.ResolveSystemStateRef(h).Enabled = false;
        }

        /// <summary>车流密度干预第二步（子场景加载完成后调用）：改写 TrafficConfig 的
        /// 车距区间再放行 TrafficSetupSystem。返回实际写入的配置描述。</summary>
        public static string ReleaseTrafficSetup(World world, float minDist, float maxDist)
        {
            var em = world.EntityManager;
            string desc;
            using (var q = em.CreateEntityQuery(ComponentType.ReadWrite<TrafficConfig>()))
            {
                if (q.IsEmptyIgnoreFilter)
                {
                    desc = "no TrafficConfig singleton";
                }
                else
                {
                    var e = q.GetSingletonEntity();
                    var cfg = em.GetComponentData<TrafficConfig>(e);
                    cfg.MinDistanceBetweenVehicles = minDist;
                    cfg.MaxDistanceBetweenVehicles = maxDist;
                    em.SetComponentData(e, cfg);
                    desc = "spacing=" + minDist + "-" + maxDist + "m";
                }
            }
            var h = world.Unmanaged.GetExistingUnmanagedSystem(typeof(TrafficSetupSystem));
            if (h != SystemHandle.Null)
                world.Unmanaged.ResolveSystemStateRef(h).Enabled = true;
            return desc;
        }

        /// <summary>离给定世界坐标最近的路网折线段（XZ 距离）：水平方位角（度，Unity
        /// atan2(x,z) 口径）+ 段中点。用于把计划的主导视轴对齐车流走廊并把机位高度
        /// 吸附到走廊高度（DynObjectProbe 的内容前提）。找不到路网时返回 null。</summary>
        public static (float AzimuthDeg, Vector3 Midpoint)? NearestRoadSegment(
            World world, Vector3 worldPos)
        {
            var em = world.EntityManager;
            float bestDist = float.MaxValue;
            float bestAz = 0f;
            Vector3 bestMid = default;
            bool found = false;
            using (var q = em.CreateEntityQuery(
                ComponentType.ReadOnly<RoadPoint>(), ComponentType.ReadOnly<RoadOffset>()))
            using (var ents = q.ToEntityArray(Unity.Collections.Allocator.Temp))
            {
                for (int e = 0; e < ents.Length; e++)
                {
                    var off = em.GetComponentData<RoadOffset>(ents[e]).Value;
                    var buf = em.GetBuffer<RoadPoint>(ents[e], true);
                    for (int i = 0; i + 1 < buf.Length; i++)
                    {
                        var a = buf[i].Value + off;
                        var b = buf[i + 1].Value + off;
                        var mid = (a + b) * 0.5f;
                        float dx = mid.x - worldPos.x, dz = mid.z - worldPos.z;
                        float d2 = dx * dx + dz * dz;
                        if (d2 < bestDist)
                        {
                            bestDist = d2;
                            bestAz = Mathf.Atan2(b.x - a.x, b.z - a.z) * Mathf.Rad2Deg;
                            bestMid = new Vector3(mid.x, mid.y, mid.z);
                            found = true;
                        }
                    }
                }
            }
            return found ? (bestAz, bestMid) : ((float, Vector3)?)null;
        }

        public static int CountEntities<T>(World world) where T : unmanaged, IComponentData
        {
            var em = world.EntityManager;
            using (var q = em.CreateEntityQuery(ComponentType.ReadOnly<T>()))
            {
                return q.CalculateEntityCount();
            }
        }

        /// <summary>Dispose the world and detach it from the player loop.</summary>
        public static void DestroyWorld(World world)
        {
            if (world == null || !world.IsCreated) return;
            ScriptBehaviourUpdateOrder.RemoveWorldFromCurrentPlayerLoop(world);
            world.Dispose();
        }
    }
}
