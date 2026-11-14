// 通用场景占据图导出（docs/71 §7 防线 1：轨迹只在自由壳内生成，穿模零容忍由构造保证）。
//
// 与 Megacity 版 SceneProbe 的分工：那边绑 ECS + 走廊聚类（车流路网是它的锚点先验）；
// 本导出器面向**普通 GameObject 场景**，不假设场景语义。产物即 trajgen
// `Scene.from_dict` 口径的 scene.json。
//
// **不用 Collider 建图**（这是本导出器的关键决定）：实测 Factory Training 有 12711 个
// Renderer 却只有 98 个静态 Collider —— 正是 docs/71 R11「collider 与可视网格不一致」
// 的实例，按 collider 建图会得到一张 0.2% 占据率的空图，穿模断言全部失真。
// 改用两个更可靠的真值源：
//   ① `Renderer.bounds` 体素化障碍 —— 可见即障碍，AABB 近似偏保守，对防穿模是安全方向；
//   ② 场景 NavMesh 三角化 —— 可行走面的权威真值，给地面高度与可站立 XZ 掩码。
// 自由空间 = 可行走 XZ 之上、非障碍的体素。
//
// 用法（batchmode）：
//   -executeMethod Diogenes.Capture.Factory.Editor.SceneOccupancyExporter.Run
//   env: DIOGENES_SCENE_PATH  要导出的场景
//        DIOGENES_OCC_OUT     输出 json 路径
//        DIOGENES_OCC_VOXEL   体素边长（默认 0.5m）
//        DIOGENES_OCC_CENTER  采集体积中心 "x,z"（默认取可行走面最开阔处）
//        DIOGENES_OCC_EXTENT  采集体积半宽 "x,z"（默认 30,30）
//        DIOGENES_OCC_HEIGHT  采集体积高度（默认 6m，从地面起算）

using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Text;
using UnityEditor;
using UnityEngine;
using UnityEngine.AI;

namespace Diogenes.Capture.Factory.Editor
{
    public static class SceneOccupancyExporter
    {
        static string Env(string k, string d)
        {
            var v = Environment.GetEnvironmentVariable(k);
            return string.IsNullOrEmpty(v) ? d : v;
        }

