// docs/78 §4.1 SceneProbeExporter（G'2/G'3）核心：真实占据快照 + 车流走廊锚点候选。
//
// 输出格式**以 engine-data/trajgen/trajgen/scene.py 的 Scene.from_dict 为准**（不发明新格式）：
//   {name, kind, lo, hi, ground_y, eye_band, boxes:[{lo,hi}...]}
// 额外键（world_offset / anchor_corridors / export）loader 按字典读取直接忽略，仅供
// 编排器（megacity_capture.py 读 world_offset）与留档报告使用。
//
// 三件事（docs/78 §4.1）：
//   1. 占据体素：0.5m 分辨率，Unity.Physics CollisionWorld 静态体窄相 CalculateDistance
//      查询（G'3：导出期用 CollisionWorld，离线期 trajgen 不再射线）→ 走廊种子泛洪
//      填实（建筑内腔封死）→ 贪心合并 AABB 盒；
//   2. 锚点候选：沿 Path/PathNode 烘焙出的 RoadPoint 折线按方向聚类成直线走廊段
//      （中心线/长度/高度带/轴对齐度），叠加静态开阔度与预热后车流密度打分，
//      选中走廊定义采集体积——trajgen 的 _open_center 在窄条体积里被边界推向中线，
//      锚点区因此**结构性**落在车流走廊上（对 docs/78 §4.1「锚点区优先落走廊」的实现）；
//   3. 相机可行壳：不预扩——trajgen 侧 CLEARANCE_M=0.5 对 boxes 施加同一口径
//      （scene.py free()/push_out），双写会叠成 1.0m。
//
// 坐标纪律（BRINGUP §2）：场景 json 用局部系（原点=选中走廊中点、走廊平均高度-4.5m），
// world_offset 记录局部原点的世界坐标；体积各角的**世界**坐标钳在 |c|<250（<256 ⇒
// float32 半 ulp ≤7.6e-6 <1e-5 构造自检承诺）。scene-file 模式恒等摆位（yaw=0/不吸附），
// 编排器据此传 DIOGENES_YAW=0 / DIOGENES_SNAP_Y=0。

using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Text;
using Unity.Burst;
using Unity.Collections;
using Unity.Entities;
using Unity.Jobs;
using Unity.Mathematics;
using Unity.MegacityMetro.Traffic;
using Unity.Physics;
using Unity.Physics.Systems;
using Unity.Transforms;

namespace Diogenes.Capture.Megacity
{
    /// <summary>Corridor candidate: a straight run of baked road spline (world frame).</summary>
    public sealed class CorridorCandidate
    {
        public int RoadIndex;
        public float3 Start;          // trimmed to the |coord|<Cap core
        public float3 End;
        public float Length;
        public float MeanY;
        public float YSpread;
        public float AxisDeg;         // deviation from the nearest world axis (deg)
        public char Axis;             // 'x' or 'z' (dominant horizontal direction)
        public float OpenFrac;        // static-clearance ≥6m fraction along centerline (camera band)
        public int VehiclesInBand;    // post-warmup vehicles inside the would-be capture volume
        public int VehiclesNear;      // post-warmup vehicles within 200m of the midpoint
        public double Score;

        public float3 Mid => (Start + End) * 0.5f;
        public float3 Dir => math.normalizesafe(End - Start);
    }

    /// <summary>Chosen capture volume (world + local frames).</summary>
    public struct VolumeSpec
    {
        public float3 LoW, HiW;       // world AABB
        public float3 Origin;         // world coords of the local origin (== world_offset)
        public char Axis;
        public float CorridorYLocal;  // corridor mean height in local frame (≈ CamDrop)
        public float3 LoL => LoW - Origin;
        public float3 HiL => HiW - Origin;
    }

    public sealed class SceneProbeResult
    {
        public List<CorridorCandidate> Corridors = new List<CorridorCandidate>();
        public int ChosenIndex = -1;
        public string SelectionMode = "score";   // 缺省打分序；env 开关改选时为 highest/min_y
        public string SelectionNote = "";
        public VolumeSpec Volume;
        public int3 Dims;
        public long SurfaceOccupied;
        public long SolidFilled;
        public int BoxCount;
        public int VehiclesTotal;
        public string SceneJsonPath = "";
        public string ReportPath = "";
        public string Error = "";
        public bool Ok => string.IsNullOrEmpty(Error);
    }

