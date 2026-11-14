// 普通 GameObject 场景的动态物体铺设器（docs/70 §4.3「NavMesh 人群/车辆铺设器」的
// 无-NavMesh 实装）+ IDynObjectSource 实现。
//
// 结构性保证入画：agent 不是在场景里乱走，而是绕**计划锚点区**（plan.json 的 anchors，
// 即三路相机的共视注视区）做环带游走，锚点随时间移动，人群跟着走 —— 这与 docs/78 §8
// 「锚点区落在车流走廊」的结构性保证同源，区别是那边搬相机就车流、这边搬人群就相机。
//
// 运动分层按 docs/69：普通走 / 跑 / 大幅度动作（原地转身+挥臂）三档，配额可配。
// 穿模防护：移动前向射线预检，命中则该帧反射方向（工厂室内有墙）。
//
// 视觉体：优先用宿主场景提供的预制体；取不到则程序化拼装（胶囊+盒子的工人/AGV），
// 与 Megacity 版 CarrierVehicleSpawner 的降级思路一致 —— 画面里必须真有东西，
// 不能只让几何投影断言过。

using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using Diogenes.EngineCapture;
using UnityEngine;
using UnityEngine.AI;

namespace Diogenes.Capture.Factory
{
    public sealed class NpcCrowdConfig
    {
        public int HumanoidCount = 16;
        public int VehicleCount = 5;
        /// <summary>运动分层配额（docs/69：普通走 30%、跑 20–30%、其余大幅度动作）。</summary>
        public float WalkFrac = 0.35f;
        public float RunFrac = 0.25f;
        public float WalkSpeed = 1.4f;
        public float RunSpeed = 3.4f;
        /// <summary>环带半径（米，绝对值）。锚点半径只有几米，直接按它缩放会把人群堆在
        /// 镜头前几米内、把场景挡死（首版实测）；宿主场景尺度差异大，故用绝对区间。</summary>
        public float RingRadiusMin = 6f;
        public float RingRadiusMax = 22f;
        public float VehicleRingRadius = 14f;
        public int Seed = 1;
        /// <summary>地面高度（世界系）的**兜底值**。真实落地逐位置查 NavMesh——
        /// 单值 GroundY 在有高低差的场景（装卸月台、不同标高的车间地坪）会让 agent
        /// 悬浮或陷地：目检实测 AGV 轮子离地约半米。</summary>
        public float GroundY = 0f;
        /// <summary>NavMesh 采样半径（m）。超出则回落 GroundY。</summary>
        public float GroundSampleRadius = 1.0f;
        /// <summary>离最近可站面的最大容许水平距离（m）。地坪层是从渲染体 AABB 提的，
        /// 块间有缝；超出此距离仍无地才判「真的没有地面」。</summary>
        public float NearGroundReach = 8f;
        public float ObstacleProbeDistance = 1.2f;
        public LayerMask ObstacleMask = ~0;
        /// <summary>agent 之间的最小中心距（m）。占据图只含**静态场景几何**，
        /// 我方注入的 agent 相互之间零碰撞——首版实测 NPC 粘连成团、身体嵌进 AGV、
        /// 车与车互穿，按 docs/69 §8.1「出现大面积穿模」是硬打回项。</summary>
        public float AgentSeparation = 2.2f;
        /// <summary>agent 与任一相机的最小水平距离（m），防「贴脸遮挡」（§8.1 打回项）。</summary>
        public float CameraKeepout = 3.5f;
        /// <summary>单帧最大推挤位移（m），避免解算把 agent 弹飞造成画面跳变。</summary>
        public float MaxPushPerTick = 0.12f;
        /// <summary>推挤解算迭代次数。</summary>
        public int SeparationIterations = 3;
    }

    /// <summary>绕锚点区游走的人群/载具，同时充当 DynObjectProbe 的数据源。</summary>
    public sealed class NpcCrowd : MonoBehaviour, IDynObjectSource
    {
        enum Gait { Walk, Run, Gesture }

        sealed class Agent
        {
            public Transform Root;
            public Transform ArmL, ArmR, LegL, LegR;
            public float Theta;       // 绕锚点的方位角
            public float Radius;      // 绕锚点的距离
            public float AngularDir;  // ±1
            public float Speed;
            public Gait Gait;
            public float Phase;
            public float BoundRadius;
            public float Height;
            public bool IsVehicle;
            public int BlockedTicks;
            public Vector3 LastGood;
            public Vector3 GroundCacheAt;
            public float GroundCacheY;
            public bool GroundCacheValid;
        }