        public static void Run()
        {
            string scenePath = Env("DIOGENES_SCENE_PATH", "");
            string outPath = Env("DIOGENES_OCC_OUT", "/tmp/diogenes-scene-occ.json");
            float voxel = float.Parse(Env("DIOGENES_OCC_VOXEL", "0.5"), CultureInfo.InvariantCulture);
            float height = float.Parse(Env("DIOGENES_OCC_HEIGHT", "6"), CultureInfo.InvariantCulture);
            var extent = ParseVec2(Env("DIOGENES_OCC_EXTENT", "30,30"));

            if (string.IsNullOrEmpty(scenePath))
            {
                Debug.LogError("[occ] DIOGENES_SCENE_PATH required");
                EditorApplication.Exit(2);
                return;
            }

            var scene = UnityEditor.SceneManagement.EditorSceneManager.OpenScene(
                scenePath, UnityEditor.SceneManagement.OpenSceneMode.Single);
            Debug.Log("[occ] scene opened: " + scenePath);

            // ---- ① 可行走面（NavMesh 三角化）----
            var tri = NavMesh.CalculateTriangulation();
            Debug.Log("[occ] navmesh verts=" + tri.vertices.Length
                + " tris=" + tri.indices.Length / 3);
            if (tri.indices.Length < 3)
            {
                Debug.LogError("[occ] scene has no NavMesh — bake one, or supply "
                    + "DIOGENES_OCC_CENTER and accept renderer-only occupancy");
            }

            // ---- ② 障碍（可见 Renderer 的 bounds；排除大地板本身与超大天花板）----
            var renderers = new List<Renderer>();
            var sceneBounds = new Bounds();
            bool first = true;
            foreach (var root in scene.GetRootGameObjects())
            {
                if (!root.activeInHierarchy) continue;
                foreach (var r in root.GetComponentsInChildren<Renderer>(false))
                {
                    if (!(r is MeshRenderer) && !(r is SkinnedMeshRenderer)) continue;
                    renderers.Add(r);
                    if (first) { sceneBounds = r.bounds; first = false; }
                    else sceneBounds.Encapsulate(r.bounds);
                }
            }
            Debug.Log("[occ] renderers=" + renderers.Count
                + " sceneBounds=" + sceneBounds.center.ToString("F1")
                + " size=" + sceneBounds.size.ToString("F1"));

            // 地面高度：NavMesh 顶点的中位高度（可行走面的权威值）
            float groundY;
            if (tri.vertices.Length > 0)
            {
                var ys = new List<float>(tri.vertices.Length);
                foreach (var v in tri.vertices) ys.Add(v.y);
                ys.Sort();
                groundY = ys[ys.Count / 2];
            }
            else groundY = sceneBounds.min.y;
            Debug.Log("[occ] ground_y=" + groundY.ToString("F2"));

            // 可行走 XZ 掩码（NavMesh 三角形栅格化到 voxel 分辨率）
            var walkable = new HashSet<long>();
            for (int i = 0; i + 2 < tri.indices.Length; i += 3)
            {
                var a = tri.vertices[tri.indices[i]];
                var b = tri.vertices[tri.indices[i + 1]];
                var c = tri.vertices[tri.indices[i + 2]];
                // 三角形重心细分采样（够密即可，NavMesh 三角一般 <5m）
                const int N = 6;
                for (int u = 0; u <= N; u++)
                    for (int w = 0; u + w <= N; w++)
                    {
                        float fu = u / (float)N, fw = w / (float)N;
                        var p = a + (b - a) * fu + (c - a) * fw;
                        walkable.Add(CellKey(p.x, p.z, voxel));
                    }
            }
            Debug.Log("[occ] walkable cells=" + walkable.Count
                + " (≈" + (walkable.Count * voxel * voxel).ToString("F0") + " m²)");

            // ---- 采集体积中心：可行走面最开阔处（或显式给）----
            Vector2 center;
            string centerEnv = Env("DIOGENES_OCC_CENTER", "");
            if (!string.IsNullOrEmpty(centerEnv)) center = ParseVec2(centerEnv);
            else center = PickOpenCenter(walkable, voxel, extent);
            Debug.Log("[occ] volume center=(" + center.x.ToString("F1") + ", "
                + center.y.ToString("F1") + ")");

            var lo = new Vector3(center.x - extent.x, groundY, center.y - extent.y);
            var hi = new Vector3(center.x + extent.x, groundY + height, center.y + extent.y);

            int nx = Mathf.Max(1, Mathf.CeilToInt((hi.x - lo.x) / voxel));
            int ny = Mathf.Max(1, Mathf.CeilToInt((hi.y - lo.y) / voxel));
            int nz = Mathf.Max(1, Mathf.CeilToInt((hi.z - lo.z) / voxel));
            var occ = new bool[nx, ny, nz];

            // 障碍体素：renderer bounds 覆盖（地板/天花板类超大扁平体不算障碍——
            // 地面由 ground_y 表达，天花板由体积高度截断）
            int marked = 0;
            var groundBoxes = new List<Bounds>();
            foreach (var r in renderers)
            {
                var b = r.bounds;
                // 可站面：扁平且水平铺开的体（地坪、月台、平台、台阶顶）。
                // 它们不是障碍，而是**站得住的地方**——NPC 落地要用它。
                // 只靠 NavMesh 不行：场景 NavMesh 常只烘焙玩法区（本场景 1024/2500 m²），
                // 覆盖不到的地方 agent 悬在空中（目检实测「站在护栏/窗台高度」）。
                bool floorLike = b.size.y < 0.6f && b.size.x * b.size.z > 12f;
                if (floorLike)
                {
                    if (b.max.y >= groundY - 0.5f && b.max.y <= hi.y
                        && b.max.x >= lo.x && b.min.x <= hi.x
                        && b.max.z >= lo.z && b.min.z <= hi.z)
                        groundBoxes.Add(b);
                    continue;
                }
                if (b.max.y < groundY + 0.1f) continue;              // 地面以下
                if (b.min.y > hi.y) continue;                        // 体积以上（天花板/桁架）
                int ix0 = Mathf.Clamp(Mathf.FloorToInt((b.min.x - lo.x) / voxel), 0, nx - 1);
                int ix1 = Mathf.Clamp(Mathf.CeilToInt((b.max.x - lo.x) / voxel), 0, nx - 1);
                int iy0 = Mathf.Clamp(Mathf.FloorToInt((b.min.y - lo.y) / voxel), 0, ny - 1);
                int iy1 = Mathf.Clamp(Mathf.CeilToInt((b.max.y - lo.y) / voxel), 0, ny - 1);
                int iz0 = Mathf.Clamp(Mathf.FloorToInt((b.min.z - lo.z) / voxel), 0, nz - 1);
                int iz1 = Mathf.Clamp(Mathf.CeilToInt((b.max.z - lo.z) / voxel), 0, nz - 1);
                if (b.max.x < lo.x || b.min.x > hi.x || b.max.z < lo.z || b.min.z > hi.z) continue;
                for (int ix = ix0; ix <= ix1; ix++)
                    for (int iy = iy0; iy <= iy1; iy++)
                        for (int iz = iz0; iz <= iz1; iz++)
                            if (!occ[ix, iy, iz]) { occ[ix, iy, iz] = true; marked++; }
            }

            // NavMesh 只用于定地面与推荐中心，**不用于封列**：不可行走 ≠ 有障碍
            // （NavMesh 常只烘焙玩法区域；首版把未覆盖列整列封死，自由空间从 2500 m²
            // 塌到 48 m²，trajgen 直接「找不到可行的四路共线起点」）。
            // 真障碍由可见几何（renderer bounds）判定 —— 那才是「不穿模」的准确定义。
            int sealedCols = 0;
            if (Env("DIOGENES_OCC_SEAL_NONWALKABLE", "0") == "1" && walkable.Count > 0)
                for (int ix = 0; ix < nx; ix++)
                    for (int iz = 0; iz < nz; iz++)
                    {
                        float wx = lo.x + (ix + 0.5f) * voxel;
                        float wz = lo.z + (iz + 0.5f) * voxel;
                        if (walkable.Contains(CellKey(wx, wz, voxel))) continue;
                        sealedCols++;
                        for (int iy = 0; iy < ny; iy++)
                            if (!occ[ix, iy, iz]) { occ[ix, iy, iz] = true; marked++; }
                    }

            Debug.Log("[occ] voxels " + nx + "x" + ny + "x" + nz + "=" + (nx * ny * nz)
                + " occupied " + marked + " (" + (100.0 * marked / (nx * ny * nz)).ToString("F1")
                + "%), sealed non-walkable columns " + sealedCols);

            var boxes = MergeBoxes(occ, nx, ny, nz, lo, voxel);
            Debug.Log("[occ] merged boxes: " + boxes.Count);

            var origin = new Vector3(center.x, groundY, center.y);
            var sb = new StringBuilder();
            sb.Append("{\n");
            sb.Append("  \"name\": \"").Append(Path.GetFileNameWithoutExtension(scenePath)).Append("\",\n");
            sb.Append("  \"kind\": \"indoor_scan\",\n");
            sb.Append("  \"lo\": ").Append(V(lo - origin)).Append(",\n");
            sb.Append("  \"hi\": ").Append(V(hi - origin)).Append(",\n");
            sb.Append("  \"ground_y\": 0.0,\n");
            sb.Append("  \"eye_band\": [1.4, 2.2],\n");
            sb.Append("  \"boxes\": [\n");
            for (int i = 0; i < boxes.Count; i++)
            {
                sb.Append("    {\"lo\": ").Append(V(boxes[i].min - origin))
                  .Append(", \"hi\": ").Append(V(boxes[i].max - origin))
                  .Append(i == boxes.Count - 1 ? "}\n" : "},\n");
            }
            sb.Append("  ],\n");
            sb.Append("  \"ground_boxes\": [\n");
            for (int i = 0; i < groundBoxes.Count; i++)
            {
                sb.Append("    {\"lo\": ").Append(V(groundBoxes[i].min - origin))
                  .Append(", \"hi\": ").Append(V(groundBoxes[i].max - origin))
                  .Append(i == groundBoxes.Count - 1 ? "}\n" : "},\n");
            }
            sb.Append("  ],\n");
            sb.Append("  \"world_offset\": [").Append(F(origin.x)).Append(", ")
              .Append(F(origin.y)).Append(", ").Append(F(origin.z)).Append("],\n");
            sb.Append("  \"placement\": {\"yaw_deg\": 0, \"snap_y\": false, ")
              .Append("\"note\": \"scene-file 恒等摆位：局部坐标 + world_offset 即世界坐标\"},\n");
            sb.Append("  \"export\": {\"generator\": \"SceneOccupancyExporter (renderer-bounds + navmesh)\"")
              .Append(", \"scene\": \"").Append(scenePath).Append("\"")
              .Append(", \"voxel_m\": ").Append(F(voxel))
              .Append(", \"dims\": [").Append(nx).Append(", ").Append(ny).Append(", ").Append(nz).Append("]")
              .Append(", \"occupied_voxels\": ").Append(marked)
              .Append(", \"renderers\": ").Append(renderers.Count)
              .Append(", \"navmesh_tris\": ").Append(tri.indices.Length / 3)
              .Append(", \"walkable_cells\": ").Append(walkable.Count)
              .Append(", \"ground_boxes\": ").Append(groundBoxes.Count).Append("}\n");
            sb.Append("}\n");

            File.WriteAllText(outPath, sb.ToString(), new UTF8Encoding(false));
            Debug.Log("[occ] written " + outPath + " (" + boxes.Count + " boxes)");
            EditorApplication.Exit(0);
        }