    public static class SceneProbe
    {
        // ---- 口径常量（docs/78 §4.1 + BRINGUP §2/§4 实测几何）----
        public const float VoxelM = 0.5f;          // 占据体素分辨率（docs/78 §4.1）
        public const float CoordCapM = 250f;       // 世界坐标钳（<256 float32 半 ulp 护栏，留 6m 余量）
        public const float CrossHalfDefaultM = 80f; // 体积横向半宽缺省（含全景观察位 standoff 的可审计空间；docs/78 §10）
        /// <summary>体积横向半宽（m）。DIOGENES_CROSS_HALF_M 可覆盖，缺省 80。
        ///
        /// 高走廊在 80m 半宽下导出 0 个盒，在 160m 下导出 9461 个盒；但同一批
        /// 40 个种子的通过率均为 24/40，成功集合重合 23/24。实测轨道半径仅
        /// 52–78m，因此空快照没有造成已观察到的放行偏差。正式高走廊基线仍用
        /// 160m，为未来可能跑出 80m 的轨道保留真实几何防护；缺省保持 80m，
        /// 避免破坏既有导出的可复现性。</summary>
        public static float CrossHalfM
        {
            get
            {
                string s = Environment.GetEnvironmentVariable("DIOGENES_CROSS_HALF_M");
                if (!string.IsNullOrEmpty(s)
                    && float.TryParse(s.Trim(), NumberStyles.Float, CultureInfo.InvariantCulture,
                                      out float v)
                    && v > 0f && v <= CoordCapM)
                    return v;
                return CrossHalfDefaultM;
            }
        }
        public const float AlongHalfMaxM = 90f;    // 体积纵向半长上限
        public const float HeightM = 46f;          // 体积高度（相机带 + 全景观察位爬升余量；docs/78 §10）
        public const float CamDropM = 4.5f;        // ground_y = 走廊高度-4.5（s2c/s2d 实测：相机≈走廊-3）
        public const float EyeLo = 1.4f, EyeHi = 2.2f;
        public const float MinCorridorLenM = 80f;  // 走廊段最短可用长度
        public const float MaxYSpreadM = 10f;      // 走廊段最大高度起伏
        public const float MaxAxisDevDeg = 20f;    // 轴对齐容差
        public const float StraightDevDeg = 15f;   // 直线聚类容差
        public const float OpenProbeDist = 6f;     // 开阔度探测半径
        public const float NearRadiusM = 200f;     // 「附近车流」半径（含上下层走廊，视锥 300m 帽以内）

        // ------------------------------------------------------------ 主流程

        /// <summary>Run the full export against a warmed-up capture world (subscenes loaded,
        /// traffic released and spawned). Writes megacity-scene.json + report into outDir.</summary>
        public static SceneProbeResult ExportAll(World world, string outDir, string sceneName = "megacity-corridor")
        {
            var res = new SceneProbeResult();
            try
            {
                Directory.CreateDirectory(outDir);
                var cw = GetCollisionWorld(world, out int numDynamic, out int numStatic);
                if (numStatic <= 0) { res.Error = "CollisionWorld has no static bodies (physics not built?)"; return res; }

                res.Corridors = ExtractCorridors(world);
                if (res.Corridors.Count == 0) { res.Error = "no usable road corridors inside the |coord|<" + CoordCapM + " core"; return res; }

                foreach (var c in res.Corridors)
                    MeasureOpenness(cw, numDynamic, c);
                res.VehiclesTotal = CountVehicles(world, res.Corridors);
                foreach (var c in res.Corridors)
                    c.Score = (double)c.Length * math.max(0.05f, c.OpenFrac)
                              * (c.VehiclesInBand + 0.25 * c.VehiclesNear)
                              * (c.AxisDeg <= MaxAxisDevDeg ? 1.0 : 0.15);
                res.Corridors.Sort((a, b) => b.Score.CompareTo(a.Score));
                res.ChosenIndex = SelectCorridor(res.Corridors,
                                                 out string selMode, out string selNote);
                res.SelectionMode = selMode;
                res.SelectionNote = selNote;
                var best = res.Corridors[res.ChosenIndex];

                res.Volume = MakeVolume(best);
                var grid = Voxelize(cw, numDynamic, res.Volume, out res.Dims, out res.SurfaceOccupied);
                res.SolidFilled = Solidify(grid, res.Dims, res.Volume);
                var boxes = MergeBoxes(grid, res.Dims, res.Volume);
                res.BoxCount = boxes.Count;

                res.SceneJsonPath = Path.Combine(outDir, "megacity-scene.json");
                WriteSceneJson(res.SceneJsonPath, sceneName, res, boxes);
                res.ReportPath = Path.Combine(outDir, "scene-export-report.md");
                WriteReport(res.ReportPath, res);
            }
            catch (Exception e)
            {
                res.Error = e.GetType().Name + ": " + e.Message + "\n" + e.StackTrace;
            }
            return res;
        }

        // ------------------------------------------------------------ 走廊选择开关