        readonly List<Agent> _agents = new List<Agent>();
        readonly List<Vector4> _anchors = new List<Vector4>(); // xyz=pos, w=t
        readonly List<float> _anchorRadius = new List<float>();
        readonly List<Bounds> _obstacles = new List<Bounds>();   // 世界系占据盒
        readonly List<Bounds> _ground = new List<Bounds>();      // 世界系可站面（地坪/月台/平台）
        NpcCrowdConfig _cfg;
        FrameClock _clock;
        float _time;

        public FrameClock Clock { get => _clock; set => _clock = value; }
        /// <summary>相机 rig（用于 CameraKeepout；不给则跳过该项）。</summary>
        public MultiCamRig Rig { get; set; }
        public int SpawnedCount => _agents.Count;
        /// <summary>解算统计（诚实入账：进 provenance）。</summary>
        public int SeparationPushes { get; private set; }
        public int CameraPushes { get; private set; }
        /// <summary>落地时找不到可站面的次数（诚实入账：悬浮残余风险的量化）。</summary>
        public int UngroundedTicks { get; private set; }
        public int HumanoidSpawned { get; private set; }
        public int VehicleSpawned { get; private set; }
        /// <summary>实际运动分层计数，用于诚实入账（provenance / 报告）。</summary>
        public int WalkCount { get; private set; }
        public int RunCount { get; private set; }
        public int GestureCount { get; private set; }

        void OnEnable() { if (_clock != null) _clock.OnTick += HandleTick; }
        void OnDisable() { if (_clock != null) _clock.OnTick -= HandleTick; }

        /// <summary>从 plan.json 读锚点时间序列（世界系 = 局部 + offset，并按 yaw 旋转）。</summary>
        public void LoadAnchors(string planDir, Vector3 worldOffset, float worldYawDeg)
        {
            _anchors.Clear();
            _anchorRadius.Clear();
            var plan = MiniJson.ParseFile(Path.Combine(planDir, "plan.json"))
                as Dictionary<string, object>;
            if (plan == null || !plan.TryGetValue("anchors", out var av)
                || !(av is List<object> list)) return;

            var rot = Quaternion.Euler(0f, worldYawDeg, 0f);
            foreach (var o in list)
            {
                if (!(o is Dictionary<string, object> d)) continue;
                if (!d.TryGetValue("p", out var pv) || !(pv is List<object> p) || p.Count < 3) continue;
                var local = new Vector3(ToF(p[0]), ToF(p[1]), ToF(p[2]));
                Vector3 w = rot * local + worldOffset;
                float t = d.TryGetValue("t", out var tv) ? ToF(tv) : 0f;
                _anchors.Add(new Vector4(w.x, w.y, w.z, t));
                _anchorRadius.Add(d.TryGetValue("radius", out var rv) ? ToF(rv) : 3f);
            }
        }

        static float ToF(object o) => o is double d ? (float)d : 0f;

        /// <summary>
        /// 载入场景占据图（SceneOccupancyExporter 产物）作为穿模判据。
        /// **射线预检在 collider 稀疏的场景等于没有**——Factory Training 12711 个
        /// renderer 只有 98 个静态 collider（docs/71 R11），`Physics.Raycast` 打不中货架，
        /// agent 会径直穿过去。占据图来自可见几何，是同一批相机轨迹用的那张图。
        /// </summary>
        public void LoadOccupancy(string sceneFilePath)
        {
            _obstacles.Clear();
            _ground.Clear();
            if (string.IsNullOrEmpty(sceneFilePath) || !File.Exists(sceneFilePath)) return;
            if (!(MiniJson.ParseFile(sceneFilePath) is Dictionary<string, object> d)) return;

            var off = Vector3.zero;
            if (d.TryGetValue("world_offset", out var wo) && wo is List<object> w && w.Count == 3)
                off = new Vector3(ToF(w[0]), ToF(w[1]), ToF(w[2]));
            if (!d.TryGetValue("boxes", out var bv) || !(bv is List<object> boxes)) return;

            foreach (var o in boxes)
            {
                if (!(o is Dictionary<string, object> bd)) continue;
                if (!bd.TryGetValue("lo", out var lv) || !(lv is List<object> l) || l.Count < 3) continue;
                if (!bd.TryGetValue("hi", out var hv) || !(hv is List<object> h) || h.Count < 3) continue;
                var min = new Vector3(ToF(l[0]), ToF(l[1]), ToF(l[2])) + off;
                var max = new Vector3(ToF(h[0]), ToF(h[1]), ToF(h[2])) + off;
                var b = new Bounds();
                b.SetMinMax(min, max);
                _obstacles.Add(b);
            }
            if (d.TryGetValue("ground_boxes", out var gv) && gv is List<object> gb)
                foreach (var o in gb)
                {
                    if (!(o is Dictionary<string, object> bd)) continue;
                    if (!bd.TryGetValue("lo", out var lv) || !(lv is List<object> l) || l.Count < 3) continue;
                    if (!bd.TryGetValue("hi", out var hv) || !(hv is List<object> h) || h.Count < 3) continue;
                    var bb = new Bounds();
                    bb.SetMinMax(new Vector3(ToF(l[0]), ToF(l[1]), ToF(l[2])) + off,
                                 new Vector3(ToF(h[0]), ToF(h[1]), ToF(h[2])) + off);
                    _ground.Add(bb);
                }
            Debug.Log("[Diogenes] NpcCrowd occupancy: " + _obstacles.Count + " boxes, ground "
                + _ground.Count + " boxes");
        }

