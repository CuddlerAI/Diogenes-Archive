// docs/78 §10 修补：把 vehicle_plan.json 里的三台载机（+氛围受控车）在采集世界里
// **渲染出来**。此前只回放相机位姿——三辆「车」在引擎里是看不见的挂点。
// （历史：统览圆时代 cam4 需要框住三车，载机层单独加回给 cam4；cam4 改为
// 「同 guide 路径异速率」后四路一律不见载机车体，见 Build 内注释。）
//
// 设计：
//   * 车体用 Megacity 自带 HoverCar 预制体（GameObject 实例，非 ECS 实体——采集世界
//     只需可见性与逐帧摆位，不需要 ECS 的批处理/物理）；
//   * 逐帧 kinematic 驱动：位姿取自 vehicle_plan，经与 PosePlayback **同一套**世界变换
//     （WorldOffset + WorldYawDeg），保证车体与车载相机严格同源、不产生错位；
//   * **自机身不入自画**：carrier-i 的车体挂到 lane-i 的专属层，MultiCamRig 已让每路
//     cullingMask 排除其它路的专属层——反过来这里还要把**自己的**专属层也从自己相机里
//     排除（车载视角看不到自己的车壳，但另外两路看得到它）。
//
// 坐标系（0806 双重世界偏移修补）：本组件吃的**必须**是生成器局部系——与
// action_camera_*.json 同系，两者由同一套 R_y(WorldYawDeg)+WorldOffset 一起搬进内容
// 世界。vehicle_plan v1.0 的 `frames_world` 却把计划侧的 world_offset 烘进了坐标，
// 于是同一偏移被加了两次：s18 实证车体离本路相机 114.36m（应恒为 |mount_offset|=3.33m），
// 统览画面里三辆载机全部缺席。计划侧 v1.1 起改落 `frames_local`；这里对 v1.0 文件
// 在载入时减掉计划内置偏移换回局部系，并补一条**构造性挂载自检**（首帧 + 逐 tick）：
// 车体与本路相机的距离必须 ≈ |mount_offset|，超差即 AbortClip——位姿断言只管相机、
// 可见性自检只核 cullingMask，这类错位此前无人把关。

using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using Diogenes.EngineCapture;
using UnityEngine;

namespace Diogenes.Capture.Megacity
{
    /// <summary>vehicle_plan.json 的一条载具轨迹（生成器局部坐标系）。</summary>
    public sealed class VehicleTrack
    {
        public int Vid;
        public string Role;
        public double[][] Positions;   // [frame][3]
        public double[] Yaws;          // [frame] 度
        public int FrameCount => Positions.Length;
    }

