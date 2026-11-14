using System;
using System.Diagnostics;
using System.IO.MemoryMappedFiles;
using System.Threading;
using TaleWorlds.Core;
using TaleWorlds.CampaignSystem;
using TaleWorlds.MountAndBlade;

namespace DataMiner.BannerlordTelemetry
{
    public sealed class SubModule : MBSubModuleBase
    {
        private const int MappingBytes = 80;
        private MemoryMappedFile mapping;
        private MemoryMappedViewAccessor view;
        private long sequence;

        protected override void OnSubModuleLoad()
        {
            base.OnSubModuleLoad();
            mapping = MemoryMappedFile.CreateOrOpen(
                "Local\\DataMinerBannerlordTelemetryV1",
                MappingBytes,
                MemoryMappedFileAccess.ReadWrite);
            view = mapping.CreateViewAccessor(0, MappingBytes, MemoryMappedFileAccess.ReadWrite);
        }

        protected override void OnGameStart(Game game, IGameStarter gameStarter)
        {
            base.OnGameStart(game, gameStarter);
            CampaignGameStarter campaignStarter = gameStarter as CampaignGameStarter;
            if (campaignStarter != null)
                campaignStarter.AddBehavior(new TelemetryBehavior(this));
        }

        private void SamplePlayer()
        {
            Campaign campaign = Campaign.Current;
            if (campaign == null || campaign.MainParty == null || view == null)
                return;

            TaleWorlds.Library.Vec3 position = campaign.MainParty.GetPositionAsVec3();
            TaleWorlds.Library.Vec2 bearing = campaign.MainParty.Bearing;
            double length = Math.Sqrt((double)bearing.x * bearing.x + (double)bearing.y * bearing.y);
            double yaw = length > 1e-8 ? Math.Atan2(bearing.y, bearing.x) : 0.0;
            double half = yaw * 0.5;

            long even = Interlocked.Add(ref sequence, 2);
            view.Write(0, even - 1);
            Thread.MemoryBarrier();
            view.Write(8, 0x31424d44u);
            view.Write(12, 1u);
            view.Write(16, Stopwatch.GetTimestamp());
            view.Write(24, (double)position.x);
            view.Write(32, (double)position.y);
            view.Write(40, (double)position.z);
            view.Write(48, 0.0);
            view.Write(56, 0.0);
            view.Write(64, Math.Sin(half));
            view.Write(72, Math.Cos(half));
            Thread.MemoryBarrier();
            view.Write(0, even);
        }

        protected override void OnSubModuleUnloaded()
        {
            if (view != null) view.Dispose();
            if (mapping != null) mapping.Dispose();
            view = null;
            mapping = null;
            base.OnSubModuleUnloaded();
        }

        private sealed class TelemetryBehavior : CampaignBehaviorBase
        {
            private readonly SubModule owner;

            public TelemetryBehavior(SubModule owner)
            {
                this.owner = owner;
            }

            public override void RegisterEvents()
            {
                CampaignEvents.TickEvent.AddNonSerializedListener(this, OnCampaignTick);
            }

            public override void SyncData(IDataStore dataStore)
            {
            }

            private void OnCampaignTick(float dt)
            {
                owner.SamplePlayer();
            }
        }
    }
}