        /// <summary>
        /// 逐位置的落地高度：查 NavMesh（可行走面的权威真值），查不到回落单值。
        /// 单值地面在 84×110m、含装卸月台与多标高地坪的场景里必然出错——
        /// 目检实测 AGV 悬浮约半米、NPC 站在几何板上方（docs/69 §8.1 打回样式）。
        /// </summary>
        float GroundAt(Vector3 p, float fallback) => TryGround(p, out float y) ? y : fallback;

        /// <summary>
        /// 该 xz 是否落在真实可行走面上，以及落地高度。
        ///
        /// **不在 NavMesh 上就不是「地面更低」，而是根本没有地面**——装卸月台外侧、
        /// 坑口、夹层边缘都是这种。单值 GroundY 会把 agent 钉在车间地坪高度、
        /// 悬在月台外的空中：目检实测「NPC 站在护栏/窗台高度、脚下无地面」
        /// （docs/69 §8.1 打回样式，与复核「站在悬浮几何板顶上」是同一现象）。
        /// 故此处返回 false 时调用方应把该点判为不可站，而不是落到兜底高度。
        /// </summary>
        bool TryGround(Vector3 p, out float y)
        {
            // ① 可站面层（导出器从场景几何里挑出的地坪/月台/平台顶）。
            // 命中就用它的顶面；没直接命中则**向外找最近的可站面**，只要在
            // NearGroundReach 之内就允许站在那个高度——地坪层是从渲染体
            // AABB 提的，块与块之间有缝，硬判「必须正落在块内」会把大量本来
            // 合法的位置判死（trajgen 侧同款约束把计划成功率从 7/8 打到 1/8，
            // 已回退：这是 NPC 侧该解的问题，不该由排产背）。
            // 超出该距离仍无地 ⇒ 真的没有地面（月台外侧、坑口），判不可站。
            float best = float.NegativeInfinity;
            float ceiling = (_cfg?.GroundY ?? 0f) + 1.2f;   // 低月台/台阶可站；3m 夹层排除
            float reach = _cfg?.NearGroundReach ?? 3f;
            float bestDist = float.MaxValue;
            for (int i = 0; i < _ground.Count; i++)
            {
                var b = _ground[i];
                if (b.max.y > ceiling) continue;
                float dx = Mathf.Max(b.min.x - p.x, 0f, p.x - b.max.x);
                float dz = Mathf.Max(b.min.z - p.z, 0f, p.z - b.max.z);
                float d = Mathf.Sqrt(dx * dx + dz * dz);
                if (d > reach) continue;
                // 直接命中（d==0）优先；否则取最近的
                if (d < bestDist || (Mathf.Approximately(d, bestDist) && b.max.y > best))
                {
                    bestDist = d;
                    best = b.max.y;
                }
            }
            if (best > float.NegativeInfinity) { y = best; return true; }

            // ② NavMesh 兜底（更准但常只覆盖玩法区）
            if (NavMesh.SamplePosition(new Vector3(p.x, p.y + 2f, p.z), out var hit,
                                       _cfg?.GroundSampleRadius ?? 1.0f, NavMesh.AllAreas))
            {
                y = hit.position.y;
                return true;
            }
            // 都没命中：用兜底高度并计数。**不硬判不可站**——实测硬判会让
            // agent 到不了锚点区，四路里出现整条 active_frac=0.000 的
            // （本场景可站面覆盖 62% 且块间有缝，与排产的锚点选择结构性不匹配；
            // 在 trajgen 侧加锚点可站约束则把计划成功率从 7/8 打到 1/8，已回退）。
            // 残余风险：距任何可站面 >NearGroundReach 的位置仍可能悬浮，
            // 计数写进 provenance，目检按它重点看。
            y = _cfg?.GroundY ?? 0f;
            UngroundedTicks++;
            return true;
        }