        /// <summary>换景开关（v2 高走廊勘察）。**两个环境变量都缺省时行为与既有完全一致**
        /// （打分序第一）。
        ///   DIOGENES_CORRIDOR_MIN_Y=&lt;float&gt;  只考虑平均高度（世界系）≥ 该值的候选；
        ///   DIOGENES_CORRIDOR_PICK=highest    在（过滤后）带内车流&gt;0 的候选里选平均高度
        ///                                     最高者（高走廊没车不出片，宁可不选）。
        /// 约束下无候选 → 回退打分序第一并在 note/报告里记录（导出不失败）。</summary>
        public static int SelectCorridor(List<CorridorCandidate> sortedByScore,
                                         out string mode, out string note)
        {
            mode = "score";
            note = "";
            string pickEnv = Environment.GetEnvironmentVariable("DIOGENES_CORRIDOR_PICK");
            string minYEnv = Environment.GetEnvironmentVariable("DIOGENES_CORRIDOR_MIN_Y");
            bool highest = !string.IsNullOrEmpty(pickEnv)
                           && pickEnv.Trim().Equals("highest", StringComparison.OrdinalIgnoreCase);
            float minY = float.NegativeInfinity;
            if (!string.IsNullOrEmpty(minYEnv)
                && float.TryParse(minYEnv, NumberStyles.Float, CultureInfo.InvariantCulture,
                                  out float v))
                minY = v;
            if (!highest && float.IsNegativeInfinity(minY))
                return 0;                          // 开关全缺省：既有行为（打分序第一）

            mode = highest ? "highest" : "min_y";
            int best = -1;
            for (int i = 0; i < sortedByScore.Count; i++)
            {
                var c = sortedByScore[i];
                if (c.MeanY < minY) continue;
                if (highest && c.VehiclesInBand <= 0) continue;
                if (best < 0) { best = i; if (!highest) break; }   // min_y 模式保持打分序
                else if (highest && c.MeanY > sortedByScore[best].MeanY) best = i;
            }
            if (best < 0)
            {
                mode = "score";
                note = "fallback=score: 开关约束下无候选（min_y="
                       + (float.IsNegativeInfinity(minY) ? "-inf" : F(minY))
                       + (highest ? ", 需带内车流>0" : "") + "）";
                return 0;
            }
            note = "pick=" + mode
                   + (float.IsNegativeInfinity(minY) ? "" : ", min_y=" + F(minY))
                   + (highest ? ", 带内车流>0" : "");
            return best;
        }

        // ------------------------------------------------------------ 物理世界

        public static CollisionWorld GetCollisionWorld(World world, out int numDynamic, out int numStatic)
        {
            var em = world.EntityManager;
            using (var q = em.CreateEntityQuery(ComponentType.ReadOnly<PhysicsWorldSingleton>()))
            {
                var singleton = q.GetSingleton<PhysicsWorldSingleton>();
                var cw = singleton.CollisionWorld;
                numDynamic = cw.NumDynamicBodies;
                numStatic = cw.NumStaticBodies;
                return cw;
            }
        }

        // ------------------------------------------------------------ 走廊聚类

        /// <summary>RoadPoint/RoadOffset polylines → straight, near-axis runs inside the core.</summary>
        public static List<CorridorCandidate> ExtractCorridors(World world)
        {
            var em = world.EntityManager;
            var outList = new List<CorridorCandidate>();
            using (var q = em.CreateEntityQuery(
                ComponentType.ReadOnly<RoadPoint>(), ComponentType.ReadOnly<RoadOffset>()))
            using (var ents = q.ToEntityArray(Allocator.Temp))
            {
                for (int e = 0; e < ents.Length; e++)
                {
                    var off = em.GetComponentData<RoadOffset>(ents[e]).Value;
                    var buf = em.GetBuffer<RoadPoint>(ents[e], true);
                    if (buf.Length < 2) continue;
                    var pts = new float3[buf.Length];
                    for (int i = 0; i < buf.Length; i++) pts[i] = buf[i].Value + off;

                    int runStart = 0;
                    for (int i = 1; i < pts.Length; i++)
                    {
                        bool closeRun = false;
                        if (i < pts.Length - 1)
                        {
                            float3 runDir = math.normalizesafe(pts[i] - pts[runStart]);
                            float3 segDir = math.normalizesafe(pts[i + 1] - pts[i]);
                            float dev = math.degrees(math.acos(math.clamp(math.dot(runDir, segDir), -1f, 1f)));
                            if (dev > StraightDevDeg) closeRun = true;
                        }
                        else closeRun = true;
                        if (!closeRun) continue;
                        EmitRun(outList, e, pts, runStart, i);
                        runStart = i;
                    }
                }
            }
            return outList;
        }

