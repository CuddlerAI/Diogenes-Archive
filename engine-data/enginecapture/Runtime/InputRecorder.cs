// NOT yet compiled in a Unity editor (this repo has no Unity toolchain) -
// APIs are written against Unity 2022.3 LTS; first import must follow the
// docs/70 M0 bring-up checklist.

using System.Collections.Generic;
using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>One input sample, pulled by PoseRecorder for the cam1 json fields.</summary>
    public struct InputSample
    {
        /// <summary>Cursor position normalized to [0,1] (x/screenW, y/screenH).
        /// Y follows Unity's bottom-left origin; a flip awaits demand-side
        /// confirmation and is recorded in provenance.</summary>
        public double MouseX;
        public double MouseY;

        /// <summary>Raw cursor delta in pixels since the previous tick (provenance keeps
        /// these so any renormalization stays lossless).</summary>
        public double RawDx;
        public double RawDy;

        /// <summary>Delta normalized to [-1,1]. Denominator is the screen WIDTH for BOTH
        /// axes for now (docs/71 interim decision, revisable via RawDx/RawDy).</summary>
        public double MouseDxNormalized;
        public double MouseDyNormalized;

        /// <summary>Held keys as Windows VK codes (see VirtualKeys).</summary>
        public int[] HeldVirtualKeys;
    }

    /// <summary>
    /// Records mouse/keyboard state for the MAIN camera lane only (docs/69: mouse/key
    /// are white fields, cam1 carries them; attach exactly one InputRecorder).
    ///
    /// Backend choice: the LEGACY Input Manager (UnityEngine.Input) is used, guarded by
    /// ENABLE_LEGACY_INPUT_MANAGER. Rationale: zero package dependencies keeps the UPM
    /// package importable everywhere; capture rigs control their own project settings
    /// so enabling "Input Manager (Old)" (or "Both") is a documented prerequisite - see
    /// README. In an Input-System-only project this component compiles but emits a
    /// one-time warning and zero samples instead of failing the whole package compile.
    ///
    /// Known limitation (M0 item): raw deltas come from cursor position differences,
    /// which freeze when the cursor is locked (Cursor.lockState == Locked). FPS-style
    /// captures need the axis-based path (sensitivity-scaled) - tracked in provenance.
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class InputRecorder : MonoBehaviour
    {
        [Tooltip("Emit mouse buttons into keyCode as VK 1/2/4. Off by default: demand-side samples only show keyboard codes.")]
        [SerializeField] bool _includeMouseButtons = false;

        /// <summary>KeyCode -> VK pairs polled every tick (single source: VirtualKeys).</summary>
        static readonly (KeyCode key, int vk)[] PolledKeys =
        {
            (KeyCode.W, VirtualKeys.W), (KeyCode.A, VirtualKeys.A),
            (KeyCode.S, VirtualKeys.S), (KeyCode.D, VirtualKeys.D),
            (KeyCode.Q, VirtualKeys.Q), (KeyCode.E, VirtualKeys.E),
            (KeyCode.R, VirtualKeys.R), (KeyCode.F, VirtualKeys.F),
            (KeyCode.C, VirtualKeys.C), (KeyCode.X, VirtualKeys.X),
            (KeyCode.Z, VirtualKeys.Z),
            (KeyCode.Space, VirtualKeys.Space),
            (KeyCode.LeftShift, VirtualKeys.Shift), (KeyCode.RightShift, VirtualKeys.Shift),
            (KeyCode.LeftControl, VirtualKeys.Control), (KeyCode.RightControl, VirtualKeys.Control),
            (KeyCode.LeftAlt, VirtualKeys.Alt), (KeyCode.RightAlt, VirtualKeys.Alt),
            (KeyCode.Escape, VirtualKeys.Escape), (KeyCode.Tab, VirtualKeys.Tab),
            (KeyCode.Return, VirtualKeys.Enter), (KeyCode.Backspace, VirtualKeys.Backspace),
            (KeyCode.LeftArrow, VirtualKeys.LeftArrow), (KeyCode.UpArrow, VirtualKeys.UpArrow),
            (KeyCode.RightArrow, VirtualKeys.RightArrow), (KeyCode.DownArrow, VirtualKeys.DownArrow)
        };

        readonly List<int> _held = new List<int>(8);
        Vector3 _prevMousePosition;
        int _lastSampledTick = -1;
        InputSample _lastSample;
        bool _warnedNoBackend;

        /// <summary>
        /// Sample input for a tick. Idempotent per tick (PoseRecorder may only call once,
        /// but repeated calls return the cached sample instead of double-diffing).
        /// </summary>
        public InputSample SampleForTick(int tick)
        {
            if (tick == _lastSampledTick) return _lastSample;
            _lastSampledTick = tick;
            _lastSample = Sample(firstTick: tick == 0);
            return _lastSample;
        }

        InputSample Sample(bool firstTick)
        {
#if ENABLE_LEGACY_INPUT_MANAGER
            Vector3 mp = Input.mousePosition;
            double rawDx = firstTick ? 0.0 : mp.x - _prevMousePosition.x;
            double rawDy = firstTick ? 0.0 : mp.y - _prevMousePosition.y;
            _prevMousePosition = mp;

            double w = Screen.width > 0 ? Screen.width : 1;
            double h = Screen.height > 0 ? Screen.height : 1;

            _held.Clear();
            for (int i = 0; i < PolledKeys.Length; i++)
            {
                if (Input.GetKey(PolledKeys[i].key) && !_held.Contains(PolledKeys[i].vk))
                    _held.Add(PolledKeys[i].vk);
            }
            if (_includeMouseButtons)
            {
                if (Input.GetKey(KeyCode.Mouse0)) _held.Add(VirtualKeys.LeftMouseButton);
                if (Input.GetKey(KeyCode.Mouse1)) _held.Add(VirtualKeys.RightMouseButton);
                if (Input.GetKey(KeyCode.Mouse2)) _held.Add(VirtualKeys.MiddleMouseButton);
            }

            return new InputSample
            {
                MouseX = Clamp01(mp.x / w),
                MouseY = Clamp01(mp.y / h),
                RawDx = rawDx,
                RawDy = rawDy,
                // docs/71 interim: both axes normalized by screen WIDTH.
                MouseDxNormalized = Clamp11(rawDx / w),
                MouseDyNormalized = Clamp11(rawDy / w),
                HeldVirtualKeys = _held.ToArray()
            };
#else
            if (!_warnedNoBackend)
            {
                _warnedNoBackend = true;
                Debug.LogWarning(
                    "EngineCapture InputRecorder: legacy Input Manager is disabled " +
                    "(Active Input Handling). Enable 'Input Manager (Old)' or 'Both' " +
                    "for input capture; emitting zero samples.");
            }
            return new InputSample { HeldVirtualKeys = System.Array.Empty<int>() };
#endif
        }

        static double Clamp01(double v) => v < 0.0 ? 0.0 : (v > 1.0 ? 1.0 : v);
        static double Clamp11(double v) => v < -1.0 ? -1.0 : (v > 1.0 ? 1.0 : v);
    }
}