        /// <summary>水平面上该点是否撞进障碍（按 agent 半径外扩；y 取 agent 身体带）。</summary>
        bool Blocked(Vector3 groundPos, float radius, float bodyTop)
        {
            if (_obstacles.Count == 0) return false;
            var lo = new Vector3(groundPos.x - radius, groundPos.y + 0.15f, groundPos.z - radius);
            var hi = new Vector3(groundPos.x + radius, groundPos.y + bodyTop, groundPos.z + radius);
            for (int i = 0; i < _obstacles.Count; i++)
            {
                var b = _obstacles[i];
                if (b.min.x <= hi.x && b.max.x >= lo.x
                    && b.min.y <= hi.y && b.max.y >= lo.y
                    && b.min.z <= hi.z && b.max.z >= lo.z)
                    return true;
            }
            return false;
        }

        /// <summary>
        /// 环带半径按水平视场缩放。内参纵向等效 ⇒ `tan(hfov/2)=(W/H)·12/f`，水平视场只随
        /// 宽高比与焦距变：9:16@24mm 只有 31°，16:9 是 83°。半径不跟着缩，竖屏条上的
        /// agent 大半在幅外 —— 实测 batch-001 里 3:4 / 9:16 两条 dyn.active_frac
        /// 0.76 / 0.65 双双打回，横屏条却全 ≥0.94。与 docs/71 §10.1 缺陷 8
        /// 「共视扇区从没为竖屏重调」是同一个公式的两处应用。
        /// </summary>
        public static float FovRadiusScale(string aspect, float meanFocalMm)
        {
            float wh = AspectRatio(aspect);
            float f = meanFocalMm > 1f ? meanFocalMm : 24f;
            const float Ref = (16f / 9f) * 12f / 24f;      // 16:9 @ 24mm
            return Mathf.Clamp((wh * 12f / f) / Ref, 0.3f, 1.5f);
        }

        static float AspectRatio(string aspect)
        {
            if (string.IsNullOrEmpty(aspect)) return 16f / 9f;
            var parts = aspect.Split(':');
            if (parts.Length == 2
                && float.TryParse(parts[0], out float w) && float.TryParse(parts[1], out float h)
                && h > 0f)
                return w / h;
            return 16f / 9f;
        }

        public void Build(NpcCrowdConfig cfg, GameObject humanoidPrefab, GameObject vehiclePrefab)
        {
            _cfg = cfg ?? new NpcCrowdConfig();
            var rng = new System.Random(_cfg.Seed);

            for (int i = 0; i < _cfg.HumanoidCount; i++)
                _agents.Add(MakeAgent(rng, false, humanoidPrefab));
            for (int i = 0; i < _cfg.VehicleCount; i++)
                _agents.Add(MakeAgent(rng, true, vehiclePrefab));

            HumanoidSpawned = _cfg.HumanoidCount;
            VehicleSpawned = _cfg.VehicleCount;

            // 初始落位：环带半径可能超出可行走面（本场景 NavMesh 仅约 1024 m²），
            // 逐个内收直到站得住——否则首帧起就有 agent 悬在月台外，
            // 靠运行时掉头要好几十 tick 才收敛，而预热只有 30 tick。
            Vector3 a0 = AnchorAt(0f, out _);
            foreach (var ag in _agents)
            {
                for (int k = 0; k < 24; k++)
                {
                    var probe = a0 + new Vector3(Mathf.Cos(ag.Theta), 0f, Mathf.Sin(ag.Theta)) * ag.Radius;
                    TryGround(probe, out float py0);
                    if (!(_obstacles.Count > 0
                            && Blocked(new Vector3(probe.x, py0, probe.z), ag.BoundRadius, ag.Height)))
                        break;
                    ag.Radius *= 0.88f;
                    if (ag.Radius < 2.5f) { ag.Radius = 2.5f + k * 0.4f; ag.Theta += 0.7f; }
                }
            }
            ApplyPositions(0f);
        }