        static long CellKey(float x, float z, float voxel)
        {
            long cx = (long)Mathf.Floor(x / voxel);
            long cz = (long)Mathf.Floor(z / voxel);
            return (cx << 32) ^ (cz & 0xffffffffL);
        }

        /// <summary>可行走格最密处作为采集体积中心（16 方向 8m 探针全通的格优先）。</summary>
        static Vector2 PickOpenCenter(HashSet<long> walkable, float voxel, Vector2 extent)
        {
            if (walkable.Count == 0) return Vector2.zero;
            float minX = float.MaxValue, maxX = float.MinValue, minZ = float.MaxValue, maxZ = float.MinValue;
            foreach (var k in walkable)
            {
                float x = (int)(k >> 32) * voxel, z = (int)(k & 0xffffffffL) * voxel;
                minX = Mathf.Min(minX, x); maxX = Mathf.Max(maxX, x);
                minZ = Mathf.Min(minZ, z); maxZ = Mathf.Max(maxZ, z);
            }
            Vector2 best = new Vector2((minX + maxX) * 0.5f, (minZ + maxZ) * 0.5f);
            int bestScore = -1;
            for (float x = minX; x <= maxX; x += 2f)
                for (float z = minZ; z <= maxZ; z += 2f)
                {
                    if (!walkable.Contains(CellKey(x, z, voxel))) continue;
                    int score = 0;
                    for (int k = 0; k < 16; k++)
                    {
                        float a = k * Mathf.PI * 2f / 16f;
                        for (float d = 2f; d <= 10f; d += 2f)
                            if (walkable.Contains(CellKey(x + Mathf.Cos(a) * d,
                                                          z + Mathf.Sin(a) * d, voxel))) score++;
                    }
                    if (score > bestScore) { bestScore = score; best = new Vector2(x, z); }
                }
            Debug.Log("[occ] auto center walkable-openness " + bestScore + "/80");
            return best;
        }

