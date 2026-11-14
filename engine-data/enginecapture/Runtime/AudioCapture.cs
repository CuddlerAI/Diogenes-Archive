// NOT yet compiled in a Unity editor (this repo has no Unity toolchain) -
// APIs are written against Unity 2022.3 LTS; first import must follow the
// docs/70 M0 bring-up checklist.

using System;
using System.Collections.Generic;
using System.IO;
using Unity.Collections;
using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// Engine-mix audio capture (docs/70 audio stage). Decided policy: all 4 lanes share
    /// ONE mix - a single audio track is captured per clip and muxed into each video (or
    /// delivered alongside), never per-camera audio.
    ///
    /// Realtime mode: taps the engine mix via OnAudioFilterRead. Attach this component
    /// to the SAME GameObject as the AudioListener so the filter sits at the end of the
    /// listener chain (the callback runs on the AUDIO thread - buffering only, no Unity
    /// API calls there).
    ///
    /// Offline mode: uses AudioRenderer - AudioRenderer.Start() at clip begin, then per
    /// FrameClock tick GetSampleCountForCaptureFrame() + Render() accumulates exactly
    /// the PCM belonging to that capture frame, frame-accurate against captureDeltaTime.
    ///
    /// Output: interleaved float PCM buffered in memory (60 s stereo 48 kHz ~ 23 MB),
    /// written as 16-bit PCM WAV by WriteWav() at clip end.
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class AudioCapture : MonoBehaviour
    {
        [SerializeField] FrameClock _clock;

        readonly object _lock = new object();
        readonly List<float> _samples = new List<float>(1 << 20);
        NativeArray<float> _renderBuffer;
        int _channels = 2;
        bool _capturing;

        public FrameClock Clock { get => _clock; set => _clock = value; }
        public int SampleRate { get; private set; }
        public int Channels => _channels;

        void OnEnable()
        {
            if (_clock != null) _clock.OnTick += HandleTick;
        }

        void OnDisable()
        {
            if (_clock != null) _clock.OnTick -= HandleTick;
            if (_capturing) EndCapture();
        }

        /// <summary>Begin buffering. Call right after FrameClock.BeginClip().</summary>
        public void BeginCapture()
        {
            if (_capturing) throw new InvalidOperationException("AudioCapture already running");
            lock (_lock) _samples.Clear();
            SampleRate = AudioSettings.outputSampleRate;
            _channels = ChannelCount(AudioSettings.speakerMode);
            if (_clock != null && _clock.Mode == FrameClockMode.Offline)
            {
                // Offline: the engine stops driving the DSP clock in realtime; the
                // AudioRenderer hands us exactly the samples for each capture frame.
                AudioRenderer.Start();
            }
            _capturing = true;
        }

        /// <summary>Stop buffering (call before WriteWav / on abort).</summary>
        public void EndCapture()
        {
            if (!_capturing) return;
            _capturing = false;
            if (_clock != null && _clock.Mode == FrameClockMode.Offline)
                AudioRenderer.Stop();
            if (_renderBuffer.IsCreated) _renderBuffer.Dispose();
        }

        void HandleTick(int tick)
        {
            if (!_capturing || _clock.Mode != FrameClockMode.Offline) return;

            int sampleFrames = AudioRenderer.GetSampleCountForCaptureFrame();
            int floatCount = sampleFrames * _channels;
            if (floatCount <= 0) return;

            if (!_renderBuffer.IsCreated || _renderBuffer.Length < floatCount)
            {
                if (_renderBuffer.IsCreated) _renderBuffer.Dispose();
                _renderBuffer = new NativeArray<float>(floatCount, Allocator.Persistent);
            }

            var slice = _renderBuffer.GetSubArray(0, floatCount);
            if (AudioRenderer.Render(slice))
            {
                lock (_lock)
                {
                    for (int i = 0; i < floatCount; i++) _samples.Add(slice[i]);
                }
            }
        }

        // Realtime engine-mix tap; runs on the audio thread.
        void OnAudioFilterRead(float[] data, int channels)
        {
            if (!_capturing || _clock == null || _clock.Mode != FrameClockMode.Realtime) return;
            _channels = channels;
            lock (_lock)
            {
                for (int i = 0; i < data.Length; i++) _samples.Add(data[i]);
            }
        }

        /// <summary>Write buffered PCM as 16-bit WAV. Returns false when empty.</summary>
        public bool WriteWav(string path)
        {
            float[] pcm;
            lock (_lock) pcm = _samples.ToArray();
            if (pcm.Length == 0) return false;

            using (var w = new BinaryWriter(File.Create(path)))
            {
                int byteRate = SampleRate * _channels * 2;
                int dataBytes = pcm.Length * 2;
                w.Write(System.Text.Encoding.ASCII.GetBytes("RIFF"));
                w.Write(36 + dataBytes);
                w.Write(System.Text.Encoding.ASCII.GetBytes("WAVE"));
                w.Write(System.Text.Encoding.ASCII.GetBytes("fmt "));
                w.Write(16);
                w.Write((short)1);            // PCM
                w.Write((short)_channels);
                w.Write(SampleRate);
                w.Write(byteRate);
                w.Write((short)(_channels * 2)); // block align
                w.Write((short)16);           // bits per sample
                w.Write(System.Text.Encoding.ASCII.GetBytes("data"));
                w.Write(dataBytes);
                for (int i = 0; i < pcm.Length; i++)
                {
                    float v = Mathf.Clamp(pcm[i], -1f, 1f);
                    w.Write((short)Mathf.RoundToInt(v * short.MaxValue));
                }
            }
            return true;
        }

        static int ChannelCount(AudioSpeakerMode mode)
        {
            switch (mode)
            {
                case AudioSpeakerMode.Mono: return 1;
                case AudioSpeakerMode.Quad: return 4;
                case AudioSpeakerMode.Surround: return 5;
                case AudioSpeakerMode.Mode5point1: return 6;
                case AudioSpeakerMode.Mode7point1: return 8;
                default: return 2; // Stereo / Prologic
            }
        }
    }
}