        Agent MakeAgent(System.Random rng, bool vehicle, GameObject prefab)
        {
            float u = (float)rng.NextDouble();
            var gait = vehicle ? Gait.Walk
                : (u < _cfg.WalkFrac ? Gait.Walk
                   : u < _cfg.WalkFrac + _cfg.RunFrac ? Gait.Run : Gait.Gesture);
            // 运动分层配额是 docs/69 对「人」的要求；AGV 强制 Walk 只是运动模式复用，
            // 计进 walk 档会让 provenance 里的分层账目虚高——只按人形计。
            if (!vehicle)
            {
                if (gait == Gait.Walk) WalkCount++;
                else if (gait == Gait.Run) RunCount++;
                else GestureCount++;
            }

            float radius = vehicle
                ? _cfg.VehicleRingRadius * (0.75f + (float)rng.NextDouble() * 0.6f)
                : Mathf.Lerp(_cfg.RingRadiusMin, _cfg.RingRadiusMax, (float)rng.NextDouble());

            var a = new Agent
            {
                Theta = (float)(rng.NextDouble() * Math.PI * 2),
                Radius = Mathf.Max(2.5f, radius),
                AngularDir = rng.NextDouble() < 0.5 ? -1f : 1f,
                Gait = gait,
                Phase = (float)(rng.NextDouble() * Math.PI * 2),
                IsVehicle = vehicle,
                Speed = vehicle ? 2.0f + (float)rng.NextDouble() * 1.5f
                     : gait == Gait.Run ? _cfg.RunSpeed
                     : gait == Gait.Walk ? _cfg.WalkSpeed
                     : 0.25f,
                Height = vehicle ? 1.2f : 1.75f,
            };
            a.BoundRadius = vehicle ? 1.6f : 0.85f;
            a.Root = prefab != null
                ? Instantiate(prefab, transform).transform
                : Procedural(a, rng);
            a.Root.name = (vehicle ? "npc_agv_" : "npc_worker_") + _agents.Count;
            if (prefab != null && !vehicle) BindLimbs(a);
            return a;
        }

        /// <summary>宿主预制体也尽量找四肢来做程序化摆动（找不到就静态模型走位，仍是动态物体）。</summary>
        void BindLimbs(Agent a)
        {
            foreach (var t in a.Root.GetComponentsInChildren<Transform>())
            {
                string n = t.name.ToLowerInvariant();
                if (a.ArmL == null && n.Contains("arm") && (n.Contains("l") || n.Contains("left"))) a.ArmL = t;
                else if (a.ArmR == null && n.Contains("arm") && (n.Contains("r") || n.Contains("right"))) a.ArmR = t;
                else if (a.LegL == null && n.Contains("leg") && (n.Contains("l") || n.Contains("left"))) a.LegL = t;
                else if (a.LegR == null && n.Contains("leg") && (n.Contains("r") || n.Contains("right"))) a.LegR = t;
            }
        }