        static List<Bounds> MergeBoxes(bool[,,] occ, int nx, int ny, int nz, Vector3 lo, float v)
        {
            var used = new bool[nx, ny, nz];
            var result = new List<Bounds>();
            for (int iy = 0; iy < ny; iy++)
                for (int iz = 0; iz < nz; iz++)
                    for (int ix = 0; ix < nx; ix++)
                    {
                        if (!occ[ix, iy, iz] || used[ix, iy, iz]) continue;
                        int x1 = ix;
                        while (x1 + 1 < nx && occ[x1 + 1, iy, iz] && !used[x1 + 1, iy, iz]) x1++;
                        int z1 = iz;
                        bool grow = true;
                        while (grow && z1 + 1 < nz)
                        {
                            for (int x = ix; x <= x1; x++)
                                if (!occ[x, iy, z1 + 1] || used[x, iy, z1 + 1]) { grow = false; break; }
                            if (grow) z1++;
                        }
                        int y1 = iy;
                        grow = true;
                        while (grow && y1 + 1 < ny)
                        {
                            for (int x = ix; x <= x1 && grow; x++)
                                for (int z = iz; z <= z1; z++)
                                    if (!occ[x, y1 + 1, z] || used[x, y1 + 1, z]) { grow = false; break; }
                            if (grow) y1++;
                        }
                        for (int x = ix; x <= x1; x++)
                            for (int y = iy; y <= y1; y++)
                                for (int z = iz; z <= z1; z++)
                                    used[x, y, z] = true;

                        var b = new Bounds();
                        b.SetMinMax(
                            new Vector3(lo.x + ix * v, lo.y + iy * v, lo.z + iz * v),
                            new Vector3(lo.x + (x1 + 1) * v, lo.y + (y1 + 1) * v, lo.z + (z1 + 1) * v));
                        result.Add(b);
                    }
            return result;
        }

        static Vector2 ParseVec2(string s)
        {
            var p = s.Split(',');
            return new Vector2(
                float.Parse(p[0], CultureInfo.InvariantCulture),
                float.Parse(p[1], CultureInfo.InvariantCulture));
        }

        static string F(float f) => f.ToString("F3", CultureInfo.InvariantCulture);
        static string V(Vector3 v) => "[" + F(v.x) + ", " + F(v.y) + ", " + F(v.z) + "]";
    }
}