        static void EmitRun(List<CorridorCandidate> outList, int roadIndex, float3[] pts, int i0, int i1)
        {
            if (i1 <= i0) return;
            float3 a = pts[i0], b = pts[i1];
            // 端点逐轴钳进核心区（沿线段参数化裁剪；水平两轴 + 高度带顶部）
            float t0 = 0f, t1 = 1f;
            for (int axis = 0; axis < 3; axis += 2)
            {
                float pa = a[axis], pb = b[axis];
                if (!ClampSpan(pa, pb, -CoordCapM, CoordCapM, ref t0, ref t1)) return;
            }
            if (t1 <= t0) return;
            float3 s = math.lerp(a, b, t0);
            float3 t = math.lerp(a, b, t1);
            float meanY = (s.y + t.y) * 0.5f;
            if (meanY + HeightM - CamDropM > CoordCapM || meanY < -CoordCapM) return;

            float ySpread = 0f;
            for (int i = i0; i <= i1; i++)
            {
                float yy = math.clamp(pts[i].y, math.min(s.y, t.y) - 50, math.max(s.y, t.y) + 50);
                ySpread = math.max(ySpread, math.abs(yy - meanY));
            }
            float len = math.distance(s, t);
            if (len < MinCorridorLenM || ySpread > MaxYSpreadM) return;

            float2 d = math.normalizesafe(new float2(t.x - s.x, t.z - s.z));
            float major = math.max(math.abs(d.x), math.abs(d.y));
            float minor = math.min(math.abs(d.x), math.abs(d.y));
            outList.Add(new CorridorCandidate
            {
                RoadIndex = roadIndex,
                Start = s,
                End = t,
                Length = len,
                MeanY = meanY,
                YSpread = ySpread,
                AxisDeg = math.degrees(math.atan2(minor, math.max(1e-6f, major))),
                Axis = math.abs(d.y) >= math.abs(d.x) ? 'z' : 'x',
            });
        }

        static bool ClampSpan(float pa, float pb, float lo, float hi, ref float t0, ref float t1)
        {
            float d = pb - pa;
            if (math.abs(d) < 1e-6f)
                return pa >= lo && pa <= hi;
            float ta = (lo - pa) / d, tb = (hi - pa) / d;
            if (ta > tb) (ta, tb) = (tb, ta);
            t0 = math.max(t0, ta);
            t1 = math.min(t1, tb);
            return t1 > t0;
        }

        // ------------------------------------------------------------ 开阔度与车流

        struct StaticOnlyCollector : ICollector<DistanceHit>
        {
            public int NumDynamic;
            public float MaxDistance;
            int _hits;
            public bool EarlyOutOnFirstHit => true;
            public float MaxFraction => MaxDistance;
            public int NumHits => _hits;
            public bool AddHit(DistanceHit hit)
            {
                if (hit.RigidBodyIndex < NumDynamic) return false;   // 动态体（车/飞艇）不算占据
                _hits = 1;
                return true;
            }
        }

        /// <summary>Fraction of centerline samples (camera band height) with no static body
        /// within OpenProbeDist. Cheap main-thread narrowphase queries.</summary>
        public static void MeasureOpenness(CollisionWorld cw, int numDynamic, CorridorCandidate c)
        {
            int n = math.max(2, (int)(c.Length / 8f));
            int free = 0;
            for (int i = 0; i <= n; i++)
            {
                float3 p = math.lerp(c.Start, c.End, i / (float)n);
                p.y -= 3f;    // 相机带（走廊 -3m）
                var col = new StaticOnlyCollector { NumDynamic = numDynamic, MaxDistance = OpenProbeDist };
                var input = new PointDistanceInput
                { Position = p, MaxDistance = OpenProbeDist, Filter = CollisionFilter.Default };
                if (!cw.CalculateDistance(input, ref col)) free++;
            }
            c.OpenFrac = free / (float)(n + 1);
        }

        /// <summary>Post-warmup vehicle census per corridor. Returns total vehicle count.</summary>
        public static int CountVehicles(World world, List<CorridorCandidate> corridors)
        {
            var em = world.EntityManager;
            int total = 0;
            using (var q = em.CreateEntityQuery(
                ComponentType.ReadOnly<VehiclePathing>(), ComponentType.ReadOnly<LocalToWorld>()))
            using (var l2w = q.ToComponentDataArray<LocalToWorld>(Allocator.Temp))
            {
                total = l2w.Length;
                foreach (var c in corridors)
                {
                    float3 mid = c.Mid;
                    float3 dir = c.Dir;
                    float alongHalf = math.min(AlongHalfMaxM, c.Length * 0.5f);
                    int inBand = 0, near = 0;
                    for (int i = 0; i < l2w.Length; i++)
                    {
                        float3 p = l2w[i].Position;
                        float3 rel = p - mid;
                        if (math.length(rel) <= NearRadiusM) near++;
                        float along = math.dot(new float2(rel.x, rel.z),
                                               math.normalizesafe(new float2(dir.x, dir.z)));
                        float2 latv = new float2(rel.x, rel.z)
                                      - along * math.normalizesafe(new float2(dir.x, dir.z));
                        if (math.abs(along) <= alongHalf && math.length(latv) <= CrossHalfM
                            && math.abs(rel.y) <= 20f)
                            inBand++;
                    }
                    c.VehiclesInBand = inBand;
                    c.VehiclesNear = near;
                }
            }
            return total;
        }

