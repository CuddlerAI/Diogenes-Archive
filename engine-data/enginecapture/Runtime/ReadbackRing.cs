// NOT yet compiled in a Unity editor (this repo has no Unity toolchain) -
// APIs are written against Unity 2022.3 LTS; first import must follow the
// docs/70 M0 bring-up checklist.

using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Threading;
using Unity.Collections;
using UnityEngine;
using UnityEngine.Rendering;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// GPU -> CPU frame transport (docs/70 readback stage; docs/71 four-lane variant).
    ///
    /// Per lane and per tick an AsyncGPUReadback request is issued after that lane's
    /// camera renders. Its completion callback copies the one-frame-lifetime NativeArray
    /// immediately into a pooled byte buffer; completed buffers are then reordered by
    /// tick and handed to ONE dedicated consumer thread. The Unity main thread never
    /// blocks on video encoding.
    ///
    /// Watchdog integration (docs/70): if a lane's issued-but-not-ordered queue exceeds
    /// MaxPendingReadbacks the GPU cannot keep up and the clip is aborted via
    /// FrameClock.AbortClip("readback_backlog", ...) for re-record.
    ///
    /// Per-lane sinks (docs/83 debate 17): colour lanes read back RGBA32 every tick into
    /// an ffmpeg pipe; depth lanes read back RFloat on EVEN ticks only and land as EXR.
    /// Both share this one ring so the readback TIMING is defined in exactly one place.
    ///
    /// Offline mode: WaitAllRequests() force-completes all pending readbacks and drains
    /// the writer queue - the deterministic path never drops a frame.
    ///
    /// Row order note: readback row origin differs per graphics API (e.g. D3D11 vs GL);
    /// vertical flip is handled in the ffmpeg filter graph (FfmpegPipe.VideoFilter),
    /// verified once per platform during M0 bring-up.
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class ReadbackRing : MonoBehaviour
    {
        [SerializeField] FrameClock _clock;
        [SerializeField] CaptureRigBase _rig;

        [Tooltip("Abort threshold for per-lane pending GPU readbacks (realtime watchdog).")]
        [SerializeField] int _maxPendingReadbacks = 8;

        sealed class Lane
        {
            public IFrameSink Sink;
            public Stack<byte[]> BufferPool = new Stack<byte[]>(); // double buffer: 2 entries
            public Queue<int> IssuedTicks = new Queue<int>();
            public Dictionary<int, byte[]> Completed = new Dictionary<int, byte[]>();
            public readonly object Sync = new object();
            public int FrameBytes;
        }

        sealed class ReadbackFailure
        {
            public string Detail;
        }

        Lane[] _lanes;                         // 惰性：长度取自 rig，构造期 rig 还没绑定

        // Tick whose rendered output still needs a readback request. Requests are issued
        // from endCameraRendering for the matching lane, never from a generic context
        // callback: a game may render several contexts in one frame, and the first
        // context is not guaranteed to contain our capture cameras.
        int _tickToCapture = -1;
        bool[] _laneIssuedForTick;
        volatile int _generation;
        readonly ConcurrentQueue<(Lane lane, int tick, byte[] buffer)> _writeQueue =
            new ConcurrentQueue<(Lane, int, byte[])>();
        readonly ConcurrentQueue<ReadbackFailure> _readbackFailures =
            new ConcurrentQueue<ReadbackFailure>();
        readonly AutoResetEvent _writeSignal = new AutoResetEvent(false);
        readonly ManualResetEventSlim _writerIdle = new ManualResetEventSlim(true);

        Thread _writerThread;
        volatile bool _running;

        public FrameClock Clock { get => _clock; set => _clock = value; }
        public CaptureRigBase Rig { get => _rig; set => _rig = value; }

        /// <summary>Wire one sink per lane before BeginClip.</summary>
        public void AttachSinks(IFrameSink[] sinks)
        {
            if (sinks == null || sinks.Length != _rig.Count)
                throw new ArgumentException("Expected " + _rig.Count + " sinks", nameof(sinks));
            // Invalidates callbacks left over from an aborted previous clip.
            _generation++;
            _tickToCapture = -1;
            while (_readbackFailures.TryDequeue(out _)) { }
            _lanes = new Lane[_rig.Count];
            _laneIssuedForTick = new bool[_rig.Count];
            for (int i = 0; i < _rig.Count; i++)
            {
                _lanes[i] = new Lane { Sink = sinks[i], FrameBytes = sinks[i].FrameBytes };
                _lanes[i].BufferPool.Push(new byte[_lanes[i].FrameBytes]);
                _lanes[i].BufferPool.Push(new byte[_lanes[i].FrameBytes]); // double buffer
            }
        }

        /// <summary>Convenience for colour-only rigs: wraps each pipe in a VideoSink.</summary>
        public void AttachPipes(FfmpegPipe[] pipes)
        {
            if (pipes == null || pipes.Length != _rig.Count)
                throw new ArgumentException("Expected " + _rig.Count + " pipes", nameof(pipes));
            var sinks = new IFrameSink[pipes.Length];
            for (int i = 0; i < pipes.Length; i++)
            {
                var size = _rig.GetLane(i).Size;
                sinks[i] = new VideoSink(pipes[i], size.Width * size.Height * 4);
            }
            AttachSinks(sinks);
        }

        void OnEnable()
        {
            if (_clock != null) _clock.OnTick += HandleTick;
            RenderPipelineManager.endCameraRendering += HandleEndCameraRendering;
            _generation++;
            _running = true;
            _writerThread = new Thread(WriterLoop) { Name = "EngineCapture.Readback", IsBackground = true };
            _writerThread.Start();
        }

        void OnDisable()
        {
            if (_clock != null) _clock.OnTick -= HandleTick;
            RenderPipelineManager.endCameraRendering -= HandleEndCameraRendering;
            _tickToCapture = -1;
            _generation++;
            _running = false;
            _writeSignal.Set();
            _writerThread?.Join(2000);
            _writerThread = null;
        }

        void HandleTick(int tick)
        {
            if (_rig == null || !_rig.IsInitialized || _lanes == null || _lanes[0] == null) return;

            if (_tickToCapture >= 0)
            {
                _clock.AbortClip("camera_not_rendered",
                    "capture cameras did not all render for tick " + _tickToCapture);
                return;
            }

            _tickToCapture = tick;
            for (int i = 0; i < _rig.Count; i++)
                _laneIssuedForTick[i] = !_lanes[i].Sink.WantsTick(tick);
            if (AllLanesIssued())
                _tickToCapture = -1;
        }

        void HandleEndCameraRendering(ScriptableRenderContext context, Camera camera)
        {
            if (_tickToCapture < 0 || camera == null) return;
            int tick = _tickToCapture;
            for (int i = 0; i < _rig.Count; i++)
            {
                if (_laneIssuedForTick[i] || _rig.GetLane(i).Camera != camera)
                    continue;

                _laneIssuedForTick[i] = true;
                IssueRequest(_lanes[i], i, tick);
                if (AllLanesIssued())
                    _tickToCapture = -1;
                return;
            }
        }

        void LateUpdate()
        {
            if (_readbackFailures.TryDequeue(out var failure) && _clock != null && _clock.IsRunning)
            {
                _clock.AbortClip("readback_error", failure.Detail);
            }
        }

        /// <summary>Offline-mode barrier: force completion of every pending readback and
        /// wait until the writer thread has flushed all frames to the pipes.</summary>
        public void WaitAllRequests()
        {
            AsyncGPUReadback.WaitAllRequests();
            while (!_writeQueue.IsEmpty) Thread.Sleep(1);
            _writerIdle.Wait(5000);
        }

        void IssueRequest(Lane lane, int laneIndex, int tick)
        {
            int backlog;
            lock (lane.Sync)
            {
                lane.IssuedTicks.Enqueue(tick);
                backlog = lane.IssuedTicks.Count;
            }

            CameraLane rigLane = _rig.GetLane(laneIndex);
            RenderTexture target = rigLane.TargetTexture;
            int generation = _generation;
            string resource = target == null
                ? "target=null"
                : "target=" + target.width + "x" + target.height
                    + ",format=" + target.graphicsFormat
                    + ",created=" + target.IsCreated();

            if (target == null || !target.IsCreated())
            {
                EnqueueReadbackFailure(
                    "lane " + (laneIndex + 1) + " tick " + tick
                    + " cannot issue readback; " + resource);
                return;
            }

            try
            {
                AsyncGPUReadback.Request(
                    target,
                    0,
                    lane.Sink.ReadbackFormat,
                    request => HandleReadbackCompleted(
                        lane,
                        laneIndex,
                        tick,
                        generation,
                        resource,
                        request));
            }
            catch (Exception exception)
            {
                EnqueueReadbackFailure(
                    "lane " + (laneIndex + 1) + " tick " + tick
                    + " request failed: " + exception.GetType().Name
                    + ": " + exception.Message + "; " + resource);
                return;
            }

            if (backlog > _maxPendingReadbacks && _clock != null && _clock.IsRunning)
            {
                _clock.AbortClip("readback_backlog",
                    "lane " + (laneIndex + 1) + " has " + backlog
                    + " issued but not yet ordered readbacks");
            }
        }

        void HandleReadbackCompleted(
            Lane lane,
            int laneIndex,
            int tick,
            int generation,
            string resource,
            AsyncGPUReadbackRequest request)
        {
            if (!_running || generation != _generation)
                return;

            if (request.hasError)
            {
                EnqueueReadbackFailure(
                    "lane " + (laneIndex + 1) + " tick " + tick
                    + " callback returned hasError; " + resource);
                return;
            }

            byte[] buffer = null;
            try
            {
                NativeArray<byte> data = request.GetData<byte>();
                if (data.Length != lane.FrameBytes)
                {
                    EnqueueReadbackFailure(
                        "lane " + (laneIndex + 1) + " tick " + tick
                        + " returned " + data.Length + " bytes; expected "
                        + lane.FrameBytes + "; " + resource);
                    return;
                }

                buffer = RentBuffer(lane);
                // Callback consumption is deliberate: Unity keeps completed request
                // data valid for one frame only. A FIFO poll can let a later request
                // expire behind an older one and then misreport hasError.
                NativeArray<byte>.Copy(data, buffer, data.Length);
            }
            catch (Exception exception)
            {
                if (buffer != null)
                    ReturnBuffer(lane, buffer);
                EnqueueReadbackFailure(
                    "lane " + (laneIndex + 1) + " tick " + tick
                    + " callback copy failed: " + exception.GetType().Name
                    + ": " + exception.Message + "; " + resource);
                return;
            }

            var ready = new List<(int tick, byte[] buffer)>();
            lock (lane.Sync)
            {
                lane.Completed[tick] = buffer;
                while (lane.IssuedTicks.Count > 0
                    && lane.Completed.TryGetValue(lane.IssuedTicks.Peek(), out var next))
                {
                    int nextTick = lane.IssuedTicks.Dequeue();
                    lane.Completed.Remove(nextTick);
                    ready.Add((nextTick, next));
                }
            }

            for (int i = 0; i < ready.Count; i++)
            {
                _writerIdle.Reset();
                _writeQueue.Enqueue((lane, ready[i].tick, ready[i].buffer));
                _writeSignal.Set();
            }
        }

        void EnqueueReadbackFailure(string detail)
        {
            _readbackFailures.Enqueue(new ReadbackFailure
            {
                Detail = detail
            });
        }

        bool AllLanesIssued()
        {
            if (_laneIssuedForTick == null) return false;
            for (int i = 0; i < _laneIssuedForTick.Length; i++)
                if (!_laneIssuedForTick[i]) return false;
            return true;
        }

        void ReturnBuffer(Lane lane, byte[] buffer)
        {
            if (buffer == null) return;
            lock (lane.BufferPool)
            {
                if (lane.BufferPool.Count < 2)
                    lane.BufferPool.Push(buffer);
            }
        }

        byte[] RentBuffer(Lane lane)
        {
            lock (lane.BufferPool)
            {
                if (lane.BufferPool.Count > 0) return lane.BufferPool.Pop();
            }
            // Both buffers in flight: allocation here is the back-pressure signal; the
            // pending-queue watchdog is the hard stop.
            return new byte[lane.FrameBytes];
        }

        void WriterLoop()
        {
            while (_running || !_writeQueue.IsEmpty)
            {
                if (_writeQueue.TryDequeue(out var item))
                {
                    try
                    {
                        item.lane.Sink?.Write(item.tick, item.buffer, item.buffer.Length);
                    }
                    catch (Exception)
                    {
                        // Sink failure surfaces via IFrameSink.FailReason on the main
                        // thread; the writer thread must never crash the process.
                    }
                    lock (item.lane.BufferPool)
                    {
                        if (item.lane.BufferPool.Count < 2)
                            item.lane.BufferPool.Push(item.buffer);
                    }
                    if (_writeQueue.IsEmpty) _writerIdle.Set();
                }
                else
                {
                    _writerIdle.Set();
                    _writeSignal.WaitOne(10);
                }
            }
            _writerIdle.Set();
        }
    }
}