        Transform Procedural(Agent a, System.Random rng)
        {
            var root = new GameObject("npc").transform;
            root.SetParent(transform, false);
            var mat = ProceduralMaterial(a.IsVehicle
                ? new Color(0.85f, 0.55f, 0.05f)                                  // AGV 工业橙
                : new Color(0.95f, 0.72f + (float)rng.NextDouble() * 0.2f, 0.1f)); // 高可视工装

            if (a.IsVehicle)
            {
                Box(root, mat, new Vector3(0f, 0.45f, 0f), new Vector3(2.2f, 0.5f, 1.4f));
                Box(root, mat, new Vector3(0f, 0.95f, -0.3f), new Vector3(1.0f, 0.6f, 1.0f));
                var dark = ProceduralMaterial(new Color(0.12f, 0.12f, 0.13f));
                foreach (var (x, z) in new[] { (0.8f, 0.6f), (0.8f, -0.6f), (-0.8f, 0.6f), (-0.8f, -0.6f) })
                    Box(root, dark, new Vector3(x, 0.22f, z), new Vector3(0.35f, 0.42f, 0.22f));
                return root;
            }

            var skin = ProceduralMaterial(new Color(0.82f, 0.65f, 0.52f));
            var pants = ProceduralMaterial(new Color(0.16f, 0.2f, 0.32f));
            var helmet = ProceduralMaterial(new Color(0.95f, 0.85f, 0.1f));
            var boots = ProceduralMaterial(new Color(0.09f, 0.09f, 0.1f));

            // 人体比例（1.75m）：腿 0.85 / 躯干 0.62 / 头颈 0.28
            Capsule(root, mat, new Vector3(0f, 1.18f, 0f), new Vector3(0.50f, 0.34f, 0.30f)); // 躯干
            Capsule(root, skin, new Vector3(0f, 1.58f, 0f), new Vector3(0.21f, 0.13f, 0.21f)); // 颈+脸
            Capsule(root, helmet, new Vector3(0f, 1.68f, 0f), new Vector3(0.26f, 0.13f, 0.26f)); // 安全帽

            // 四肢挂在**关节 pivot** 上（肩/髋），视觉体向下偏移半个长度——
            // 直接旋转视觉体会绕它自己的中心转，手臂会横在身前（首版实测的翻车样式）。
            a.ArmL = Limb(root, mat, new Vector3(-0.30f, 1.42f, 0f), 0.60f, 0.13f);
            a.ArmR = Limb(root, mat, new Vector3(0.30f, 1.42f, 0f), 0.60f, 0.13f);
            a.LegL = Limb(root, pants, new Vector3(-0.13f, 0.88f, 0f), 0.80f, 0.17f);
            a.LegR = Limb(root, pants, new Vector3(0.13f, 0.88f, 0f), 0.80f, 0.17f);
            // 靴子挂在髋 pivot 下，随腿一起摆；尺寸是绝对值（pivot 不缩放）
            Box(a.LegL, boots, new Vector3(0f, -0.79f, 0.05f), new Vector3(0.19f, 0.10f, 0.29f));
            Box(a.LegR, boots, new Vector3(0f, -0.79f, 0.05f), new Vector3(0.19f, 0.10f, 0.29f));
            return root;
        }

        /// <summary>关节 pivot（空物体）+ 向下偏移的视觉体。返回 pivot——绕它转才是绕肩/髋转。</summary>
        static Transform Limb(Transform parent, Material m, Vector3 joint, float length, float thick)
        {
            var pivot = new GameObject("joint").transform;
            pivot.SetParent(parent, false);
            pivot.localPosition = joint;
            var vis = Box(pivot, m, new Vector3(0f, -length * 0.5f, 0f),
                new Vector3(thick, length, thick));
            vis.name = "limb";
            return pivot;
        }

        static Material ProceduralMaterial(Color c)
        {
            var sh = Shader.Find("Universal Render Pipeline/Lit") ?? Shader.Find("Standard");
            var m = new Material(sh) { color = c };
            if (m.HasProperty("_BaseColor")) m.SetColor("_BaseColor", c);
            if (m.HasProperty("_Smoothness")) m.SetFloat("_Smoothness", 0.25f);
            return m;
        }

        static Transform Box(Transform parent, Material m, Vector3 pos, Vector3 size)
            => Prim(PrimitiveType.Cube, parent, m, pos, size);

        static Transform Capsule(Transform parent, Material m, Vector3 pos, Vector3 size)
            => Prim(PrimitiveType.Capsule, parent, m, pos, size);

        static Transform Prim(PrimitiveType type, Transform parent, Material m,
            Vector3 pos, Vector3 size)
        {
            var go = GameObject.CreatePrimitive(type);
            var col = go.GetComponent<Collider>();
            if (col != null) Destroy(col);   // 采集体不参与物理，避免干扰宿主场景
            go.transform.SetParent(parent, false);
            go.transform.localPosition = pos;
            go.transform.localScale = size;
            go.GetComponent<Renderer>().sharedMaterial = m;
            return go.transform;
        }

        void HandleTick(int tick)
        {
            _time = (float)(tick * CaptureFormats.FrameIntervalSeconds);
            ApplyPositions(_time);
        }