        // ------------------------------------------------------------ 体积

        public static VolumeSpec MakeVolume(CorridorCandidate c)
        {
            float3 mid = c.Mid;
            float alongHalf = math.min(AlongHalfMaxM, c.Length * 0.5f);
            float gy = c.MeanY - CamDropM;
            float height = math.min(HeightM, CoordCapM - gy);

            float xHalf = c.Axis == 'z' ? CrossHalfM : alongHalf;
            float zHalf = c.Axis == 'z' ? alongHalf : CrossHalfM;
            // 世界坐标钳：收边并回中（保持体积仍然含走廊中线）
            float xLo = math.max(-CoordCapM, mid.x - xHalf), xHi = math.min(CoordCapM, mid.x + xHalf);
            float zLo = math.max(-CoordCapM, mid.z - zHalf), zHi = math.min(CoordCapM, mid.z + zHalf);
            var origin = new float3(
                (float)System.Math.Round((xLo + xHi) * 0.5f, 2),
                (float)System.Math.Round(gy, 2),
                (float)System.Math.Round((zLo + zHi) * 0.5f, 2));
            return new VolumeSpec
            {
                LoW = new float3(xLo, gy, zLo),
                HiW = new float3(xHi, gy + height, zHi),
                Origin = origin,
                Axis = c.Axis,
                CorridorYLocal = c.MeanY - gy,
            };
        }

        // ------------------------------------------------------------ 占据体素（Burst）

        [BurstCompile]
        struct StaticVoxelJob : IJobParallelFor
        {
            [ReadOnly] public CollisionWorld World;
            public float3 LoW;
            public int3 Dims;
            public float Voxel;
            public float MaxDist;
            public int NumDynamic;
            [NativeDisableParallelForRestriction] public NativeArray<byte> Grid;

            public void Execute(int iz)
            {
                for (int iy = 0; iy < Dims.y; iy++)
                {
                    int row = (iz * Dims.y + iy) * Dims.x;
                    for (int ix = 0; ix < Dims.x; ix++)
                    {
                        var p = LoW + new float3(ix + 0.5f, iy + 0.5f, iz + 0.5f) * Voxel;
                        var col = new StaticOnlyCollector { NumDynamic = NumDynamic, MaxDistance = MaxDist };
                        var input = new PointDistanceInput
                        { Position = p, MaxDistance = MaxDist, Filter = CollisionFilter.Default };
                        World.CalculateDistance(input, ref col);
                        Grid[row + ix] = (byte)(col.NumHits > 0 ? 1 : 0);
                    }
                }
            }
        }

        /// <summary>0.5m static-occupancy voxelization of the volume (Burst, parallel by z-slice).
        /// A voxel is occupied when a STATIC collider surface lies within half its diagonal.</summary>
        public static byte[] Voxelize(CollisionWorld cw, int numDynamic, VolumeSpec vol,
                                      out int3 dims, out long occupied)
        {
            float3 size = vol.HiW - vol.LoW;
            dims = new int3(
                math.max(1, (int)math.ceil(size.x / VoxelM)),
                math.max(1, (int)math.ceil(size.y / VoxelM)),
                math.max(1, (int)math.ceil(size.z / VoxelM)));
            long n = (long)dims.x * dims.y * dims.z;
            using (var grid = new NativeArray<byte>((int)n, Allocator.TempJob,
                                                    NativeArrayOptions.UninitializedMemory))
            {
                var job = new StaticVoxelJob
                {
                    World = cw,
                    LoW = vol.LoW,
                    Dims = dims,
                    Voxel = VoxelM,
                    MaxDist = VoxelM * 0.9f,    // 半对角 0.433 + 余量：表面壳连续不漏
                    NumDynamic = numDynamic,
                    Grid = grid,
                };
                job.Schedule(dims.z, 1).Complete();
                var managed = grid.ToArray();
                occupied = 0;
                for (int i = 0; i < managed.Length; i++) occupied += managed[i];
                return managed;
            }
        }

        // ------------------------------------------------------------ 泛洪填实