    /// <summary>
    /// 读取 vehicle_plan.json、生成车体、逐 tick 摆位。挂在场景里，BeginClip 前
    /// 调用 <see cref="Build"/>。
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class CarrierVehicleSpawner : MonoBehaviour
    {
        [SerializeField] FrameClock _clock;
        [SerializeField] MultiCamRig _rig;
        [SerializeField] Vector3 _worldOffset = Vector3.zero;
        [SerializeField] float _worldYawDeg;

        public FrameClock Clock { get => _clock; set => _clock = value; }
        public MultiCamRig Rig { get => _rig; set => _rig = value; }
        public Vector3 WorldOffset { get => _worldOffset; set => _worldOffset = value; }
        public float WorldYawDeg { get => _worldYawDeg; set => _worldYawDeg = value; }

        /// <summary>三台载机的车体（下标 0..2 对应 cam1..3）。</summary>
        public GameObject[] Carriers { get; private set; } = new GameObject[0];

        readonly List<VehicleTrack> _tracks = new List<VehicleTrack>();
        readonly List<GameObject> _bodies = new List<GameObject>();
        GameObject _prefab;

        /// <summary>生成的车体总数（含氛围受控车），报表用。</summary>
        public int SpawnedCount => _bodies.Count;

        /// <summary>计划自称的版本（`version` 字段），报表/日志用。</summary>
        public string PlanVersion { get; private set; } = "";

        /// <summary>逐帧坐标所在的系：v1.1+ 为 `generator-local`，v1.0 为
        /// `frames_world(-world_offset)`——后者载入时已减掉计划内置偏移。</summary>
        public string PlanFramesFrame { get; private set; } = "";

        /// <summary>车体↔本路相机的刚性挂载间距 = |mount_offset|（计划自带）。</summary>
        public double MountDistanceMeters { get; private set; }

        public void LoadPlan(string planDir)
        {
            _tracks.Clear();
            string path = Path.Combine(planDir, "vehicle_plan.json");
            if (!File.Exists(path))
                throw new FileNotFoundException("vehicle_plan.json 不存在：" + path);
            var root = MiniJson.ParseFile(path) as Dictionary<string, object>;
            if (root == null || !root.TryGetValue("vehicles", out var vlist))
                throw new InvalidDataException("vehicle_plan.json 缺 vehicles");
            PlanVersion = root.TryGetValue("version", out var vv) ? vv as string ?? "" : "";
            // 挂载自检的期望值必须来自计划本身（不能反推：错位后的间距同样是个常数）。
            var mount = ReadVec3(root, "mount_offset");
            if (mount == null)
                throw new InvalidDataException("vehicle_plan.json 缺 mount_offset——挂载自检无期望值");
            MountDistanceMeters = Math.Sqrt(mount[0] * mount[0] + mount[1] * mount[1]
                                            + mount[2] * mount[2]);
            // v1.0 兼容：`frames_world` 把计划侧 world_offset 烘进了坐标，减掉换回局部系。
            var planOffset = ReadVec3(root, "world_offset") ?? new[] { 0.0, 0.0, 0.0 };
            foreach (var item in (List<object>)vlist)
            {
                var d = (Dictionary<string, object>)item;
                // [frame][x, y, z, yaw_deg]（生成器局部系，已含 lane/y 偏移）
                bool baked = !d.TryGetValue("frames_local", out var frames);
                if (baked && !d.TryGetValue("frames_world", out frames))
                    throw new InvalidDataException("vehicle_plan.json 车目缺 frames_local/frames_world");
                PlanFramesFrame = baked ? "frames_world(-world_offset)" : "generator-local";
                var fw = (List<object>)frames;
                int n = fw.Count;
                var pos = new double[n][];
                var yaws = new double[n];
                for (int i = 0; i < n; i++)
                {
                    var row = (List<object>)fw[i];
                    pos[i] = baked
                        ? new[] { ToD(row[0]) - planOffset[0], ToD(row[1]) - planOffset[1],
                                  ToD(row[2]) - planOffset[2] }
                        : new[] { ToD(row[0]), ToD(row[1]), ToD(row[2]) };
                    yaws[i] = ToD(row[3]);
                }
                _tracks.Add(new VehicleTrack
                {
                    Vid = (int)ToD(d["id"]),
                    Role = d.TryGetValue("role", out var r) ? (string)r : "",
                    Positions = pos,
                    Yaws = yaws
                });
            }
            Debug.Log("[Diogenes] vehicle_plan v" + PlanVersion + " frames=" + PlanFramesFrame
                      + " plan_world_offset=(" + planOffset[0] + "," + planOffset[1] + ","
                      + planOffset[2] + ") mount=" + MountDistanceMeters.ToString("F3") + "m");
        }

        static double ToD(object o) =>
            o is double dd ? dd : o is long ll ? ll : Convert.ToDouble(o, CultureInfo.InvariantCulture);

        static double[] ReadVec3(Dictionary<string, object> d, string key)
        {
            if (!d.TryGetValue(key, out var v) || !(v is List<object> l) || l.Count != 3)
                return null;
            return new[] { ToD(l[0]), ToD(l[1]), ToD(l[2]) };
        }

        /// <summary>生成车体。prefab 为空时用 Megacity 的 HoverCar 变体（Resources 外
        /// 的预制体由调用方传入，避免本组件依赖具体资产路径）。</summary>
        public void Build(GameObject vehiclePrefab)
        {
            if (_rig == null || !_rig.IsInitialized)
                throw new InvalidOperationException("CarrierVehicleSpawner: rig 必须先 Initialize");
            _prefab = vehiclePrefab;
            var carriers = new List<GameObject>();
            foreach (var t in _tracks)
            {
                var go = _prefab != null
                    ? Instantiate(_prefab)
                    : GameObject.CreatePrimitive(PrimitiveType.Cube);
                go.name = "DiogenesCarrier_" + t.Vid + "_" + t.Role;
                if (_prefab == null)
                    go.transform.localScale = new Vector3(4.2f, 1.6f, 9.0f); // 飞车量级兜底
                bool isCarrier = t.Role != null && t.Role.StartsWith("carrier");
                int slot = isCarrier ? t.Vid - 1 : -1;
                // 载机车体进本路专属层：其它路 cullingMask 已含本层（MultiCamRig 只排除
                // “其它路的层”），本路相机再单独排除自己的层 ⇒ 自机身不入自画。
                if (slot >= 0 && slot < MultiCamRig.LaneCount)
                {
                    int layer = _rig.Lanes[slot].ExclusiveLayer;
                    SetLayerRecursive(go, layer);
                    var cam = _rig.Lanes[slot].Camera;
                    cam.cullingMask &= ~(1 << layer);
                    // **四路全部不见任何载机车体。**
                    // 统览圆时代曾把载机层单独加回 cam4（框三车的取景目的）；cam4 改为
                    // 「同 guide 路径异速率」后（张阔 0714 确认语义），cam4 与载机同路
                    // 尾随，看见前车 = 相机拍到相机，与 cam1-3 互见同罪（需求 §2
                    // 「各视角互相之间不可视」）。MultiCamRig 默认排除所有专属层，
                    // 此处不再给任何路加回 ⇒ 四路 cullingMask 恒不含载机层。
                    // 车体照常生成：物理占位与 dyn 探针（动态物体占比）语义不变。
                    while (carriers.Count <= slot) carriers.Add(null);
                    carriers[slot] = go;
                }
                _bodies.Add(go);
            }
            Carriers = carriers.ToArray();
            VerifyCarrierVisibility();
            ApplyFrame(0);
            // 首帧挂载自检：车体必须就在本路相机旁 |mount_offset| 处。调用方（PosePlayback）
            // 已把相机摆到首帧位姿，所以这里能在开拍前把坐标系错配堵死。
            if (!VerifyMount(0, out string mountDetail))
                throw new InvalidOperationException(
                    "CarrierVehicleSpawner: 首帧挂载自检不通过（坐标系错配？）" + mountDetail);
            Debug.Log("[Diogenes] carrier mount check @frame0: " + DescribeMount());
            // 计数/最差值只统计开拍后的 tick（首帧这一次由本方法把关，不混入报表口径）
            MountFramesVerified = 0;
            WorstMountErrorMeters = 0.0;
        }

        /// <summary>销毁已生成的车体（中止路径用；车体不是本组件的子物体）。</summary>
        public void DestroyBodies()
        {
            foreach (var go in _bodies)
                if (go != null) Destroy(go);
            _bodies.Clear();
            Carriers = new GameObject[0];
        }

        /// <summary>能拍到载机车体的相机路数计数（0 = 满足需求 §2「各视角互相之间
        /// 不可视」）。构造期算出，供 provenance 与调用方核对。</summary>
        public int MutualCarrierVisible { get; private set; } = -1;

        /// <summary>可见性矩阵的可读快照，形如 `cam1=[] cam2=[] cam3=[] cam4=[]`。</summary>
        public string VisibilityMatrix { get; private set; } = "";

        /// <summary>
        /// **构造性自检**：直接核对 cullingMask，不依赖画面。
        ///
        /// 为什么必须这么验而不是看画面：Megacity 的环境车流由 DOTS 系统实时模拟、
        /// 无固定种子，同一份计划连跑两遍**位姿逐位相同（实测 0.000e+00 m）但画面
        /// 逐路不同**。渲染层的 A/B 对比会被车流噪声淹没——本机实测差分图里找不出
        /// 载机车体的增删。所以「谁能看见谁」只能在构造期断言。
        ///
        /// 不变量（cam4 同路径异速率版）：**任何相机看见任何载机车体都违反需求 §2**
        /// ——cam4 与载机同路尾随，拍到前车即相机拍到相机；矩阵四路必须全空。
        /// </summary>
        void VerifyCarrierVisibility()
        {
            var sb = new System.Text.StringBuilder();
            int violations = 0;
            for (int lane = 0; lane < MintCamLaneCount(); lane++)
            {
                var seen = new List<int>();
                for (int slot = 0; slot < MintCamLaneCount(); slot++)
                {
                    if (Carriers == null || slot >= Carriers.Length || Carriers[slot] == null)
                        continue;   // 该 slot 没有载机车体
                    int bit = 1 << _rig.Lanes[slot].ExclusiveLayer;
                    if ((_rig.Lanes[lane].Camera.cullingMask & bit) != 0)
                    {
                        seen.Add(slot + 1);
                        violations++;
                    }
                }
                sb.Append("cam").Append(lane + 1).Append("=[")
                  .Append(string.Join(",", seen)).Append("] ");
            }
            VisibilityMatrix = sb.ToString().TrimEnd();
            MutualCarrierVisible = violations;
            Debug.Log("[Diogenes] carrier visibility: " + VisibilityMatrix
                      + " | mutual_carrier_visible=" + violations
                      + (violations == 0 ? " OK（四路全 0）" : " VIOLATES 69 §2"));
        }

        static int MintCamLaneCount() => MultiCamRig.LaneCount;

        /// <summary>挂载间距容差（m）。计划坐标量化到 1mm、摆位走 float32，s18 全片
        /// 实测偏差 &lt;1e-3 m；而坐标系错配是**百米级**，两者之间没有灰带。</summary>
        public double MountToleranceMeters { get; set; } = 0.25;

        /// <summary>本片至今最差的挂载间距误差（m），报表用。</summary>
        public double WorstMountErrorMeters { get; private set; }

        /// <summary>通过挂载自检的「载机-帧」数（PosePlayback.FramesVerified 同口径）。</summary>
        public long MountFramesVerified { get; private set; }

        /// <summary>
        /// **构造性挂载自检**（0806 双重世界偏移修补的守门人）。
        ///
        /// 不变量：carrier-i 的车体与 cam-i 是刚性挂载（相机 = 车体位姿 ∘ mount_offset），
        /// 所以两者的**世界**距离恒等于 |mount_offset|，与摆位偏移/摆位 yaw 无关——
        /// 这正好把「车体和相机用了不同坐标系」这一类错位变成可断言的量。
        ///
        /// 为什么非查不可：cam4 的出幅断言是纯几何投影（用计划坐标，不看引擎里的车体），
        /// 车体可见性只由 cullingMask 决定（谁能看见谁，不管在哪），
        /// dyn.active_frac 由环境车流撑满——现有自检都对「车体被平移了两次」失明。
        /// </summary>
        /// <returns>true = 通过；false 时 <paramref name="detail"/> 给出超差明细。</returns>
        public bool VerifyMount(int frame, out string detail)
        {
            detail = "";
            // 逐 tick 走这条路径：通过时**不分配**（字符串只在超差/显式描述时才拼）
            System.Text.StringBuilder bad = null;
            int checkedCount = 0;
            for (int slot = 0; slot < Carriers.Length; slot++)
            {
                var cam = LaneCameraFor(slot);
                if (cam == null) continue;
                double dist = Vector3.Distance(Carriers[slot].transform.position,
                                               cam.transform.position);
                double err = Math.Abs(dist - MountDistanceMeters);
                checkedCount++;
                if (err > WorstMountErrorMeters) WorstMountErrorMeters = err;
                if (err <= MountToleranceMeters) continue;
                if (bad == null) bad = new System.Text.StringBuilder();
                bad.Append("carrier").Append(slot + 1)
                   .Append(": |body-cam|=").Append(dist.ToString("F2"))
                   .Append("m, 期望 |mount_offset|=").Append(MountDistanceMeters.ToString("F2"))
                   .Append("m（差 ").Append(err.ToString("F2")).Append("m）body=")
                   .Append(Carriers[slot].transform.position).Append(" cam=")
                   .Append(cam.transform.position).Append("; ");
            }
            if (bad != null)
            {
                detail = "frame " + frame + ": " + bad.ToString().TrimEnd();
                return false;
            }
            MountFramesVerified += checkedCount;
            return true;
        }

        /// <summary>把当前帧的挂载间距写成可读快照（日志/报表用，不进逐 tick 路径）。</summary>
        public string DescribeMount()
        {
            var sb = new System.Text.StringBuilder();
            for (int slot = 0; slot < Carriers.Length; slot++)
            {
                var cam = LaneCameraFor(slot);
                if (cam == null) continue;
                sb.Append("carrier").Append(slot + 1).Append("=")
                  .Append(Vector3.Distance(Carriers[slot].transform.position,
                                           cam.transform.position).ToString("F3"))
                  .Append("m ");
            }
            return sb.ToString().TrimEnd() + " | 期望 "
                   + MountDistanceMeters.ToString("F3") + "m ±"
                   + MountToleranceMeters.ToString("F2") + "m";
        }

        /// <summary>slot 号载机对应的车载相机；没有车体/相机时为 null。</summary>
        Camera LaneCameraFor(int slot)
        {
            if (_rig == null || slot >= MultiCamRig.LaneCount) return null;
            if (Carriers == null || slot >= Carriers.Length || Carriers[slot] == null) return null;
            return _rig.Lanes[slot].Camera;
        }

        void OnEnable()
        {
            if (_clock != null) _clock.OnTick += ApplyFrame;
        }

        void OnDisable()
        {
            if (_clock != null) _clock.OnTick -= ApplyFrame;
        }

        /// <summary>逐 tick 摆位。世界变换与 PosePlayback 逐字一致（同源，防错位）。</summary>
        public void ApplyFrame(int frame)
        {
            double yawRad = _worldYawDeg * (Math.PI / 180.0);
            double cy = Math.Cos(yawRad), sy = Math.Sin(yawRad);
            for (int i = 0; i < _tracks.Count && i < _bodies.Count; i++)
            {
                var t = _tracks[i];
                if (t.FrameCount == 0) continue;
                int f = frame < 0 ? 0 : (frame >= t.FrameCount ? t.FrameCount - 1 : frame);
                var p = t.Positions[f];
                double rx = p[0] * cy + p[2] * sy;
                double rz = -p[0] * sy + p[2] * cy;
                _bodies[i].transform.SetPositionAndRotation(
                    new Vector3((float)(rx + _worldOffset.x),
                                (float)(p[1] + _worldOffset.y),
                                (float)(rz + _worldOffset.z)),
                    Quaternion.Euler(0f, (float)t.Yaws[f] + _worldYawDeg, 0f));
            }
            // 逐 tick 复核挂载不变量（相机已在本 tick 摆好：PosePlayback 先于本组件
            // 订阅 OnTick）。首帧由 Build 直接把关，开拍后走标准中止通道。
            if (_clock != null && _clock.IsRunning && !VerifyMount(frame, out string detail))
                _clock.AbortClip("carrier_mount_mismatch", detail);
        }

        static void SetLayerRecursive(GameObject go, int layer)
        {
            go.layer = layer;
            foreach (Transform c in go.transform) SetLayerRecursive(c.gameObject, layer);
        }
    }
}
