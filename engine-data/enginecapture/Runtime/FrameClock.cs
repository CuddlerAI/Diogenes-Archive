// NOT yet compiled in a Unity editor (this repo has no Unity toolchain) -
// APIs are written against Unity 2022.3 LTS; first import must follow the
// docs/70 M0 bring-up checklist.

using System;
using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>Clock operating mode (docs/70).</summary>
    public enum FrameClockMode
    {
        /// <summary>Realtime capture: fixedDeltaTime locked to 1/30 + per-frame watchdog.
        /// A frame overrunning its budget aborts the CURRENT CLIP for re-record
        /// (docs/70: a stuttered clip is worthless, re-record instead of patching).</summary>
        Realtime,

        /// <summary>Offline (deterministic) capture: Time.captureDeltaTime = 1/30, so the
        /// engine simulates exactly one capture frame per rendered frame regardless of
        /// wall-clock speed. tick == frame number by construction.</summary>
        Offline
    }

    /// <summary>Reason payload for an aborted clip.</summary>
    public struct ClipAbortReason
    {
        public string Code;    // e.g. "frame_timeout", "readback_backlog", "ffmpeg_exit"
        public string Detail;
        public int Tick;
    }

    /// <summary>
    /// THE authoritative metronome of a capture session (docs/70; docs/71 timing).
    /// All recorders (PoseRecorder, InputRecorder, ReadbackRing, AudioCapture)
    /// subscribe to <see cref="OnTick"/> and must NOT read any other clock source
    /// (Time.time / unscaledTime / realtimeSinceStartup / DateTime for timing) -
    /// enforced socially by review + Editor/PurityCheck scan.
    ///
    /// Ticks are integers starting at 0 per clip; at 30 fps CFR one tick == one
    /// delivered video frame == one action_camera json element.
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class FrameClock : MonoBehaviour
    {
        [SerializeField] FrameClockMode _mode = FrameClockMode.Offline;

        [Tooltip("Realtime watchdog: abort the clip if a rendered frame exceeds this many capture intervals (1 interval = 1/30 s).")]
        [SerializeField] float _watchdogBudgetIntervals = 1.5f;

        /// <summary>Fired once per capture frame while a clip is running. Argument is the
        /// 0-based tick (== frame number in delivery json for MC/SFM-30; SFM-15 exports
        /// even ticks only, handled by DepthExporter/serializer).</summary>
        public event Action<int> OnTick;

        /// <summary>Fired when the running clip must be discarded and re-recorded.</summary>
        public event Action<ClipAbortReason> OnClipAborted;

        public FrameClockMode Mode => _mode;
        public bool IsRunning { get; private set; }
        public int CurrentTick { get; private set; } = -1;

        float _prevFixedDeltaTime;
        float _prevCaptureDeltaTime;
        int _prevTargetFrameRate;
        int _prevVSyncCount;
        float _lastFrameStartRealtime; // watchdog only; NOT a data clock source

        /// <summary>Begin a clip: locks engine time sources and resets the tick counter.</summary>
        public void BeginClip()
        {
            if (IsRunning)
                throw new InvalidOperationException("FrameClock: BeginClip while a clip is running");

            _prevFixedDeltaTime = Time.fixedDeltaTime;
            _prevCaptureDeltaTime = Time.captureDeltaTime;
            _prevTargetFrameRate = Application.targetFrameRate;
            _prevVSyncCount = QualitySettings.vSyncCount;

            // Both modes lock physics to the capture rate (docs/70: single time authority).
            Time.fixedDeltaTime = (float)CaptureFormats.FrameIntervalSeconds;

            if (_mode == FrameClockMode.Offline)
            {
                // Offline: engine time advances exactly 1/30 s per rendered frame.
                // captureDeltaTime is locked to the SAME value as fixedDeltaTime so each
                // capture frame runs exactly one physics step (tick == frame number).
                Time.captureDeltaTime = (float)CaptureFormats.FrameIntervalSeconds;
            }
            else
            {
                // Realtime: pace rendering at 30 fps; vSync off so targetFrameRate rules.
                QualitySettings.vSyncCount = 0;
                Application.targetFrameRate = CaptureFormats.FrameRate;
                _lastFrameStartRealtime = Time.realtimeSinceStartup; // watchdog only
            }

            CurrentTick = -1;
            IsRunning = true;
        }

        /// <summary>End the clip normally and restore engine time settings.</summary>
        public void EndClip()
        {
            if (!IsRunning) return;
            IsRunning = false;
            RestoreEngineTime();
        }

        /// <summary>Abort the clip (recorders listen to OnClipAborted and discard output).</summary>
        public void AbortClip(string code, string detail)
        {
            if (!IsRunning) return;
            IsRunning = false;
            RestoreEngineTime();
            var reason = new ClipAbortReason { Code = code, Detail = detail, Tick = CurrentTick };
            OnClipAborted?.Invoke(reason);
        }

        void Update()
        {
            if (!IsRunning) return;

            if (_mode == FrameClockMode.Realtime)
            {
                // Watchdog: realtimeSinceStartup is used ONLY to measure wall-clock frame
                // duration for the abort decision - it never enters recorded data.
                float now = Time.realtimeSinceStartup;
                float elapsed = now - _lastFrameStartRealtime;
                _lastFrameStartRealtime = now;
                float budget = _watchdogBudgetIntervals * (float)CaptureFormats.FrameIntervalSeconds;
                if (CurrentTick >= 0 && elapsed > budget)
                {
                    AbortClip("frame_timeout",
                        "frame " + CurrentTick + " took " + (elapsed * 1000f).ToString("F1") +
                        " ms (budget " + (budget * 1000f).ToString("F1") + " ms)");
                    return;
                }
            }

            CurrentTick++;
            OnTick?.Invoke(CurrentTick);
        }

        void OnDisable()
        {
            if (IsRunning)
                AbortClip("clock_disabled", "FrameClock component disabled mid-clip");
        }

        void RestoreEngineTime()
        {
            Time.fixedDeltaTime = _prevFixedDeltaTime;
            Time.captureDeltaTime = _prevCaptureDeltaTime;
            Application.targetFrameRate = _prevTargetFrameRate;
            QualitySettings.vSyncCount = _prevVSyncCount;
        }
    }
}