        /// <summary>Flood fill open air from corridor seeds (6-connected); everything not
        /// reached and not surface becomes occupied (seals building interiors). Returns the
        /// number of voxels flipped to occupied.</summary>
        public static long Solidify(byte[] grid, int3 dims, VolumeSpec vol)
        {
            int nx = dims.x, ny = dims.y, nz = dims.z;
            long n = (long)nx * ny * nz;
            var visited = new bool[n];
            var stack = new Stack<int>(1 << 16);

            void Seed(float3 local)
            {
                int ix = (int)((local.x - vol.LoL.x) / VoxelM);
                int iy = (int)((local.y - vol.LoL.y) / VoxelM);
                int iz = (int)((local.z - vol.LoL.z) / VoxelM);
                if (ix < 0 || iy < 0 || iz < 0 || ix >= nx || iy >= ny || iz >= nz) return;
                int idx = (iz * ny + iy) * nx + ix;
                if (grid[idx] == 0 && !visited[idx]) { visited[idx] = true; stack.Push(idx); }
            }

            // 种子：走廊中线（局部系沿主轴）在走廊高度与相机带高度两层
            float alongLo = vol.Axis == 'z' ? vol.LoL.z : vol.LoL.x;
            float alongHi = vol.Axis == 'z' ? vol.HiL.z : vol.HiL.x;
            for (float a = alongLo + VoxelM; a < alongHi; a += VoxelM)
            {
                foreach (float y in new[] { vol.CorridorYLocal, (EyeLo + EyeHi) * 0.5f })
                {
                    var p = vol.Axis == 'z' ? new float3(0f, y, a) : new float3(a, y, 0f);
                    Seed(p);
                }
            }

            while (stack.Count > 0)
            {
                int idx = stack.Pop();
                int ix = idx % nx, iy = (idx / nx) % ny, iz = idx / (nx * ny);
                for (int d = 0; d < 6; d++)
                {
                    int jx = ix + (d == 0 ? 1 : d == 1 ? -1 : 0);
                    int jy = iy + (d == 2 ? 1 : d == 3 ? -1 : 0);
                    int jz = iz + (d == 4 ? 1 : d == 5 ? -1 : 0);
                    if (jx < 0 || jy < 0 || jz < 0 || jx >= nx || jy >= ny || jz >= nz) continue;
                    int j = (jz * ny + jy) * nx + jx;
                    if (visited[j] || grid[j] != 0) continue;
                    visited[j] = true;
                    stack.Push(j);
                }
            }

            long filled = 0;
            for (long i = 0; i < n; i++)
            {
                if (grid[i] == 0 && !visited[i]) { grid[i] = 1; filled++; }
            }
            return filled;
        }

        // ------------------------------------------------------------ 盒合并

        /// <summary>Greedy exact merge of occupied voxels into AABBs (grow y → z → x).</summary>
        public static List<(float3 lo, float3 hi)> MergeBoxes(byte[] grid, int3 dims, VolumeSpec vol)
        {
            int nx = dims.x, ny = dims.y, nz = dims.z;
            var consumed = new bool[grid.Length];
            var boxes = new List<(float3, float3)>();
            int Idx(int x, int y, int z) => (z * ny + y) * nx + x;

            for (int z0 = 0; z0 < nz; z0++)
            for (int y0 = 0; y0 < ny; y0++)
            for (int x0 = 0; x0 < nx; x0++)
            {
                int i0 = Idx(x0, y0, z0);
                if (grid[i0] == 0 || consumed[i0]) continue;

                int y1 = y0;
                while (y1 + 1 < ny && grid[Idx(x0, y1 + 1, z0)] != 0 && !consumed[Idx(x0, y1 + 1, z0)]) y1++;

                int z1 = z0;
                bool CanZ(int z)
                {
                    for (int y = y0; y <= y1; y++)
                        if (grid[Idx(x0, y, z)] == 0 || consumed[Idx(x0, y, z)]) return false;
                    return true;
                }
                while (z1 + 1 < nz && CanZ(z1 + 1)) z1++;

                int x1 = x0;
                bool CanX(int x)
                {
                    for (int z = z0; z <= z1; z++)
                        for (int y = y0; y <= y1; y++)
                            if (grid[Idx(x, y, z)] == 0 || consumed[Idx(x, y, z)]) return false;
                    return true;
                }
                while (x1 + 1 < nx && CanX(x1 + 1)) x1++;

                for (int z = z0; z <= z1; z++)
                    for (int y = y0; y <= y1; y++)
                        for (int x = x0; x <= x1; x++)
                            consumed[Idx(x, y, z)] = true;

                boxes.Add((
                    vol.LoL + new float3(x0, y0, z0) * VoxelM,
                    vol.LoL + new float3(x1 + 1, y1 + 1, z1 + 1) * VoxelM));
            }
            return boxes;
        }