        Vector3 AnchorAt(float t, out float radius)
        {
            radius = 3f;
            if (_anchors.Count == 0) return transform.position;
            if (_anchors.Count == 1)
            {
                radius = _anchorRadius[0];
                return new Vector3(_anchors[0].x, _anchors[0].y, _anchors[0].z);
            }
            for (int i = 1; i < _anchors.Count; i++)
            {
                float t0 = _anchors[i - 1].w, t1 = _anchors[i].w;
                if (t > t1 && i < _anchors.Count - 1) continue;
                float k = Mathf.Approximately(t1, t0) ? 0f : Mathf.Clamp01((t - t0) / (t1 - t0));
                var a = new Vector3(_anchors[i - 1].x, _anchors[i - 1].y, _anchors[i - 1].z);
                var b = new Vector3(_anchors[i].x, _anchors[i].y, _anchors[i].z);
                radius = Mathf.Lerp(_anchorRadius[i - 1], _anchorRadius[i], k);
                return Vector3.Lerp(a, b, k);
            }
            var last = _anchors[_anchors.Count - 1];
            radius = _anchorRadius[_anchorRadius.Count - 1];
            return new Vector3(last.x, last.y, last.z);
        }

        void ApplyPositions(float t)
        {
            Vector3 anchor = AnchorAt(t, out _);
            float groundY = _cfg?.GroundY ?? 0f;

            foreach (var a in _agents)
            {
                // 环带角速度 = 线速度 / 半径（保证 speed 语义是真实线速度）
                float omega = a.Gait == Gait.Gesture ? 0.25f : a.Speed / Mathf.Max(a.Radius, 0.5f);
                a.Theta += a.AngularDir * omega * (float)CaptureFormats.FrameIntervalSeconds;

                float r = a.Radius + Mathf.Sin(t * 0.37f + a.Phase) * 0.12f;
                var next = anchor + new Vector3(Mathf.Cos(a.Theta), 0f, Mathf.Sin(a.Theta)) * r;
                TryGround(next, out float ny);
                next.y = ny;

                // 穿模预检（占据图优先；无图时回落到射线，见 LoadOccupancy 的说明）
                bool blocked = false;
                if (_obstacles.Count > 0)
                {
                    blocked = Blocked(next, a.BoundRadius, a.Height);
                }
                else if (!blocked)
                {
                    var from = a.Root.position + Vector3.up * 0.9f;
                    var dir = next - a.Root.position;
                    blocked = _cfg != null && dir.sqrMagnitude > 1e-6f
                        && Physics.Raycast(from, dir.normalized, _cfg.ObstacleProbeDistance,
                                           _cfg.ObstacleMask, QueryTriggerInteraction.Ignore);
                }
                if (blocked)
                {
                    // 掉头 + 收缩环带；连续受阻时逐步内收，避免贴着货架来回抖
                    a.AngularDir = -a.AngularDir;
                    a.Radius = Mathf.Max(2.0f, a.Radius * 0.88f);
                    a.BlockedTicks++;
                    continue;
                }
                a.BlockedTicks = 0;

                a.Root.position = next;
                // 朝向沿切线（Gesture 档原地转身，动作幅度更大）
                var tangent = new Vector3(-Mathf.Sin(a.Theta), 0f, Mathf.Cos(a.Theta)) * a.AngularDir;
                a.Root.rotation = a.Gait == Gait.Gesture
                    ? Quaternion.Euler(0f, t * 55f + a.Phase * Mathf.Rad2Deg, 0f)
                    : Quaternion.LookRotation(tangent, Vector3.up);

                Animate(a, t);
            }

            Resolve(anchor, groundY);
        }

