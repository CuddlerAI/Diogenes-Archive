// docs/78 §4.2 步骤 6 的 Megacity 侧数据源：车辆（VehiclePathing）与飞艇
// （BlimpComponent）实体的 LocalToWorld → 世界空间包围球，喂给 SDK 的 DynObjectProbe。
// 包围半径为近似常量（车辆/飞艇模型半径量级），只影响 pixel_share 估计，
// 不影响「视锥内计数 > 0」的活跃帧判定。

using System.Collections.Generic;
using Diogenes.EngineCapture;
using Unity.Collections;
using Unity.Entities;
using Unity.MegacityMetro.Gameplay;
using Unity.MegacityMetro.Traffic;
using Unity.Transforms;

namespace Diogenes.Capture.Megacity
{
    public sealed class EcsDynObjectSource : IDynObjectSource
    {
        /// <summary>Approximate bounding radius of one traffic vehicle (metres). Megacity
        /// vehicles are ~30 units long (Constants.VehicleLength); half-length as sphere.</summary>
        public float VehicleRadius = 15f;

        /// <summary>Approximate bounding radius of one blimp (metres).</summary>
        public float BlimpRadius = 40f;

        readonly World _world;
        EntityQuery _vehicles;
        EntityQuery _blimps;

        public EcsDynObjectSource(World world)
        {
            _world = world;
            var em = world.EntityManager;
            _vehicles = em.CreateEntityQuery(
                ComponentType.ReadOnly<VehiclePathing>(),
                ComponentType.ReadOnly<LocalToWorld>());
            _blimps = em.CreateEntityQuery(
                ComponentType.ReadOnly<BlimpComponent>(),
                ComponentType.ReadOnly<LocalToWorld>());
        }

        public int VehicleCount => _vehicles.CalculateEntityCount();
        public int BlimpCount => _blimps.CalculateEntityCount();

        public void CollectSpheres(List<DynSphere> buffer)
        {
            if (!_world.IsCreated) return;
            Append(_vehicles, VehicleRadius, buffer);
            Append(_blimps, BlimpRadius, buffer);
        }

        static void Append(EntityQuery query, float radius, List<DynSphere> buffer)
        {
            using (var l2w = query.ToComponentDataArray<LocalToWorld>(Allocator.Temp))
            {
                for (int i = 0; i < l2w.Length; i++)
                {
                    var p = l2w[i].Position;
                    buffer.Add(new DynSphere(
                        new UnityEngine.Vector3(p.x, p.y, p.z), radius));
                }
            }
        }
    }
}