        // ------------------------------------------------------------ 落盘

        static string F(double v) => System.Math.Round(v, 3).ToString("0.###", CultureInfo.InvariantCulture);
        static string V3(float3 v) => "[" + F(v.x) + ", " + F(v.y) + ", " + F(v.z) + "]";

        public static void WriteSceneJson(string path, string name, SceneProbeResult res,
                                          List<(float3 lo, float3 hi)> boxes)
        {
            var vol = res.Volume;
            var best = res.Corridors[res.ChosenIndex];
            var sb = new StringBuilder(1 << 22);
            sb.Append("{\n");
            sb.Append("  \"name\": \"").Append(name).Append("\",\n");
            sb.Append("  \"kind\": \"megacity_corridor\",\n");
            sb.Append("  \"lo\": ").Append(V3(vol.LoL)).Append(",\n");
            sb.Append("  \"hi\": ").Append(V3(vol.HiL)).Append(",\n");
            sb.Append("  \"ground_y\": 0.0,\n");
            sb.Append("  \"eye_band\": [").Append(F(EyeLo)).Append(", ").Append(F(EyeHi)).Append("],\n");
            sb.Append("  \"boxes\": [");
            for (int i = 0; i < boxes.Count; i++)
            {
                sb.Append(i > 0 ? ",\n    " : "\n    ");
                sb.Append("{\"lo\": ").Append(V3(boxes[i].lo))
                  .Append(", \"hi\": ").Append(V3(boxes[i].hi)).Append("}");
            }
            sb.Append(boxes.Count > 0 ? "\n  ],\n" : "],\n");
            // ---- 附加元数据（trajgen loader 忽略；编排器读 world_offset）----
            sb.Append("  \"world_offset\": ").Append(V3(vol.Origin)).Append(",\n");
            sb.Append("  \"placement\": {\"yaw_deg\": 0, \"snap_y\": false, ")
              .Append("\"note\": \"scene-file 模式恒等摆位：局部坐标+world_offset 即世界坐标\"},\n");
            sb.Append("  \"anchor_corridors\": [\n");
            for (int i = 0; i < res.Corridors.Count; i++)
            {
                var c = res.Corridors[i];
                sb.Append("    {\"road\": ").Append(c.RoadIndex)
                  .Append(", \"start_w\": ").Append(V3(c.Start))
                  .Append(", \"end_w\": ").Append(V3(c.End))
                  .Append(", \"length_m\": ").Append(F(c.Length))
                  .Append(", \"mean_y\": ").Append(F(c.MeanY))
                  .Append(", \"y_spread\": ").Append(F(c.YSpread))
                  .Append(", \"axis\": \"").Append(c.Axis)
                  .Append("\", \"axis_dev_deg\": ").Append(F(c.AxisDeg))
                  .Append(", \"open_frac\": ").Append(F(c.OpenFrac))
                  .Append(", \"vehicles_in_band\": ").Append(c.VehiclesInBand)
                  .Append(", \"vehicles_near\": ").Append(c.VehiclesNear)
                  .Append(", \"score\": ").Append(F(c.Score))
                  .Append(", \"chosen\": ").Append(i == res.ChosenIndex ? "true" : "false")
                  .Append("}").Append(i == res.Corridors.Count - 1 ? "\n" : ",\n");
            }
            sb.Append("  ],\n");
            sb.Append("  \"export\": {\n");
            sb.Append("    \"generator\": \"SceneProbeExporter (docs/78 G'2)\",\n");
            sb.Append("    \"unity\": \"").Append(UnityEngine.Application.unityVersion).Append("\",\n");
            sb.Append("    \"date\": \"").Append(DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss")).Append("\",\n");
            sb.Append("    \"voxel_m\": ").Append(F(VoxelM)).Append(",\n");
            sb.Append("    \"dims\": [").Append(res.Dims.x).Append(", ").Append(res.Dims.y)
              .Append(", ").Append(res.Dims.z).Append("],\n");
            sb.Append("    \"surface_occupied_voxels\": ").Append(res.SurfaceOccupied).Append(",\n");
            sb.Append("    \"solid_filled_voxels\": ").Append(res.SolidFilled).Append(",\n");
            sb.Append("    \"box_count\": ").Append(res.BoxCount).Append(",\n");
            sb.Append("    \"vehicles_total\": ").Append(res.VehiclesTotal).Append(",\n");
            sb.Append("    \"corridor_selection\": {\"mode\": \"").Append(res.SelectionMode)
              .Append("\", \"note\": \"").Append(res.SelectionNote)
              .Append("\", \"chosen_mean_y\": ").Append(F(best.MeanY)).Append("},\n");
            sb.Append("    \"cross_half_m\": ").Append(F(CrossHalfM)).Append(",\n");
            sb.Append("    \"corridor_y_local\": ").Append(F(vol.CorridorYLocal)).Append(",\n");
            sb.Append("    \"clearance_note\": \"可行壳 0.5m 由 trajgen CLEARANCE_M 施加，boxes 未预扩\"\n");
            sb.Append("  }\n");
            sb.Append("}\n");
            File.WriteAllText(path, sb.ToString(), new UTF8Encoding(false));
        }