        /// <summary>
        /// 位置解算：agent 互斥 + 相机 keepout + 静态障碍最终校验。
        ///
        /// 环带公式只保证「绕锚点走」，不保证 agent 之间不重叠——占据图是**静态场景
        /// 几何**，对我方注入体不起作用。首版实测（复核 5 clip × 9 时间点目检）：
        /// NPC 粘连成团、身体嵌进 AGV、车与车互穿、NPC 贴到镜头前占满画面，
        /// 按 docs/69 §8.1「大面积穿模 / 遮挡物大面积」全是硬打回项。
        /// 推挤限幅 + 多次迭代，保证解算不把 agent 弹飞造成画面跳变。
        /// </summary>
        void Resolve(Vector3 anchor, float groundY)
        {
            if (_cfg == null || _agents.Count == 0) return;
            float maxPush = _cfg.MaxPushPerTick;

            for (int it = 0; it < Mathf.Max(1, _cfg.SeparationIterations); it++)
            {
                // ① agent 两两互斥（AGV 半径大，用各自 BoundRadius 之和兜底）
                for (int i = 0; i < _agents.Count; i++)
                    for (int j = i + 1; j < _agents.Count; j++)
                    {
                        var a = _agents[i];
                        var b = _agents[j];
                        var pa = a.Root.position;
                        var pb = b.Root.position;
                        float minD = Mathf.Max(_cfg.AgentSeparation, a.BoundRadius + b.BoundRadius);
                        var d = new Vector3(pb.x - pa.x, 0f, pb.z - pa.z);
                        float dist = d.magnitude;
                        if (dist >= minD) continue;
                        // 完全重合时用固定方向拆开，避免 NaN
                        var dir = dist > 1e-4f ? d / dist : new Vector3(1f, 0f, 0f);
                        float push = Mathf.Min((minD - dist) * 0.5f, maxPush);
                        a.Root.position = pa - dir * push;
                        b.Root.position = pb + dir * push;
                        SeparationPushes++;
                    }

                // ② 相机 keepout（防贴脸遮挡）
                if (Rig != null && Rig.IsInitialized)
                    for (int i = 0; i < _agents.Count; i++)
                    {
                        var a = _agents[i];
                        for (int lane = 0; lane < MultiCamRig.LaneCount; lane++)
                        {
                            var cam = Rig.Lanes[lane].Camera;
                            if (cam == null) continue;
                            var cp = cam.transform.position;
                            var p = a.Root.position;
                            var d = new Vector3(p.x - cp.x, 0f, p.z - cp.z);
                            float dist = d.magnitude;
                            float minD = _cfg.CameraKeepout + a.BoundRadius;
                            if (dist >= minD) continue;
                            var dir = dist > 1e-4f ? d / dist : new Vector3(0f, 0f, 1f);
                            float push = Mathf.Min(minD - dist, maxPush);
                            a.Root.position = p + dir * push;
                            CameraPushes++;
                        }
                    }
            }

            // ③ 静态障碍最终校验：解算可能把 agent 推进货架，推不出来就退回上一帧位置
            for (int i = 0; i < _agents.Count; i++)
            {
                var a = _agents[i];
                var p = a.Root.position;
                TryGround(p, out float py);
                p.y = py;
                if (_obstacles.Count > 0 && Blocked(p, a.BoundRadius, a.Height))
                    p = a.LastGood == Vector3.zero ? p : a.LastGood;
                else
                    a.LastGood = p;
                a.Root.position = p;

                // ④ **把解算结果回写进环带参数**——否则下一 tick 的 ApplyPositions
                // 用未变的 (Theta, Radius) 重算位置，把推挤成果整个覆盖掉，
                // 解算等于没做（首版实测：NPC 依旧粘连成团，复核 §8.1 打回）。
                var d = p - anchor;
                d.y = 0f;
                float r = d.magnitude;
                if (r > 1e-3f)
                {
                    a.Radius = Mathf.Max(2.0f, r);
                    a.Theta = Mathf.Atan2(d.z, d.x);
                }
            }
        }

        static void Animate(Agent a, float t)
        {
            if (a.IsVehicle || a.ArmL == null) return;
            float freq = a.Gait == Gait.Run ? 8.5f : a.Gait == Gait.Walk ? 4.5f : 2.2f;
            float amp = a.Gait == Gait.Run ? 55f : a.Gait == Gait.Walk ? 32f : 70f; // Gesture=挥臂
            float s = Mathf.Sin(t * freq + a.Phase) * amp;
            a.ArmL.localRotation = Quaternion.Euler(s, 0f, 0f);
            a.ArmR.localRotation = Quaternion.Euler(-s, 0f, 0f);
            if (a.LegL != null && a.Gait != Gait.Gesture)
            {
                a.LegL.localRotation = Quaternion.Euler(-s * 0.7f, 0f, 0f);
                a.LegR.localRotation = Quaternion.Euler(s * 0.7f, 0f, 0f);
            }
        }

        public void CollectSpheres(List<DynSphere> buffer)
        {
            foreach (var a in _agents)
                buffer.Add(new DynSphere(
                    a.Root.position + Vector3.up * (a.Height * 0.5f), a.BoundRadius));
        }

        public string QuotaNote()
            => string.Format(CultureInfo.InvariantCulture,
                "humanoid={0} vehicle={1} gait[walk={2},run={3},gesture={4}] "
                + "resolve[sep={5},cam={6}] ungrounded={7}",
                HumanoidSpawned, VehicleSpawned, WalkCount, RunCount, GestureCount,
                SeparationPushes, CameraPushes, UngroundedTicks);
    }
}