        public static void WriteReport(string path, SceneProbeResult res)
        {
            var vol = res.Volume;
            long totalVox = (long)res.Dims.x * res.Dims.y * res.Dims.z;
            long occ = res.SurfaceOccupied + res.SolidFilled;
            var sb = new StringBuilder();
            sb.Append("# SceneProbeExporter 导出报告（自动生成，docs/78 §4.1）\n\n");
            sb.Append("- Unity ").Append(UnityEngine.Application.unityVersion)
              .Append(" · ").Append(DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss")).Append("\n");
            sb.Append("- 车辆总数（预热后）: ").Append(res.VehiclesTotal).Append("\n");
            sb.Append("- 走廊候选: ").Append(res.Corridors.Count)
              .Append(" 条（直线聚类 ≤").Append(F(StraightDevDeg)).Append("°、长度 ≥")
              .Append(F(MinCorridorLenM)).Append("m、高度起伏 ≤").Append(F(MaxYSpreadM))
              .Append("m、核心区 |x|,|z| ≤ ").Append(F(CoordCapM)).Append("）\n");
            sb.Append("- 走廊选择: mode=").Append(res.SelectionMode)
              .Append(string.IsNullOrEmpty(res.SelectionNote)
                          ? "（缺省打分序）" : "（" + res.SelectionNote + "）")
              .Append("\n\n");
            sb.Append("## 走廊候选打分（score = 长度 × 开阔度 × (带内车流 + 0.25×近旁车流) × 轴对齐）\n\n");
            sb.Append("| # | road | 轴 | 轴偏° | 长度 m | 平均y | 起伏 | 开阔度 | 带内车 | 近旁车 | score |\n");
            sb.Append("|---|---|---|---|---|---|---|---|---|---|---|\n");
            for (int i = 0; i < res.Corridors.Count; i++)
            {
                var c = res.Corridors[i];
                sb.Append("| ").Append(i == res.ChosenIndex ? "**" + i + "✓**" : i.ToString())
                  .Append(" | ").Append(c.RoadIndex)
                  .Append(" | ").Append(c.Axis)
                  .Append(" | ").Append(F(c.AxisDeg))
                  .Append(" | ").Append(F(c.Length))
                  .Append(" | ").Append(F(c.MeanY))
                  .Append(" | ").Append(F(c.YSpread))
                  .Append(" | ").Append(F(c.OpenFrac))
                  .Append(" | ").Append(c.VehiclesInBand)
                  .Append(" | ").Append(c.VehiclesNear)
                  .Append(" | ").Append(F(c.Score)).Append(" |\n");
            }
            sb.Append("\n## 选中体积\n\n");
            sb.Append("- 世界 AABB: ").Append(V3(vol.LoW)).Append(" ~ ").Append(V3(vol.HiW)).Append("\n");
            sb.Append("- world_offset（局部原点）: ").Append(V3(vol.Origin)).Append("\n");
            sb.Append("- 走廊高度（局部）: ").Append(F(vol.CorridorYLocal))
              .Append("m · 相机带（局部）: ").Append(F(EyeLo)).Append("–").Append(F(EyeHi)).Append("m\n\n");
            sb.Append("## 占据体素（").Append(F(VoxelM)).Append("m · Unity.Physics CollisionWorld 静态体窄相）\n\n");
            sb.Append("- 网格 ").Append(res.Dims.x).Append("×").Append(res.Dims.y).Append("×")
              .Append(res.Dims.z).Append(" = ").Append(totalVox).Append(" 体素\n");
            sb.Append("- 表面壳占据 ").Append(res.SurfaceOccupied)
              .Append(" + 泛洪填实（建筑内腔） ").Append(res.SolidFilled)
              .Append(" = ").Append(occ).Append("（占用率 ")
              .Append(F(100.0 * occ / math.max(1, totalVox))).Append("%）\n");
            sb.Append("- 贪心合并 → ").Append(res.BoxCount).Append(" 个 AABB 盒\n");
            sb.Append("- 相机可行壳外扩 0.5m：由 trajgen CLEARANCE_M 施加（boxes 未预扩，避免双写叠加）\n");
            File.WriteAllText(path, sb.ToString(), new UTF8Encoding(false));
        }
    }
}
